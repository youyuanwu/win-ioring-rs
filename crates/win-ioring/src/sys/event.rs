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
/// Asynchronous waiting lives on the crate-internal `ArmedEvent` instead, which
/// keeps one thread-pool registration armed for its whole life rather than
/// creating one per wait.
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
/// `ArmedEvent` is reachable only from the `Driver` that owns it or from a
/// [`crate::pipe::Server`]'s accept, neither of which is reachable from a pool
/// thread — `Driver` is `!Send` (asserted by a `compile_fail` doc-test), as is
/// `Server`, so a waker woken on a pool thread cannot legally poll either there.
/// The `PhantomData` below makes that argument compiler-checked rather than a
/// review obligation.
///
/// The accept path is the second owner, added after this comment was written,
/// and it is worth naming because it does *not* use the fused teardown below:
/// it releases the registration first, on its own, and only then collects. The
/// reason is that a blocking collect and a callback that may still fire are two
/// consumers of one auto-reset signal, which was measured deadlocking. See
/// [`release_registration`].
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
    /// Whether the registration is still live. See `Registration`.
    ///
    /// The teardown steps are separately callable so that a caller which needs
    /// the registration released *before* the value is dropped can do so; this
    /// field is what keeps the release idempotent across that split. Calling
    /// `UnregisterWaitEx` twice on one registration does not return an error —
    /// it terminates the process with `STATUS_INVALID_PARAMETER`.
    registration: Registration,
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

// Test seam: forces the next unregister to fail.
//
// Same reasoning as `FAIL_NEXT_ARM`, and the same thread-local scoping for the
// same reason. This one exists because `Drop`'s leak-rather-than-close branch
// was unreachable under test: `UnregisterWaitEx` has no reproducible failure
// mode either, so the branch that declines to close the handle had never been
// executed by anything. A branch no test can reach is a branch no test can
// check, which is the condition the ordering test below exists to end.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_UNREGISTER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the thread-pool registration is still live.
///
/// Three states rather than a boolean, because `Released` and `Failed` license
/// **opposite** actions: after a successful release the count may be reclaimed
/// and the handle closed, while after a failed one neither may ever happen. A
/// boolean would have to pick one of those to conflate with `Live`, and both
/// conflations are unsound in one direction.
///
/// `pub(crate)` in every configuration, not only under `cfg(test)`: the pipe
/// server's teardown releases the registration before its blocking collect and
/// must branch on the result in shipped code, because a failed release means the
/// pool may still be a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Registration {
    /// Registered with the thread pool; the release step has not run.
    Live,
    /// `UnregisterWaitEx` succeeded. No callback is running or can start.
    Released,
    /// `UnregisterWaitEx` failed. A callback may still be running, so the count
    /// must never be reclaimed and the handle must never be closed.
    Failed,
}

/// One teardown step, recorded as it is performed.
///
/// The recording lives *inside* the helper that does the work, not beside the
/// call to it, so that transposing two calls transposes the trace. Recording
/// held separately from the operations would pin the order of the
/// instrumentation and leave the order of the work unmeasured — which is the
/// precise failure this trace exists to rule out.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// `UnregisterWaitEx` was called. Carries whether it reported success.
    Unregister { succeeded: bool },
    /// The release step was entered in a state that was not `Live`, so it did
    /// nothing. Carries the state it observed.
    ///
    /// This is what makes "the guard branch is never taken on the driver's
    /// path" a measurable claim rather than an assertion about the driver: the
    /// driver's own teardown must produce a trace with no `ReleaseSkipped` in
    /// it.
    ReleaseSkipped(Registration),
    /// The operating system's reference count was reclaimed.
    Reclaim,
    /// The event handle was closed.
    Close,
}

#[cfg(test)]
thread_local! {
    static TEARDOWN_TRACE: std::cell::RefCell<Vec<Step>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn record(step: Step) {
    TEARDOWN_TRACE.with(|t| t.borrow_mut().push(step));
}

/// Test seam: clears the trace and returns what a closure's teardown recorded.
#[cfg(test)]
pub(crate) fn trace_of(f: impl FnOnce()) -> Vec<Step> {
    TEARDOWN_TRACE.with(|t| t.borrow_mut().clear());
    f();
    TEARDOWN_TRACE.with(|t| t.borrow().clone())
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

        // Hand one reference count to the operating system. It belongs to the
        // registration for its whole life; `Drop` reclaims it once the blocking
        // unregister has proved no callback can still be running.
        let raw = Arc::into_raw(Arc::clone(&shared));
        let mut wait = HANDLE::default();

        // The seam is consulted *after* the count has been handed over, so a
        // simulated failure takes the same reclaim path a real one does. Failing
        // earlier would have left that path — the one the test is named for —
        // unreachable.
        #[cfg(test)]
        let armed = if FAIL_NEXT_ARM.with(|f| f.replace(false)) {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            ))
        } else {
            // SAFETY: as below.
            unsafe {
                RegisterWaitForSingleObject(
                    &mut wait,
                    event.handle(),
                    Some(armed_callback),
                    Some(raw as *const std::ffi::c_void),
                    INFINITE,
                    WORKER_THREAD_FLAGS(0),
                )
            }
        };

        // SAFETY: `wait` is a local the call fills in; `event` outlives the
        // registration because both are fields of the value built below and
        // `Drop` unregisters before the handle closes; `raw` is a reference
        // count deliberately handed over for the callback to borrow. No flags,
        // so the wait re-arms rather than firing once.
        #[cfg(not(test))]
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
            registration: Registration::Live,
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

    /// Test seam: makes the next `UnregisterWaitEx` on this thread fail.
    ///
    /// Exposed beyond this module because the pipe server's teardown is the
    /// second `UnregisterWaitEx` call site in the crate, and its
    /// failed-release branch is the one FR-014's rationale singles out: a
    /// failed release means the pool may still be a consumer, which is the
    /// entire premise of releasing before the collect. Without the seam that
    /// branch cannot be reached by any test, in either caller.
    #[cfg(test)]
    pub(crate) fn fail_next_unregister() {
        FAIL_NEXT_UNREGISTER.with(|f| f.set(true));
    }

    /// Releases the thread-pool registration, blocking until no callback for it
    /// is running or can start. Idempotent.
    ///
    /// This is step one of three, and the only one that may run before the
    /// value is dropped. Splitting it out lets a caller holding an `ArmedEvent`
    /// through an operation the kernel may still be writing to release the
    /// registration at a point of its choosing, and then let `Drop` do the
    /// remaining two steps in their established order.
    ///
    /// Returns the state the registration is now in, which is also what the
    /// caller must consult before doing anything else: a `Failed` release means
    /// the count must never be reclaimed and the handle must never be closed.
    ///
    /// The guard is not an optimisation. `UnregisterWaitEx` called twice on one
    /// registration **terminates the process** with `STATUS_INVALID_PARAMETER`
    /// rather than returning an error, so a second call is not something a
    /// caller could detect and recover from.
    pub(crate) fn release_registration(&mut self) -> Registration {
        if self.registration != Registration::Live {
            #[cfg(test)]
            record(Step::ReleaseSkipped(self.registration));
            return self.registration;
        }

        // `INVALID_HANDLE_VALUE` asks the operating system to wait until every
        // callback for this registration has finished. Two things depend on it:
        // the reference count is only ours to reclaim once no callback can still
        // borrow it, and the event handle must not close while a wait is still
        // pending on it — the platform calls that undefined.
        //
        // Blocking here is safe only because this cannot run on a callback
        // thread; see the type's documentation. No lock is held across the call:
        // the callback releases the mutex before waking, so it can never be
        // holding it while this waits the callback out.
        #[cfg(test)]
        let unregistered = if FAIL_NEXT_UNREGISTER.with(|f| f.replace(false)) {
            Err(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            ))
        } else {
            // SAFETY: as below.
            unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) }
        };

        // SAFETY: `self.wait` came from a successful registration, and the guard
        // above is what makes "has not been unregistered before now" true — it
        // is the only path to this call, and it runs only from the `Live` state,
        // which this call then leaves.
        #[cfg(not(test))]
        let unregistered = unsafe { UnregisterWaitEx(self.wait, Some(INVALID_HANDLE_VALUE)) };

        self.registration = if unregistered.is_ok() {
            Registration::Released
        } else {
            Registration::Failed
        };

        #[cfg(test)]
        record(Step::Unregister {
            succeeded: unregistered.is_ok(),
        });

        self.registration
    }

    /// Reclaims the reference count handed to the operating system at arming.
    ///
    /// Step two of three. Sound only after a *successful* release: the count is
    /// what the callback borrows, so reclaiming it while a callback could still
    /// start would free state that callback is about to touch.
    ///
    /// # Safety
    ///
    /// `release_registration` must have returned `Released`, and this must not
    /// have been called before.
    unsafe fn reclaim_count(&mut self) {
        #[cfg(test)]
        record(Step::Reclaim);
        // SAFETY: the caller has established that the blocking unregister
        // succeeded, so no callback is running or can start, and that this runs
        // exactly once. The callback never reclaims this count itself.
        unsafe { drop(Arc::from_raw(self.raw)) };
    }

    /// Closes the event handle.
    ///
    /// Step three of three, and last for a reason: closing a handle with a wait
    /// still pending on it is undefined, so this may only follow a successful
    /// release.
    ///
    /// # Safety
    ///
    /// `release_registration` must have returned `Released`, this must not have
    /// been called before, and the value must not be used afterwards.
    unsafe fn close_event(&mut self) {
        #[cfg(test)]
        record(Step::Close);
        // SAFETY: the caller has established that no wait is pending on this
        // handle and that this runs exactly once on a value about to be
        // destroyed.
        unsafe { std::mem::ManuallyDrop::drop(&mut self.event) };
    }

    /// Test seam: a weak handle to the shared state, so a test can tell a leak
    /// from a correct reclaim by whether the allocation outlived this value.
    #[cfg(test)]
    pub(crate) fn watch(&self) -> std::sync::Weak<ArmedShared> {
        Arc::downgrade(&self.shared)
    }
}

impl Drop for ArmedEvent {
    /// Unregister, then reclaim, then close.
    ///
    /// The order is the point, and it is now pinned by a test rather than by
    /// this comment — see `teardown_runs_unregister_then_reclaim_then_close`.
    /// Each step records itself from inside the helper that performs it, so
    /// transposing two of these calls transposes the recorded trace.
    ///
    /// A failed release stops the sequence here. The wait may still be pending,
    /// so closing the handle would be undefined and reclaiming the count could
    /// free state a callback is about to touch. Leak both instead — the event is
    /// `ManuallyDrop` precisely so this path can decline to close it. This
    /// crate's standing rule is to leak or abort rather than reach undefined
    /// behaviour, and a failed unregister has no recovery that is not one of
    /// those.
    fn drop(&mut self) {
        if self.release_registration() == Registration::Failed {
            return;
        }

        // SAFETY: the release above returned `Released` — either here or at an
        // earlier explicit release, which the guard makes idempotent — so no
        // callback is running or can start. Both run exactly once because
        // `self` is being destroyed and nothing can call them again.
        unsafe {
            self.reclaim_count();
            self.close_event();
        }
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

    // Nine tests were removed here when `AsyncEvent::wait` and
    // `EventWaitFuture` were deleted. The reasons, so a reader of this file does
    // not have to find the pull request:
    //
    // - `test_async_event_signal_and_wait`, `test_async_event_delayed_signal`,
    //   `test_async_event_timeout`, `test_async_event_reset_and_reuse` —
    //   rewritten against `ArmedEvent` below, asserting the same properties:
    //   signalled-before-wait, signalled-after-wait, never-signalled, and one
    //   event serving many waits.
    // - `test_multiple_waiters_on_reset_event`,
    //   `test_manual_reset_multiple_waiters` — **removed outright**. Both
    //   asserted how many of two concurrent waiters a given event mode releases.
    //   `ArmedEvent` is single-consumer by construction: one registration, one
    //   waker slot. They would have asserted a property the replacement
    //   deliberately does not have. The contract that replaced it is asserted by
    //   `a_second_poller_replaces_the_first_pollers_waker`.
    // - `cancelled_waits_do_not_leak`, `dropping_a_wait_while_it_fires_is_safe`,
    //   `a_replacement_wait_still_sees_the_signal`, and the
    //   `register_then_abandon` helper — superseded by the reference-count and
    //   drop-race tests below, which exercise the same three hazards against the
    //   primitive that now exists.
    //
    // The five `wait_sync` tests are unchanged: that surface is untouched.

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
        // The seam fails the arming *after* the reference count has been handed
        // over, so this exercises the reclaim branch rather than skipping it.
        // The `Weak` is what proves the count came back: a leak would leave the
        // allocation alive after both the `Arc` and the raw count are gone.
        let watch = {
            let probe = ArmedEvent::new().unwrap();
            probe.watch()
        };
        assert_eq!(watch.strong_count(), 0, "control: a normal build reclaims");

        ArmedEvent::fail_next_arm();
        let result = ArmedEvent::new();
        assert!(
            result.is_err(),
            "a wait that cannot be armed must be reported, not swallowed — a \
             driver that silently could not be woken would hang"
        );

        // The seam is consumed, so the next construction succeeds and works.
        let event = ArmedEvent::new().unwrap();
        let watch = event.watch();
        assert_eq!(
            watch.strong_count(),
            2,
            "a live registration holds exactly one count besides ours"
        );
        event.signal().unwrap();
        wait_once(&event).await;
        drop(event);
        assert_eq!(
            watch.strong_count(),
            0,
            "teardown must reclaim the operating system's count exactly once"
        );
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

    // ---- teardown ordering ------------------------------------------------
    //
    // The three teardown steps must run as unregister, then reclaim, then
    // close, and `Drop`'s comment has called that "the point" since the wake-path
    // work. Nothing verified it. Every test above passes with the reclaim and
    // the close transposed, and passes with either elided, because none of them
    // can observe the order in which teardown did its work — they observe only
    // its end state, which the transposition does not change.
    //
    // That gap is pre-existing and independent of any caller. It became visible
    // because the release step was split out for a caller that needs to release
    // early, and splitting it is exactly what makes a transposition easy to
    // introduce by accident.
    //
    // The tests below close it. They read a trace each step appends from inside
    // the helper that performs it, so what is pinned is the order of the work
    // rather than the order of some instrumentation standing beside it.

    /// The whole point: the sequence, in order, on the ordinary path.
    ///
    /// Fails against all three transpositions and all three elisions, which is
    /// the entire mutation space for three steps — enumerated rather than
    /// sampled.
    #[tokio::test(flavor = "current_thread")]
    async fn teardown_runs_unregister_then_reclaim_then_close() {
        let trace = trace_of(|| {
            let event = ArmedEvent::new().unwrap();
            drop(event);
        });

        assert_eq!(
            trace,
            vec![
                Step::Unregister { succeeded: true },
                Step::Reclaim,
                Step::Close
            ],
            "teardown must unregister, then reclaim, then close; \
             reclaiming or closing before the unregister has proved no callback \
             can still be running is undefined behaviour, not a style question"
        );
    }

    /// The driver's own path must not take the guard's early return.
    ///
    /// `release_registration`'s guard exists for callers that release early. The
    /// driver is not one of them, and the claim that the split changed nothing
    /// for the driver rests on its teardown entering the release step in the
    /// `Live` state exactly as it did before the state existed. That is an
    /// assertion about the driver, so it needs a test that fails if the driver
    /// ever stops behaving that way.
    #[tokio::test(flavor = "current_thread")]
    async fn the_ordinary_drop_path_never_takes_the_release_guard() {
        // Exercise the event first, so this is a drop of a *used* value and not
        // merely of a freshly armed one. The wait is awaited rather than polled
        // once: the callback runs on a pool thread, so a single poll straight
        // after the signal races it and would make this test flaky for a reason
        // that has nothing to do with what it asserts.
        let event = ArmedEvent::new().unwrap();
        event.signal().unwrap();
        wait_once(&event).await;

        // Only the teardown is traced. Nothing before it records anything, but
        // scoping it this narrowly keeps the assertion about the drop alone.
        let trace = trace_of(|| drop(event));

        assert!(
            !trace.iter().any(|s| matches!(s, Step::ReleaseSkipped(_))),
            "the ordinary drop path must enter the release step in the Live \
             state, taking the same route it took before the state existed; \
             trace was {trace:?}"
        );
        assert_eq!(
            trace.first(),
            Some(&Step::Unregister { succeeded: true }),
            "and it must actually perform the unregister; trace was {trace:?}"
        );
    }

    /// A failed unregister leaks rather than reaching undefined behaviour.
    ///
    /// Reachable only through `FAIL_NEXT_UNREGISTER`: `UnregisterWaitEx` has no
    /// reproducible failure mode, so before that seam existed this branch could
    /// not be executed by any test. It deliberately leaks the registration and
    /// the event handle for the life of the process — that is the behaviour
    /// under test, not an oversight.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_unregister_neither_reclaims_nor_closes() {
        let watch = std::cell::RefCell::new(None);
        let trace = trace_of(|| {
            let event = ArmedEvent::new().unwrap();
            *watch.borrow_mut() = Some(event.watch());
            FAIL_NEXT_UNREGISTER.with(|f| f.set(true));
            drop(event);
        });

        assert_eq!(
            trace,
            vec![Step::Unregister { succeeded: false }],
            "a failed unregister must stop the sequence: the wait may still be \
             pending, so closing the handle would be undefined and reclaiming \
             the count could free state a callback is about to touch"
        );
        assert!(
            watch.borrow().as_ref().unwrap().upgrade().is_some(),
            "the count must be leaked, not reclaimed, when the unregister failed"
        );
    }

    /// An early release followed by a drop unregisters exactly once.
    ///
    /// Itemised rather than left to the mutation table because the failure mode
    /// is not an assertion failure: `UnregisterWaitEx` called twice on one
    /// registration terminates the process with `STATUS_INVALID_PARAMETER`
    /// instead of returning an error. Without the guard this test does not fail,
    /// it aborts the test binary — so the guard is load-bearing for soundness
    /// and not merely for tidiness.
    #[tokio::test(flavor = "current_thread")]
    async fn an_early_release_then_drop_unregisters_once_and_still_tears_down() {
        let trace = trace_of(|| {
            let mut event = ArmedEvent::new().unwrap();
            assert_eq!(event.release_registration(), Registration::Released);
            drop(event);
        });

        assert_eq!(
            trace,
            vec![
                Step::Unregister { succeeded: true },
                Step::ReleaseSkipped(Registration::Released),
                Step::Reclaim,
                Step::Close
            ],
            "the early release must unregister once, the drop's release must \
             observe Released and do nothing, and the remaining two steps must \
             still run in order"
        );
        assert_eq!(
            trace
                .iter()
                .filter(|s| matches!(s, Step::Unregister { .. }))
                .count(),
            1,
            "exactly one unregister; a second would terminate the process"
        );
    }

    /// A failed release is never retried, even across an explicit release.
    ///
    /// The tri-state is what makes this expressible: `Released` and `Failed`
    /// both mean "not live", but only one of them licenses the reclaim and the
    /// close. A boolean would have to conflate `Failed` with one of the other
    /// two, and both conflations are unsound in one direction.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_release_is_not_retried_by_drop() {
        let trace = trace_of(|| {
            let mut event = ArmedEvent::new().unwrap();
            FAIL_NEXT_UNREGISTER.with(|f| f.set(true));
            assert_eq!(event.release_registration(), Registration::Failed);
            drop(event);
        });

        assert_eq!(
            trace,
            vec![
                Step::Unregister { succeeded: false },
                Step::ReleaseSkipped(Registration::Failed),
            ],
            "drop must observe the earlier failure and neither retry the \
             unregister nor proceed to reclaim or close"
        );
    }
}
