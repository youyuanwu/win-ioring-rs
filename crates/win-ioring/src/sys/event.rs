use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateEventW, INFINITE, RegisterWaitForSingleObject, ResetEvent, SetEvent, UnregisterWaitEx,
    WORKER_THREAD_FLAGS, WaitForSingleObject,
};

/// A Windows event object.
///
/// A thin owner for the handle, with synchronous waiting. Exposed because the
/// raw [`crate::io_ring`] layer needs a completion event and a caller driving a
/// ring by hand needs to be able to make one.
///
/// Asynchronous waiting lives on [`ArmedEvent`] instead, which keeps one
/// thread-pool registration armed for its whole life rather than creating one
/// per wait.
pub struct AsyncEvent {
    handle: HANDLE,
}

impl AsyncEvent {
    /// Creates a new auto-reset event in the non-signaled state.
    /// Auto-reset events automatically return to non-signaled state after one waiter is released.
    pub fn new() -> windows::core::Result<Self> {
        // SAFETY: all arguments are plain values. The handle is owned by the
        // `AsyncEvent` built from it and closed exactly once, on drop.
        let handle = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self { handle })
    }

    /// Creates a new manual-reset event in the non-signaled state.
    /// Manual-reset events remain signaled until explicitly reset, allowing multiple waiters to be released.
    pub fn new_manual_reset() -> windows::core::Result<Self> {
        // SAFETY: as for `new`, but asking for a manual-reset event.
        let handle = unsafe { CreateEventW(None, true, false, None)? };
        Ok(Self { handle })
    }

    /// Signals the event, allowing waiting tasks to complete.
    pub fn signal(&self) -> windows::core::Result<()> {
        // SAFETY: `self.handle` is open for as long as `self` is alive.
        unsafe { SetEvent(self.handle) }
    }

    /// Resets the event to the non-signaled state, allowing it to be reused.
    /// Resets the event to the non-signaled state, allowing it to be reused.
    /// After calling reset(), a subsequent wait will block until signal() is
    /// called again.
    pub fn reset(&self) -> windows::core::Result<()> {
        // SAFETY: `self.handle` is open for as long as `self` is alive.
        unsafe { ResetEvent(self.handle) }
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
        // SAFETY: `self.handle` is open for as long as `self` is alive, and the
        // timeout is a plain value.
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

    /// Returns the raw event handle.
    ///
    /// Used to hand the event to the platform, which signals it when a
    /// completion is queued. The handle is owned by this event and closed with
    /// it, so the caller must not close it.
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
/// A Windows event with one thread-pool wait armed for its whole life.
///
/// This is how a completion signalled by the platform reaches the driver's
/// thread. The registration is created once, in the constructor, and torn down
/// once, in `Drop` — never per wait. Arming and disarming a thread-pool wait on
/// every park cost more than the I/O being announced.
///
/// # Why it owns its event
///
/// The registration deliberately omits `WT_EXECUTEONLYONCE`, so the operating
/// system re-arms it after every callback. The platform's own guidance is that
/// an object left signalled — a manual-reset event — must not be registered that
/// way, because the callback may then run "too many times before the event is
/// reset". An auto-reset event is therefore not a preference here but a
/// requirement, and constructing the event inside makes handing this type the
/// wrong kind unrepresentable rather than merely documented.
///
/// # Why it is crate-private and `!Send`
///
/// `Drop` waits out any running callback with a blocking `UnregisterWaitEx`.
/// That is sound only because it can never run *on* a callback thread: an
/// `ArmedEvent` is reachable only from the `Driver` that owns it, and `Driver`
/// is `!Send` (asserted by a `compile_fail` doc-test), so a waker woken on a
/// pool thread cannot legally poll it there. The `PhantomData` below makes that
/// argument compiler-checked rather than a review obligation.
///
/// Making this type public would give that guarantee away, and the teardown
/// would then need the two-path form the per-wait future used to have —
/// non-blocking unregister when dropping from inside the callback, blocking
/// otherwise, with the reclaim deferred to callback exit. Anyone reaching for
/// `pub` here is taking that on.
pub(crate) struct ArmedEvent {
    /// The event the platform signals. Auto-reset; see above.
    ///
    /// ManuallyDrop so that a failed unregister can decline to close it: a
    /// handle closed with a wait still pending is undefined behaviour, so the
    /// only sound response to that failure is to leak it.
    event: std::mem::ManuallyDrop<AsyncEvent>,
    /// The live registration with the thread pool.
    wait: HANDLE,
    /// The reference count handed to the operating system, owned by the
    /// registration for its whole life and reclaimed exactly once, in `Drop`.
    raw: *const ArmedShared,
    /// Our end of the shared state.
    shared: Arc<ArmedShared>,
    /// Makes the type `!Send` and `!Sync`, which is what licenses the blocking
    /// unregister in `Drop`.
    _not_send: PhantomData<*const ()>,
}

/// State shared between the driver and the thread-pool callback.
///
/// The callback runs on an operating-system thread pool thread, so this must be
/// thread-safe even though the driver that consumes the wakeup is
/// single-threaded. Waking across threads is exactly what makes the crate
/// runtime-agnostic: the executor's own waker does the hand-off.
pub(crate) struct ArmedShared {
    state: Mutex<ArmedState>,
}

struct ArmedState {
    /// Set by the callback, cleared only by a resolved poll.
    ///
    /// Sticky on purpose. A signal raised while nobody is polling — between two
    /// parks, or while the driver is mid-pass — must still be seen by the next
    /// poll, or a completion could be announced to nobody and the driver would
    /// park on work the platform has already finished.
    signalled: bool,
    /// The parked driver's waker, present only while it is parked.
    waker: Option<Waker>,
}

// Test seam: forces the next arming to fail.
//
// `RegisterWaitForSingleObject` has no reliably reproducible failure mode, so
// the path that reports one cannot be exercised without injecting it. A
// thread-local rather than a global, so tests running in parallel cannot
// consume each other's injection.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_ARM: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl ArmedEvent {
    /// Creates an auto-reset event and arms one thread-pool wait on it.
    ///
    /// The wait re-arms itself after every callback, so this is the only
    /// registration the event will ever have.
    pub(crate) fn new() -> crate::Result<Self> {
        let event = AsyncEvent::new()?;
        let shared = Arc::new(ArmedShared {
            state: Mutex::new(ArmedState {
                signalled: false,
                waker: None,
            }),
        });

        #[cfg(test)]
        if FAIL_NEXT_ARM.with(|f| f.replace(false)) {
            return Err(crate::Error::Os(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            )));
        }

        // Hand one reference count to the operating system. It belongs to the
        // registration for its whole life; `Drop` reclaims it once the blocking
        // unregister has proved no callback can still be running.
        let raw = Arc::into_raw(Arc::clone(&shared));
        let mut wait = HANDLE::default();
        // SAFETY: `wait` is a local the call fills in; `event` outlives the
        // registration because both are fields of the value built below and
        // `Drop` unregisters before the handle closes; `raw` is a reference
        // count deliberately handed over for the callback to borrow. No flags,
        // so the wait re-arms rather than firing once.
        let armed = unsafe {
            RegisterWaitForSingleObject(
                &mut wait,
                event.handle(),
                Some(armed_callback),
                Some(raw as *const std::ffi::c_void),
                INFINITE,
                WORKER_THREAD_FLAGS(0),
            )
        };

        if let Err(e) = armed {
            // Nothing was registered, so the callback will never run and the
            // count we handed over is ours to take back. This is the only place
            // it can come back on this path.
            // SAFETY: arming failed, so no callback exists to reclaim `raw`, and
            // the early return below means this happens exactly once.
            unsafe { drop(Arc::from_raw(raw)) };
            return Err(e.into());
        }

        Ok(Self {
            event: std::mem::ManuallyDrop::new(event),
            wait,
            raw,
            shared,
            _not_send: PhantomData,
        })
    }

    /// The handle to hand the ring.
    pub(crate) fn handle(&self) -> HANDLE {
        self.event.handle()
    }

    /// Signals the underlying event.
    ///
    /// Test-only: in production the kernel signals this event, never the crate,
    /// so a method with only test callers would be dead code in a non-test
    /// build and the crate denies warnings.
    #[cfg(test)]
    pub(crate) fn signal(&self) -> crate::Result<()> {
        self.event.signal().map_err(Into::into)
    }

    /// Resolves if a signal is outstanding, consuming it; otherwise records
    /// `cx`'s waker and returns `Pending`.
    ///
    /// The waker is replaced on every poll, not just the first, because the
    /// task driving may not be the one that parked last time.
    pub(crate) fn poll_signalled(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.signalled {
            state.signalled = false;
            state.waker = None;
            return Poll::Ready(());
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    /// Drops the recorded waker without touching the signal.
    ///
    /// Used when the park resolves for some other reason. Losing the waker
    /// cannot lose a completion: `signalled` is sticky, so a signal arriving
    /// with no waker recorded is still seen by the next poll.
    pub(crate) fn release_waker(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.waker = None;
    }

    /// Test seam: makes the next [`ArmedEvent::new`] on this thread fail.
    #[cfg(test)]
    pub(crate) fn fail_next_arm() {
        FAIL_NEXT_ARM.with(|f| f.set(true));
    }

    /// Test seam: a weak handle to the shared state, so a test can tell a leak
    /// from a correct reclaim by whether the allocation outlived this value.
    #[cfg(test)]
    pub(crate) fn watch(&self) -> std::sync::Weak<ArmedShared> {
        Arc::downgrade(&self.shared)
    }
}

impl Drop for ArmedEvent {
    fn drop(&mut self) {
        // `INVALID_HANDLE_VALUE` asks the operating system to wait until every
        // callback for this registration has finished. Two things depend on it:
        // the reference count below is only ours to reclaim once no callback
        // can still borrow it, and the event handle must not close while a wait
        // is still pending on it — the platform calls that undefined.
        //
        // Blocking here is safe only because this cannot run on a callback
        // thread; see the type's documentation. No lock is held across the call:
        // the callback releases the mutex before waking, so it can never be
        // holding it while this waits the callback out.
        // SAFETY: `self.wait` came from a successful registration and has not
        // been unregistered before now.
        let unregistered = unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) };

        if unregistered.is_err() {
            // The wait may still be pending, so closing the handle would be
            // undefined and reclaiming the count could free state a callback is
            // about to touch. Leak both instead — the event is `ManuallyDrop`
            // precisely so this path can decline to close it. This crate's
            // standing rule is to leak or abort rather than reach undefined
            // behaviour, and a failed unregister has no recovery that is not one
            // of those.
            return;
        }

        // SAFETY: the blocking unregister above succeeded, so no callback is
        // running or can start, and this count is reclaimed exactly once — the
        // callback never touches it.
        unsafe { drop(Arc::from_raw(self.raw)) };
        // Only now is the handle safe to close. The order is the point:
        // unregister, then reclaim, then close.
        // SAFETY: reached on the only path that gets here, exactly once, and
        // never again because `self` is being destroyed.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.event) };
    }
}

/// Invoked by the operating system thread pool when the event is signalled.
///
/// # Safety
///
/// `context` must be the pointer produced by `Arc::into_raw` when the wait was
/// armed, and the registration must still be live. Both hold because the count
/// is reclaimed only after a blocking `UnregisterWaitEx` has proved no callback
/// is running or can start.
unsafe extern "system" fn armed_callback(context: *mut std::ffi::c_void, _timer_fired: bool) {
    if context.is_null() {
        return;
    }
    // Borrow, do not take: this registration re-arms, so the count must survive
    // every callback and is reclaimed only by `Drop`.
    // SAFETY: the caller guarantees `context` points to live shared state whose
    // reference count outlives this call.
    let shared = unsafe { &*(context as *const ArmedShared) };

    let waker = {
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.signalled = true;
        state.waker.take()
    };

    // Wake outside the lock: the executor may poll the driver immediately, and
    // that poll takes the same lock. Releasing first is also what lets `Drop`
    // block on this callback without the two deadlocking.
    if let Some(waker) = waker {
        waker.wake();
    }
}

impl Drop for AsyncEvent {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is open and owned by this event, and `self` is
        // being dropped, so it cannot be closed again.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    // ---- the armed wait ---------------------------------------------------
    //
    // `ArmedEvent` keeps one thread-pool registration for its whole life, so
    // the properties worth testing are different from a per-wait future's.
    // What matters is that a signal is never lost across the boundaries where
    // nobody is polling, that the waker follows whichever task is waiting now,
    // and that the single reference count handed to the operating system comes
    // back exactly once.

    /// Resolves the wait once, failing rather than hanging if it never fires.
    async fn wait_once(event: &ArmedEvent) {
        timeout(
            Duration::from_secs(5),
            std::future::poll_fn(|cx| event.poll_signalled(cx)),
        )
        .await
        .expect("the armed wait should have resolved");
    }

    /// Polls once without awaiting, reporting whether it resolved.
    fn poll_once(event: &ArmedEvent) -> bool {
        let mut cx = std::task::Context::from_waker(futures::task::noop_waker_ref());
        event.poll_signalled(&mut cx).is_ready()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_signal_raised_before_the_first_poll_is_observed() {
        // The signal is sticky, so arming and polling need not race the kernel.
        let event = ArmedEvent::new().unwrap();
        event.signal().unwrap();
        wait_once(&event).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_signal_raised_between_two_polls_is_observed_by_the_second() {
        let event = ArmedEvent::new().unwrap();
        assert!(!poll_once(&event), "nothing signalled yet");
        event.signal().unwrap();
        wait_once(&event).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn resolving_consumes_the_signal() {
        let event = ArmedEvent::new().unwrap();
        event.signal().unwrap();
        wait_once(&event).await;
        assert!(
            !poll_once(&event),
            "a resolved wait must consume its signal, or the driver would spin"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_second_poller_replaces_the_first_pollers_waker() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct Counting(AtomicUsize);
        impl std::task::Wake for Counting {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let event = ArmedEvent::new().unwrap();
        let first = Arc::new(Counting(AtomicUsize::new(0)));
        let second = Arc::new(Counting(AtomicUsize::new(0)));
        let w1 = std::task::Waker::from(Arc::clone(&first));
        let w2 = std::task::Waker::from(Arc::clone(&second));

        assert!(
            event
                .poll_signalled(&mut std::task::Context::from_waker(&w1))
                .is_pending()
        );
        assert!(
            event
                .poll_signalled(&mut std::task::Context::from_waker(&w2))
                .is_pending()
        );

        event.signal().unwrap();
        // Give the thread pool a moment to dispatch.
        for _ in 0..500 {
            if second.0.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        assert_eq!(
            first.0.load(Ordering::SeqCst),
            0,
            "the waker from the earlier poll must not be used"
        );
        assert!(
            second.0.load(Ordering::SeqCst) > 0,
            "the wake must reach whichever task is waiting now"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn many_signals_before_one_poll_produce_one_resolution() {
        let event = ArmedEvent::new().unwrap();
        for _ in 0..8 {
            event.signal().unwrap();
        }
        // Let the auto-reset event and its re-arming registration settle.
        tokio::time::sleep(Duration::from_millis(50)).await;
        wait_once(&event).await;
        assert!(
            !poll_once(&event),
            "signals collapse into one outstanding resolution"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn releasing_the_waker_does_not_lose_a_later_signal() {
        // The park does exactly this when it resolves for some other reason.
        let event = ArmedEvent::new().unwrap();
        assert!(!poll_once(&event));
        event.release_waker();
        event.signal().unwrap();
        wait_once(&event).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_unpolled_armed_wait_absorbs_many_signals() {
        // The registration is always armed, including on passes where the
        // driver is not parked. Nothing should accumulate.
        let watch = {
            let event = ArmedEvent::new().unwrap();
            let watch = event.watch();
            for _ in 0..10_000 {
                event.signal().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert_eq!(
                watch.strong_count(),
                2,
                "exactly two counts: ours and the operating system's"
            );
            watch
        };
        assert_eq!(
            watch.strong_count(),
            0,
            "the count handed to the operating system must come back exactly once"
        );
        assert!(watch.upgrade().is_none(), "the shared state must be freed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_while_signals_are_in_flight_is_safe() {
        // The unregister/callback race, several hundred times over. A leak
        // leaves the weak handle alive; a double reclaim aborts the process.
        for _ in 0..300 {
            let watch = {
                let event = ArmedEvent::new().unwrap();
                let watch = event.watch();
                event.signal().unwrap();
                event.signal().unwrap();
                watch
            };
            assert_eq!(
                watch.strong_count(),
                0,
                "dropping must reclaim the operating system's count exactly once"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failure_to_arm_is_reported_and_reclaims_its_count() {
        ArmedEvent::fail_next_arm();
        let result = ArmedEvent::new();
        assert!(
            result.is_err(),
            "a wait that cannot be armed must be reported, not swallowed — a \
             driver that silently could not be woken would hang"
        );
        // The seam is consumed, so the next construction succeeds.
        let event = ArmedEvent::new().unwrap();
        event.signal().unwrap();
        wait_once(&event).await;
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
