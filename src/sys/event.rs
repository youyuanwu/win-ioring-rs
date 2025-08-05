use futures::FutureExt;
use futures::channel::oneshot;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWait,
    WT_EXECUTEONLYONCE, WaitForSingleObject,
};

/// An async wrapper around Windows Event objects using RegisterWaitForSingleObject.
///
/// This provides efficient async waiting for Windows Events without blocking threads.
/// Events can be signaled, reset, and reused multiple times.
pub struct AsyncEvent {
    handle: HANDLE,
}

impl AsyncEvent {
    /// Creates a new auto-reset event in the non-signaled state.
    /// Auto-reset events automatically return to non-signaled state after one waiter is released.
    pub fn new() -> windows::core::Result<Self> {
        let handle = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self { handle })
    }

    /// Creates a new manual-reset event in the non-signaled state.
    /// Manual-reset events remain signaled until explicitly reset, allowing multiple waiters to be released.
    pub fn new_manual_reset() -> windows::core::Result<Self> {
        let handle = unsafe { CreateEventW(None, true, false, None)? };
        Ok(Self { handle })
    }

    /// Signals the event, allowing waiting tasks to complete.
    pub fn signal(&self) -> windows::core::Result<()> {
        unsafe { SetEvent(self.handle) }
    }

    /// Resets the event to the non-signaled state, allowing it to be reused.
    /// After calling reset(), new calls to wait() will block until signal() is called again.
    pub fn reset(&self) -> windows::core::Result<()> {
        unsafe { ResetEvent(self.handle) }
    }

    /// Returns a future that will complete when the event is signaled.
    /// Multiple waiters can wait on the same event simultaneously.
    pub fn wait(&self) -> EventWaitFuture<'_> {
        EventWaitFuture::new(self)
    }

    /// Synchronously waits for the event to be signaled.
    /// This will block the current thread until the event is signaled.
    ///
    /// # Arguments
    /// * `timeout_ms` - Optional timeout in milliseconds. Use `None` for infinite wait.
    ///
    /// # Returns
    /// * `Ok(())` if the event was signaled
    /// * `Err(windows::core::Error)` if the wait failed or timed out
    pub fn wait_sync(&self, timeout_ms: Option<u32>) -> windows::core::Result<()> {
        let timeout = timeout_ms.unwrap_or(INFINITE);
        unsafe {
            match WaitForSingleObject(self.handle, timeout) {
                WAIT_OBJECT_0 => Ok(()),
                WAIT_TIMEOUT => Err(windows::core::Error::from_win32()),
                _ => Err(windows::core::Error::from_win32()),
            }
        }
    }

    /// Synchronously waits for the event to be signaled with infinite timeout.
    /// This will block the current thread until the event is signaled.
    pub fn wait_sync_infinite(&self) -> windows::core::Result<()> {
        self.wait_sync(None)
    }

    pub fn handle(&self) -> HANDLE {
        self.handle
    }
}

pub struct EventWaitFuture<'a> {
    event: &'a AsyncEvent,
    state: EventWaitState,
}

enum EventWaitState {
    NotStarted,
    Waiting {
        wait_handle: HANDLE,
        receiver: oneshot::Receiver<()>,
    },
    Completed,
}

impl<'a> EventWaitFuture<'a> {
    fn new(event: &'a AsyncEvent) -> Self {
        Self {
            event,
            state: EventWaitState::NotStarted,
        }
    }
}

impl<'a> Future for EventWaitFuture<'a> {
    type Output = windows::core::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match &mut self.state {
            EventWaitState::NotStarted => {
                let (sender, receiver) = oneshot::channel();

                // Box the sender so we can pass it to the callback
                let sender_ptr = Box::into_raw(Box::new(sender));

                // Register the wait callback
                let mut wait_handle = HANDLE::default();
                let result = unsafe {
                    RegisterWaitForSingleObject(
                        &mut wait_handle,
                        self.event.handle,
                        Some(wait_callback),
                        Some(sender_ptr as *const std::ffi::c_void),
                        INFINITE,
                        WT_EXECUTEONLYONCE,
                    )
                };

                match result {
                    Ok(_) => {
                        self.state = EventWaitState::Waiting {
                            wait_handle,
                            receiver,
                        };
                        // Continue to poll the receiver
                        self.poll(cx)
                    }
                    Err(e) => {
                        // Clean up the sender if registration failed
                        unsafe {
                            drop(Box::from_raw(sender_ptr));
                        }
                        Poll::Ready(Err(e))
                    }
                }
            }
            EventWaitState::Waiting {
                wait_handle,
                receiver,
            } => {
                match receiver.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        // Unregister the wait
                        let _ = unsafe { UnregisterWait(*wait_handle) };
                        self.state = EventWaitState::Completed;
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(_)) => {
                        // Channel was dropped, this shouldn't happen
                        let _ = unsafe { UnregisterWait(*wait_handle) };
                        panic!("Receiver was dropped before completion");
                    }
                    Poll::Pending => Poll::Pending,
                }
            }
            EventWaitState::Completed => Poll::Ready(Ok(())),
        }
    }
}

// Callback function called by Windows thread pool
unsafe extern "system" fn wait_callback(
    context: *mut std::ffi::c_void,
    _timer_or_wait_fired: bool,
) {
    if !context.is_null() {
        // Convert back to sender and send completion signal
        let sender: Box<oneshot::Sender<()>> =
            unsafe { Box::from_raw(context as *mut oneshot::Sender<()>) };
        let _ = sender.send(());
    }
}

impl<'a> Drop for EventWaitFuture<'a> {
    fn drop(&mut self) {
        if let EventWaitState::Waiting { wait_handle, .. } = &self.state {
            // Unregister the wait if we're dropped while waiting
            let _ = unsafe { UnregisterWait(*wait_handle) };
        }
    }
}

// RAII wrapper for Windows Event handles
impl Drop for AsyncEvent {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test(flavor = "current_thread")]
    async fn test_async_event_signal_and_wait() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Signal the event immediately
        event.signal()?;

        // Wait for the event (should complete immediately)
        let result = timeout(Duration::from_millis(100), event.wait()).await;
        assert!(result.is_ok(), "Event wait should complete quickly");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_async_event_delayed_signal() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Copy the handle value as a raw pointer for thread safety
        let raw_handle = event.handle().0 as usize;

        // Spawn a task to signal the event after a delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            unsafe {
                let handle = HANDLE(raw_handle as *mut std::ffi::c_void);
                let _ = SetEvent(handle);
            }
        });

        // Wait for the event
        let result = timeout(Duration::from_millis(200), event.wait()).await;
        assert!(result.is_ok(), "Event wait should complete after signal");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_async_event_timeout() {
        let event = AsyncEvent::new().unwrap();

        // Wait for an event that will never be signaled (with timeout)
        let result = timeout(Duration::from_millis(50), event.wait()).await;
        assert!(result.is_err(), "Event wait should timeout");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_async_event_reset_and_reuse() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // First cycle: signal and wait
        event.signal()?;
        let result1 = timeout(Duration::from_millis(100), event.wait()).await;
        assert!(result1.is_ok(), "First wait should complete quickly");

        // Reset the event
        event.reset()?;

        // Second cycle: signal after a delay and wait
        let raw_handle = event.handle().0 as usize;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            unsafe {
                let handle = HANDLE(raw_handle as *mut std::ffi::c_void);
                let _ = SetEvent(handle);
            }
        });

        let result2 = timeout(Duration::from_millis(200), event.wait()).await;
        assert!(result2.is_ok(), "Second wait should complete after signal");

        // Reset again
        event.reset()?;

        // Third cycle: timeout test after reset
        let result3 = timeout(Duration::from_millis(50), event.wait()).await;
        assert!(result3.is_err(), "Third wait should timeout after reset");

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_multiple_waiters_on_reset_event() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Reset to ensure we start in non-signaled state
        event.reset()?;

        // Since this is an auto-reset event, only one waiter will be notified
        // Test that at least one waiter completes and the other times out
        let wait1_future = timeout(Duration::from_millis(200), event.wait());
        let wait2_future = timeout(Duration::from_millis(100), event.wait()); // Shorter timeout

        // Start both waiters concurrently and signal after a delay
        let signal_task = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            event.signal().unwrap();
        };

        // Run all operations concurrently
        let (result1, result2, _) = tokio::join!(wait1_future, wait2_future, signal_task);

        // With an auto-reset event, only one waiter should complete
        let completed_count = [&result1, &result2].iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            completed_count, 1,
            "Exactly one waiter should complete with auto-reset event"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_manual_reset_multiple_waiters() -> windows::core::Result<()> {
        let event = AsyncEvent::new_manual_reset()?;

        // Reset to ensure we start in non-signaled state
        event.reset()?;

        // Create futures for multiple waiters
        let wait1_future = timeout(Duration::from_millis(200), event.wait());
        let wait2_future = timeout(Duration::from_millis(200), event.wait());

        // Start both waiters concurrently and signal after a delay
        let signal_task = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            event.signal().unwrap();
        };

        // Run all operations concurrently
        let (result1, result2, _) = tokio::join!(wait1_future, wait2_future, signal_task);

        // With a manual-reset event, both waiters should complete
        assert!(
            result1.is_ok(),
            "First waiter should complete with manual-reset event"
        );
        assert!(
            result2.is_ok(),
            "Second waiter should complete with manual-reset event"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wait_sync_immediate() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Signal the event first
        event.signal()?;

        // Wait synchronously should return immediately
        let result = event.wait_sync(Some(100));
        assert!(
            result.is_ok(),
            "Sync wait should complete immediately when event is signaled"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wait_sync_timeout() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Don't signal the event, so it should timeout
        let result = event.wait_sync(Some(50));
        assert!(
            result.is_err(),
            "Sync wait should timeout when event is not signaled"
        );

        Ok(())
    }

    #[test]
    fn test_wait_sync_with_delayed_signal() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Signal the event in a separate thread after a delay
        let raw_handle = event.handle().0 as usize;
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            unsafe {
                let handle = HANDLE(raw_handle as *mut std::ffi::c_void);
                let _ = SetEvent(handle);
            }
        });

        // Wait synchronously (this will block until signaled)
        let result = event.wait_sync(Some(200));
        assert!(
            result.is_ok(),
            "Sync wait should complete when event is signaled"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wait_sync_infinite() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Signal the event first
        event.signal()?;

        // Wait with infinite timeout should return immediately
        let result = event.wait_sync_infinite();
        assert!(
            result.is_ok(),
            "Infinite sync wait should complete immediately when event is signaled"
        );

        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_wait_sync_none_timeout() -> windows::core::Result<()> {
        let event = AsyncEvent::new()?;

        // Signal the event first
        event.signal()?;

        // Wait with None timeout (infinite) should return immediately
        let result = event.wait_sync(None);
        assert!(
            result.is_ok(),
            "Sync wait with None timeout should complete immediately when event is signaled"
        );

        Ok(())
    }
}
