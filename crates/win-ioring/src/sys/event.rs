use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWaitEx,
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
                WAIT_TIMEOUT => Err(windows::core::Error::from_thread()),
                _ => Err(windows::core::Error::from_thread()),
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

/// State shared between a waiting future and the thread pool callback that
/// signals it.
///
/// The callback runs on an operating system thread pool thread, so this must be
/// thread-safe even though the driver that ultimately consumes the wakeup is
/// single-threaded. Waking across threads is exactly what makes the crate
/// runtime-agnostic: the executor's own waker does the hand-off.
struct WaitShared {
    state: Mutex<WaitState>,
    /// Set by the callback just before it returns.
    ///
    /// Read only after `UnregisterWaitEx` has confirmed no callback can still
    /// be running, at which point it says definitively whether the callback
    /// consumed the reference count that was handed to the operating system.
    callback_ran: AtomicBool,
}

enum WaitState {
    Waiting(Option<Waker>),
    Signalled,
}

/// A live registration with the thread pool.
struct Registration {
    wait_handle: HANDLE,
    shared: Arc<WaitShared>,
    /// The reference count handed to the operating system, reclaimed on drop if
    /// the callback never ran.
    raw: *const WaitShared,
}

/// Waits for an [`AsyncEvent`] to become signalled.
///
/// The wait is registered with the operating system thread pool rather than
/// occupying a thread, and completion is delivered through the task's waker, so
/// this works under any executor.
pub struct EventWaitFuture<'a> {
    event: &'a AsyncEvent,
    registration: Option<Registration>,
}

impl<'a> EventWaitFuture<'a> {
    fn new(event: &'a AsyncEvent) -> Self {
        Self {
            event,
            registration: None,
        }
    }
}

impl Future for EventWaitFuture<'_> {
    type Output = crate::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        if this.registration.is_none() {
            let shared = Arc::new(WaitShared {
                state: Mutex::new(WaitState::Waiting(Some(cx.waker().clone()))),
                callback_ran: AtomicBool::new(false),
            });

            // Hand one reference count to the operating system. The callback
            // reclaims it; if the callback never runs, `Drop` does.
            let raw = Arc::into_raw(Arc::clone(&shared));

            let mut wait_handle = HANDLE::default();
            let result = unsafe {
                RegisterWaitForSingleObject(
                    &mut wait_handle,
                    this.event.handle,
                    Some(wait_callback),
                    Some(raw as *const std::ffi::c_void),
                    INFINITE,
                    WT_EXECUTEONLYONCE,
                )
            };

            if let Err(e) = result {
                // Registration failed, so the callback will never run and the
                // reference count we handed over must come back here.
                unsafe { drop(Arc::from_raw(raw)) };
                return Poll::Ready(Err(e.into()));
            }

            this.registration = Some(Registration {
                wait_handle,
                shared,
                raw,
            });
        }

        let registration = this.registration.as_ref().expect("just registered");
        let mut state = registration
            .shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match &mut *state {
            WaitState::Signalled => Poll::Ready(Ok(())),
            WaitState::Waiting(slot) => {
                // Refresh the waker: the future may have been polled by a
                // different task than the one that registered it.
                *slot = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl Drop for EventWaitFuture<'_> {
    fn drop(&mut self) {
        let Some(registration) = self.registration.take() else {
            return;
        };

        if registration.shared.callback_ran.load(Ordering::Acquire) {
            // The callback has already run, so it consumed the reference count
            // and there is nothing to reclaim. Checking this *first* also
            // avoids a self-deadlock: an executor may wake a task inline from
            // the callback, and that task may drop this future, so blocking
            // below would be waiting for the very callback we are inside.
            let _ = unsafe { UnregisterWaitEx(registration.wait_handle, None) };
            return;
        }

        // `INVALID_HANDLE_VALUE` asks the operating system to wait until every
        // callback for this registration has finished. Without it the flag read
        // below would be a guess rather than an answer.
        let _ = unsafe { UnregisterWaitEx(registration.wait_handle, Some(INVALID_HANDLE_VALUE)) };

        if !registration.shared.callback_ran.load(Ordering::Acquire) {
            // The callback never ran and never will, so the reference count
            // handed to the operating system is ours to reclaim. Failing to do
            // this leaks the shared state on every cancelled wait.
            unsafe { drop(Arc::from_raw(registration.raw)) };
        }
    }
}

/// Invoked by the operating system thread pool when the event is signalled.
///
/// # Safety
///
/// `context` must be the pointer produced by `Arc::into_raw` when the wait was
/// registered, and this function must be called at most once for it, which
/// `WT_EXECUTEONLYONCE` guarantees.
unsafe extern "system" fn wait_callback(context: *mut std::ffi::c_void, _timer_fired: bool) {
    if context.is_null() {
        return;
    }
    // Take back the reference count that was handed to the operating system.
    let shared = unsafe { Arc::from_raw(context as *const WaitShared) };

    let waker = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let waker = match &mut *state {
            WaitState::Waiting(slot) => slot.take(),
            WaitState::Signalled => None,
        };
        *state = WaitState::Signalled;
        waker
    };

    // Publish before returning, so that a concurrent `Drop` which has just
    // waited for this callback sees that the reference count was consumed.
    shared.callback_ran.store(true, Ordering::Release);

    // Wake outside the lock: the executor may poll the future immediately, and
    // that poll takes the same lock.
    drop(shared);
    if let Some(waker) = waker {
        waker.wake();
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

    /// Polls a wait once so it registers with the thread pool, then abandons
    /// it. The future is dropped at the end of this call, not the pin.
    fn register_then_abandon(event: &AsyncEvent) {
        let fut = event.wait();
        futures::pin_mut!(fut);
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        // `fut` goes out of scope here, dropping the future itself.
    }

    /// A wait abandoned before the event fires must reclaim the reference count
    /// it handed to the operating system. Previously every cancelled wait
    /// leaked its shared state.
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_waits_do_not_leak() {
        let event = AsyncEvent::new().unwrap();
        for _ in 0..2000 {
            register_then_abandon(&event);
        }

        // The event still works afterwards.
        event.signal().unwrap();
        timeout(Duration::from_millis(500), event.wait())
            .await
            .expect("wait timed out")
            .unwrap();
    }

    /// Abandoning a wait at the moment the event fires exercises the race
    /// between the thread pool callback and `UnregisterWaitEx`.
    #[tokio::test(flavor = "current_thread")]
    async fn dropping_a_wait_while_it_fires_is_safe() {
        for _ in 0..500 {
            let event = AsyncEvent::new().unwrap();
            {
                let fut = event.wait();
                futures::pin_mut!(fut);
                let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
                assert!(fut.as_mut().poll(&mut cx).is_pending());
                // Signal while the wait is still registered, so the callback
                // may be running concurrently with the drop below.
                event.signal().unwrap();
            }
        }
    }

    /// A wait that is abandoned and replaced must still observe the signal.
    #[tokio::test(flavor = "current_thread")]
    async fn a_replacement_wait_still_sees_the_signal() {
        let event = AsyncEvent::new_manual_reset().unwrap();
        register_then_abandon(&event);

        event.signal().unwrap();
        timeout(Duration::from_millis(500), event.wait())
            .await
            .expect("replacement wait timed out")
            .unwrap();
    }
}
