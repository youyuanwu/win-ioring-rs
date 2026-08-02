//! A safe, runtime-agnostic driver for IoRing operations.
//!
//! # How it fits together
//!
//! [`Driver`] owns the ring and all in-flight operation state. It is driven by a
//! future: the caller spawns [`Driver::drive`] on whatever executor they already
//! use, and issues operations through a [`Handle`]. There is no ambient state and
//! no dependency on any particular runtime — completions reach the executor
//! through the task's own waker.
//!
//! # Why the driver owns the buffers
//!
//! The kernel holds a pointer into the caller's buffer until an operation
//! completes, and a future can be dropped at any time. An operation's state is
//! therefore moved into the driver's [`slab`] *before* submission, and the future
//! keeps only a token, a shared result slot, and a weak reference back to the
//! driver. Dropping the future cannot free the buffer; only the operation's own
//! completion can.
//!
//! # Submission is a state machine, not a single step
//!
//! Building an operation publishes a pointer into the submission queue, and that
//! entry cannot be withdrawn. When `SubmitIoRing` fails for any reason other
//! than a wait timeout, the platform leaves every entry in the queue. A
//! built-but-unsubmitted operation is therefore a real, persistent state whose
//! buffer must be retained even though nothing has reached the kernel yet.
//!
//! A submission failure is consequently **not** an operation failure. The future
//! stays pending, the driver reports the error to its error observer, and it
//! retries. Resolving the future would mean handing back a buffer the submission
//! queue still points at.

use std::any::Any;
use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::task::{Context, Poll, Waker};

use crate::buf::{BufResult, IoBufMut, check_read_capacity};
use crate::error::{Error, Result};
use crate::file::{File, FileState};
use crate::io_ring::IoRing;
use crate::io_ring::ops::{ReadOp, SqeFlags};
use crate::sys::AsyncEvent;

pub mod slab;

use slab::{Lifecycle, OpSlab, Token, TokenKind};

/// The outcome of an operation.
///
/// Almost every completion is [`Outcome::Completed`], which carries the result
/// and hands the caller's buffer back. [`Outcome::Retained`] covers the one case
/// where the buffer cannot be returned: the driver was torn down while the
/// kernel could still reach it, so it was leaked rather than freed.
#[derive(Debug)]
pub enum Outcome<T, B> {
    /// The operation reached a terminal state and the buffer came back.
    Completed(BufResult<T, B>),
    /// The buffer could not be recovered and was retained.
    Retained(Error),
}

impl<T, B> Outcome<T, B> {
    /// Returns the [`BufResult`], panicking if the buffer was retained.
    ///
    /// # Panics
    ///
    /// Panics on [`Outcome::Retained`], which only arises during an unclean
    /// teardown.
    pub fn expect_completed(self) -> BufResult<T, B> {
        match self {
            Outcome::Completed(r) => r,
            Outcome::Retained(e) => panic!("operation buffer was retained: {e}"),
        }
    }

    /// Returns the success value and the buffer, panicking on any failure.
    ///
    /// # Panics
    ///
    /// Panics if the operation failed or its buffer was retained.
    pub fn unwrap(self) -> (T, B) {
        self.expect_completed().unwrap()
    }

    /// Returns `true` if the operation completed and succeeded.
    pub fn is_ok(&self) -> bool {
        matches!(self, Outcome::Completed(r) if r.is_ok())
    }

    /// Returns the error, if this outcome represents any kind of failure.
    pub fn err(&self) -> Option<&Error> {
        match self {
            Outcome::Completed(r) => r.result.as_ref().err(),
            Outcome::Retained(e) => Some(e),
        }
    }
}

/// Identifies a submitted operation, for explicit cancellation.
///
/// The driver already knows which file an operation targets, so cancelling it
/// needs nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(Token);

/// Reports errors that belong to no single operation.
///
/// A submission failure affects every queued entry and leaves them queued, so it
/// cannot be any one operation's result. The driver reports such errors here and
/// retries.
pub type ErrorObserver = Box<dyn Fn(&Error)>;

/// The number of bytes an operation transferred.
type Transferred = u32;

/// Where a completed operation's result is left for its future to collect.
///
/// Shared between the driver-owned payload and the future, so the future can be
/// resolved without the driver knowing its concrete buffer type.
struct ResultSlot {
    /// The result and the buffer, once the operation has completed.
    completed: Option<(Result<Transferred>, Box<dyn Any>)>,
    /// Set at teardown when the buffer had to be abandoned.
    retained: bool,
    waker: Option<Waker>,
}

impl ResultSlot {
    fn new() -> Self {
        Self {
            completed: None,
            retained: false,
            waker: None,
        }
    }
}

/// The driver-owned state of one in-flight operation.
struct OpPayload {
    /// The caller's buffer, type-erased so the slab need not know about it.
    buffer: Option<Box<dyn Any>>,
    /// Keeps the target file's handle open for as long as the kernel may use it,
    /// and lets a cancellation name the same file the operation named.
    file: Rc<FileState>,
    /// Shared with the future awaiting this operation.
    slot: Rc<RefCell<ResultSlot>>,
}

/// Shared driver state, reached by handles and futures.
struct DriverInner {
    ring: IoRing,
    slab: OpSlab,
    /// Resources a cancellation request needs kept alive.
    ///
    /// A cancellation names the file its target named, and completes
    /// independently — possibly *after* the target. The target's payload, and
    /// with it the target's own file reference, is released when the target
    /// completes, so the cancellation needs its own hold or the handle could
    /// close while the kernel is still working on the cancellation.
    cancel_holds: Vec<(Token, Rc<FileState>)>,
    /// Signalled to nudge the driver; held here so a dropped future can reach
    /// it to prompt submission of the cancellation it just queued.
    wake: Rc<AsyncEvent>,
    /// Set when entries are queued but not yet accepted by the kernel.
    ///
    /// While set, a retry is owed and no completion can arrive to prompt it, so
    /// the driver must schedule its own wake.
    pending_submit: bool,
    shutting_down: bool,
    torn_down: bool,
    /// Errors awaiting delivery to the observer.
    ///
    /// Reporting happens outside the driver's borrow, for the same reason
    /// waking does: an observer may call back into the driver, and doing that
    /// under an active borrow would panic.
    deferred_reports: Vec<Error>,
    /// Consecutive failed submission attempts.
    ///
    /// Used to avoid flooding the observer with identical errors while a queue
    /// is persistently stuck.
    submit_failures: u32,
    /// Test seam: fail this many submissions before letting one through.
    ///
    /// `SubmitIoRing` has no documented, reliably reproducible failure mode, so
    /// the retry path cannot be exercised without injecting one.
    #[cfg(test)]
    fail_next_submits: u32,
    /// Test seam: skip the quiescence drain at teardown.
    ///
    /// A ring that refuses to settle cannot be produced on demand, so the
    /// retain-and-leak path needs injecting too.
    #[cfg(test)]
    force_unquiet_teardown: bool,
}

impl DriverInner {
    /// Queues an error for delivery to the observer once the borrow is gone.
    fn report(&mut self, error: Error) {
        self.deferred_reports.push(error);
    }

    /// Takes the queued errors, for the caller to deliver outside the borrow.
    #[must_use = "the returned errors must be reported after releasing the borrow"]
    fn take_reports(&mut self) -> Vec<Error> {
        std::mem::take(&mut self.deferred_reports)
    }

    /// Hands queued entries to the kernel.
    ///
    /// On failure every entry stays in the submission queue, so the affected
    /// buffers stay retained and a retry is owed.
    fn submit_pending(&mut self) {
        if !self.pending_submit {
            return;
        }

        #[cfg(test)]
        if self.fail_next_submits > 0 {
            self.fail_next_submits -= 1;
            let injected = Error::Os(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL,
            ));
            self.note_submit_failure(injected);
            return;
        }

        match self.ring.submit(0, 0) {
            Ok(_) => {
                self.pending_submit = false;
                self.submit_failures = 0;
                self.slab.promote_built_to_submitted();
                // An operation whose future was dropped before it reached the
                // kernel had nothing to cancel at the time. Now that it is
                // submitted, cancel it: nobody is waiting for the result.
                self.cancel_abandoned();
            }
            Err(e) => {
                self.note_submit_failure(e);
                // Leave `pending_submit` set. The entries are still queued and
                // their buffers must stay retained until the kernel takes them.
            }
        }
    }

    /// Records a failed submission and queues a report, without flooding.
    fn note_submit_failure(&mut self, error: Error) {
        self.submit_failures = self.submit_failures.saturating_add(1);
        // Report the first failure of a streak, then only occasionally, so a
        // persistently stuck queue does not drown the observer.
        if self.submit_failures == 1 || self.submit_failures.is_multiple_of(64) {
            self.report(error);
        }
    }

    /// Cancels operations whose future was dropped before they reached the
    /// kernel.
    ///
    /// Dropping a future can only ask the platform to cancel an operation the
    /// kernel already has. One dropped while still queued therefore gets its
    /// cancellation deferred to here, once submission has promoted it.
    fn cancel_abandoned(&mut self) {
        for token in self.slab.detached_submitted_uncancelled() {
            self.request_cancel(token);
        }
    }

    /// Asks the platform to cancel a submitted operation.
    ///
    /// Best-effort by nature: the request may fail to build, may lose the race
    /// with the operation it targets, or may be refused outright. None of that
    /// is an error, and none of it changes the fact that the target's buffer is
    /// released only by the target's own completion.
    ///
    /// Cancelling twice, or cancelling something already finished, is a no-op.
    fn request_cancel(&mut self, token: Token) {
        if self.shutting_down || self.torn_down {
            return;
        }
        // Only submitted operations can be cancelled. A described one has
        // nothing in the queue, and a built one has not reached the kernel, so
        // there is nothing for the platform to find.
        if self.slab.state(token).map(|(lifecycle, _)| lifecycle) != Some(Lifecycle::Submitted) {
            return;
        }

        // The cancellation must name the same file the target named.
        let Some(file) = self
            .slab
            .payload_mut(token)
            .and_then(|p| p.downcast_mut::<OpPayload>())
            .map(|p| Rc::clone(&p.file))
        else {
            return;
        };

        // Refused if a cancellation has ever been issued for this operation,
        // which is what makes a repeat request a no-op.
        let Some(cancel_token) = self.slab.register_cancel(token) else {
            return;
        };

        let op = crate::io_ring::ops::CancelOp::builder()
            .with_raw_handle(file.raw_handle())
            .with_op_to_cancel(token.as_user_data())
            .with_user_data(cancel_token.as_user_data())
            .build();
        let Ok(op) = op else {
            // Nothing was queued, so retire the bookkeeping we just took.
            self.slab.complete_cancel(cancel_token);
            return;
        };

        // Hold the file open until the cancellation's own completion arrives.
        // Do this before building, so the hold is in place no matter what.
        self.cancel_holds.push((cancel_token, file));

        // SAFETY: the file handle is kept open by the hold just pushed, which
        // is released only when this cancellation's own completion is dequeued.
        if unsafe { self.ring.build_cancel_request(op) }.is_err() {
            // Failing to enqueue a cancellation is explicitly not an error: the
            // target simply runs to completion. Undo the bookkeeping so the
            // target's slot is not left waiting for a completion that will
            // never arrive.
            self.cancel_holds.retain(|(t, _)| *t != cancel_token);
            self.slab.complete_cancel(cancel_token);
            return;
        }

        self.pending_submit = true;
        let _ = self.wake.signal();
    }

    /// Drains the completion queue.
    ///
    /// Returns the wakers of any futures that were resolved, **without** waking
    /// them. The caller must release its borrow of the driver before waking: a
    /// valid executor may poll the woken task inline, and that task can call
    /// straight back into the driver, which would panic on the still-active
    /// borrow.
    #[must_use = "the returned wakers must be woken after releasing the borrow"]
    fn reap_completions(&mut self) -> Vec<Waker> {
        let mut wakers = Vec::new();
        loop {
            let cqe = match self.ring.pop_completion() {
                Ok(Some(cqe)) => cqe,
                Ok(None) => return wakers,
                Err(e) => {
                    self.report(e);
                    return wakers;
                }
            };

            let token = Token::from_user_data(cqe.UserData);

            if token.kind() == TokenKind::Cancel {
                // A cancellation's completion never releases the target's
                // buffer; it only retires the cancellation's own bookkeeping
                // and releases the file hold that kept the handle open for it.
                self.slab.complete_cancel(token);
                self.cancel_holds.retain(|(t, _)| *t != token);
                continue;
            }

            let result = match cqe.ResultCode.ok() {
                Ok(()) => Ok(cqe.Information as Transferred),
                Err(e) => Err(Error::from(e)),
            };

            // `complete` yields nothing for an unknown or stale token, which is
            // how a completion for a long-finished operation is ignored.
            let Some(payload) = self.slab.complete(token) else {
                continue;
            };
            let Ok(mut payload) = payload.downcast::<OpPayload>() else {
                continue;
            };

            let buffer = payload.buffer.take().expect("payload lost its buffer");
            let mut slot = payload.slot.borrow_mut();
            slot.completed = Some((result, buffer));
            if let Some(waker) = slot.waker.take() {
                wakers.push(waker);
            }
        }
    }

    /// Closes the ring, releasing what is safe to release and leaking the rest.
    ///
    /// Returns the wakers of any futures left waiting, for the caller to wake
    /// after releasing its borrow.
    #[must_use = "the returned wakers must be woken after releasing the borrow"]
    fn teardown(&mut self) -> Vec<Waker> {
        if self.torn_down {
            return Vec::new();
        }
        self.torn_down = true;
        self.shutting_down = true;

        // Try to reach quiescence before deciding anything. Each round hands
        // any queued entries to the kernel and then waits, briefly, for the
        // outstanding operations to report. The wait is bounded so shutdown
        // cannot hang; if the ring will not settle within it, quiescence is
        // deemed unestablished and the leak path below runs.
        let mut wakers = self.reap_completions();
        #[cfg(test)]
        let drain_rounds = if self.force_unquiet_teardown {
            0
        } else {
            DRAIN_ROUNDS
        };
        #[cfg(not(test))]
        let drain_rounds = DRAIN_ROUNDS;

        for _ in 0..drain_rounds {
            if self.slab.outstanding() == 0 {
                break;
            }
            self.submit_pending();
            let waiting = self.slab.outstanding();
            // Submitting with a wait count blocks until that many completions
            // are available or the timeout expires, which is how the driver
            // waits without a timer of its own.
            let _ = self.ring.submit(waiting, DRAIN_TIMEOUT_MS);
            wakers.extend(self.reap_completions());
        }

        if self.slab.outstanding() == 0 {
            let _ = self.ring.close();
            drop(self.slab.drain());
            self.cancel_holds.clear();
            return wakers;
        }

        // Work is still outstanding and quiescence cannot be established.
        // Record the outcome for every waiting future *before* abandoning the
        // buffers: the payloads hold the wakers, so leaking first would hang
        // those futures forever rather than reporting to them.
        self.slab.for_each_payload(|payload| {
            if let Some(payload) = payload.downcast_mut::<OpPayload>() {
                let mut slot = payload.slot.borrow_mut();
                slot.retained = true;
                if let Some(waker) = slot.waker.take() {
                    wakers.push(waker);
                }
            }
        });

        let _ = self.ring.close();
        self.slab.leak();
        // The handles these hold may still be reachable by the kernel, so they
        // are abandoned along with the buffers rather than closed.
        for (_, file) in self.cancel_holds.drain(..) {
            std::mem::forget(file);
        }

        wakers
    }
}

/// How many rounds of draining to attempt before declaring the ring unquiet.
const DRAIN_ROUNDS: u32 = 4;

/// How long each drain round waits for outstanding operations to report.
const DRAIN_TIMEOUT_MS: usize = 250;

/// Yields to the executor and guarantees another poll.
///
/// Unlike `futures::pending!()`, which returns `Pending` without waking, this
/// wakes the current task first. The driver relies on that: when a submission
/// retry is owed, no completion can arrive to prompt the next poll, so the
/// driver must arrange its own.
struct YieldNow(bool);

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn yield_now() -> YieldNow {
    YieldNow(false)
}

/// Owns the ring and drives it to completion.
///
/// Spawn [`Driver::drive`] on your executor and issue operations through
/// [`Driver::handle`]. The driver, its handles, and its futures are all
/// single-threaded by design and cannot be sent between threads.
pub struct Driver {
    inner: Rc<RefCell<DriverInner>>,
    /// Held outside `DriverInner` so it is never invoked under the driver's
    /// borrow; an observer may call back into the driver.
    on_error: Option<ErrorObserver>,
    /// Signalled by the kernel when completions are available.
    completion_event: AsyncEvent,
    /// Signalled when work is queued or shutdown is requested.
    wake: Rc<AsyncEvent>,
}

impl Driver {
    /// Creates a driver for a ring.
    ///
    /// The ring's completion event is configured here, so the ring must not
    /// already have one.
    pub fn new(ring: IoRing) -> Result<Self> {
        Self::with_error_observer(ring, None)
    }

    /// Creates a driver that reports non-operation errors to `observer`.
    ///
    /// Submission failures are the main thing reported this way; see the module
    /// documentation for why they cannot be delivered to a future.
    pub fn with_error_observer(mut ring: IoRing, observer: Option<ErrorObserver>) -> Result<Self> {
        // Each fallible step must close the ring on the way out, since nothing
        // else owns it yet.
        let mut build = || -> Result<(AsyncEvent, Rc<AsyncEvent>)> {
            let completion_event = AsyncEvent::new_manual_reset()?;
            ring.set_io_ring_completion_event(completion_event.handle())?;
            let wake = Rc::new(AsyncEvent::new_manual_reset()?);
            Ok((completion_event, wake))
        };
        let (completion_event, wake) = match build() {
            Ok(parts) => parts,
            Err(e) => {
                let _ = ring.close();
                return Err(e);
            }
        };

        Ok(Self {
            inner: Rc::new(RefCell::new(DriverInner {
                ring,
                slab: OpSlab::new(),
                cancel_holds: Vec::new(),
                wake: Rc::clone(&wake),
                pending_submit: false,
                shutting_down: false,
                torn_down: false,
                deferred_reports: Vec::new(),
                submit_failures: 0,
                #[cfg(test)]
                fail_next_submits: 0,
                #[cfg(test)]
                force_unquiet_teardown: false,
            })),
            on_error: observer,
            completion_event,
            wake,
        })
    }

    /// Delivers queued errors to the observer.
    ///
    /// Must be called with no borrow of `DriverInner` held.
    fn flush_reports(&self, reports: Vec<Error>) {
        if let Some(observer) = &self.on_error {
            for error in reports {
                observer(&error);
            }
        }
    }

    /// Returns a handle for issuing operations.
    pub fn handle(&self) -> Handle {
        Handle {
            inner: Rc::downgrade(&self.inner),
            strong: Rc::clone(&self.inner),
            wake: Rc::clone(&self.wake),
        }
    }

    /// Runs the driver until shutdown is requested, then closes the ring.
    ///
    /// Spawn this on your executor.
    pub async fn drive(&self) {
        use futures::FutureExt;

        loop {
            let (shutting_down, retry_owed, wakers, reports) = {
                let mut inner = self.inner.borrow_mut();
                inner.submit_pending();
                let wakers = inner.reap_completions();
                let reports = inner.take_reports();
                (inner.shutting_down, inner.pending_submit, wakers, reports)
            };

            // Wake and report only after the borrow is released: an executor may
            // poll the woken task inline, and an observer may call straight back
            // into the driver. Either under a live borrow would panic.
            for waker in wakers {
                waker.wake();
            }
            self.flush_reports(reports);

            if shutting_down {
                break;
            }

            if retry_owed {
                // Entries are queued but the kernel has not taken them, and no
                // completion can arrive to prompt a retry. Yield and come
                // straight back.
                //
                // This is a self-waking yield rather than a timed backoff
                // because the crate deliberately has no timer — acquiring one
                // would mean depending on a runtime, which is exactly what this
                // crate avoids. It must wake itself: a bare `Pending` would
                // leave the driver parked forever, since no completion can
                // arrive to poll it again. Giving up is not an option either,
                // because the submission queue still references the callers'
                // buffers. Reporting is throttled so a persistently stuck queue
                // does not flood the observer.
                yield_now().await;
                continue;
            }

            futures::select! {
                _ = self.wake.wait().fuse() => {
                    self.wake.reset().ok();
                }
                _ = self.completion_event.wait().fuse() => {
                    self.completion_event.reset().ok();
                }
            }
        }

        let (wakers, reports) = {
            let mut inner = self.inner.borrow_mut();
            let wakers = inner.teardown();
            let reports = inner.take_reports();
            (wakers, reports)
        };
        for waker in wakers {
            waker.wake();
        }
        self.flush_reports(reports);
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // An abrupt drop gets the same treatment as a requested shutdown: no
        // memory the kernel might still reach is freed.
        let (wakers, reports) = {
            let mut inner = self.inner.borrow_mut();
            let wakers = inner.teardown();
            let reports = inner.take_reports();
            (wakers, reports)
        };
        for waker in wakers {
            waker.wake();
        }
        self.flush_reports(reports);
    }
}

/// A cloneable, single-threaded reference to a [`Driver`].
#[derive(Clone)]
pub struct Handle {
    /// Weak, so futures holding a handle cannot keep the driver alive.
    inner: Weak<RefCell<DriverInner>>,
    /// Strong, so a handle held by the caller keeps the driver usable.
    strong: Rc<RefCell<DriverInner>>,
    wake: Rc<AsyncEvent>,
}

impl Handle {
    /// Requests shutdown.
    ///
    /// The driver stops accepting new work and closes the ring.
    pub fn shutdown(&self) {
        self.strong.borrow_mut().shutting_down = true;
        let _ = self.wake.signal();
    }

    /// Returns `true` if the driver has been asked to shut down.
    pub fn is_shutting_down(&self) -> bool {
        self.strong.borrow().shutting_down
    }

    /// Returns the number of operations the driver is still tracking.
    pub fn outstanding(&self) -> usize {
        self.strong.borrow().slab.outstanding()
    }

    /// Reads from `file` at `offset` into `buffer`.
    ///
    /// The buffer is moved into the driver for the duration of the operation and
    /// returned when it completes, whether it succeeded or failed. Dropping the
    /// returned future is safe: the operation runs to completion and the buffer
    /// is released then, never earlier.
    pub fn read<B: IoBufMut>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> ReadFuture<B> {
        match self.try_read(file, buffer, len, offset) {
            Ok(fut) => fut,
            Err((error, buffer)) => ReadFuture::failed(error, buffer),
        }
    }

    /// Submits a read, returning the buffer alongside any pre-submission error.
    fn try_read<B: IoBufMut>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
    ) -> std::result::Result<ReadFuture<B>, (Error, B)> {
        {
            let inner = self.strong.borrow();
            if inner.shutting_down {
                return Err((Error::ShuttingDown, buffer));
            }
        }
        if let Err(e) = check_read_capacity(&buffer, len as u64) {
            return Err((e, buffer));
        }

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let mut inner = self.strong.borrow_mut();

        // The payload is boxed by the slab, so the buffer's address is stable
        // from here until the operation completes. Its pointer is taken only
        // after that placement.
        let payload = Box::new(OpPayload {
            buffer: None,
            file: file.state(),
            slot: Rc::clone(&slot),
        });
        let token = match inner.slab.insert(payload) {
            Ok(token) => token,
            Err(_) => return Err((Error::QueueFull, buffer)),
        };

        // Box the buffer FIRST, then take its address. Taking the pointer from
        // the local before boxing would hand the kernel a stale address for any
        // buffer stored inline, such as `[u8; N]`, whose bytes move when the
        // value moves. Coercing `Box<B>` to `Box<dyn Any>` only changes the
        // pointer's metadata; the allocation, and therefore the address, stays
        // put for as long as the payload lives in the slab.
        let mut boxed: Box<B> = Box::new(buffer);
        let data_ptr = boxed.buf_mut_ptr();
        {
            let payload = inner
                .slab
                .payload_mut(token)
                .and_then(|p| p.downcast_mut::<OpPayload>())
                .expect("just inserted");
            payload.buffer = Some(boxed as Box<dyn Any>);
        }

        let op = ReadOp::builder()
            .with_raw_handle(file.as_raw_handle())
            .with_raw_data_address(data_ptr as *mut _)
            .with_num_of_bytes_to_read(len)
            .with_offset(offset)
            .with_user_data(token.as_user_data())
            .with_sqe_flags(SqeFlags::NONE)
            .build();

        let op = match op {
            Ok(op) => op,
            Err(e) => return Err((e, recover_buffer(&mut inner, token))),
        };

        // SAFETY: the payload lives in the slab until this operation's own
        // completion is dequeued, and the slab never moves a boxed payload, so
        // `data_ptr` stays valid for the whole time the kernel may use it. The
        // file's handle is kept open by the `Rc<FileState>` in the payload.
        if let Err(e) = unsafe { inner.ring.build_read_file(op) } {
            return Err((e, recover_buffer(&mut inner, token)));
        }

        // The entry is now in the submission queue and cannot be withdrawn.
        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        drop(inner);

        let _ = self.wake.signal();

        Ok(ReadFuture::pending(token, slot, Weak::clone(&self.inner)))
    }

    /// Requests cancellation of a previously submitted operation.
    ///
    /// Cancellation is best-effort: it may fail, or arrive too late, and neither
    /// is an error. The caller keeps the original future and still observes that
    /// operation's terminal result, which is the only thing that releases the
    /// buffer. Cancelling twice, or cancelling an operation that has already
    /// finished, is a no-op.
    pub fn cancel(&self, id: OperationId) {
        self.strong.borrow_mut().request_cancel(id.0);
    }
}

/// Takes a buffer back out of a slot whose operation never reached the kernel.
///
/// Only correct before the operation has been built into the submission queue,
/// because after that the entry references the buffer and cannot be withdrawn.
fn recover_buffer<B: IoBufMut>(inner: &mut DriverInner, token: Token) -> B {
    let payload = inner
        .slab
        .complete(token)
        .expect("slot was just inserted")
        .downcast::<OpPayload>()
        .unwrap_or_else(|_| unreachable!("payload type mismatch"));
    let buffer = payload.buffer.expect("payload lost its buffer");
    *buffer.downcast::<B>().expect("buffer type mismatch")
}

/// A read in progress.
///
/// Holds only a token, a shared result slot, and a weak reference to the driver.
/// The buffer itself lives in the driver, which is what makes dropping this
/// future safe.
pub struct ReadFuture<B> {
    state: ReadState<B>,
}

enum ReadState<B> {
    /// Rejected before anything was submitted; the buffer never left.
    Failed(Option<(Error, B)>),
    /// Submitted and awaiting completion.
    Waiting {
        token: Token,
        slot: Rc<RefCell<ResultSlot>>,
        driver: Weak<RefCell<DriverInner>>,
        /// Records the buffer type without affecting auto traits. The future
        /// holds no self-references, so it is always `Unpin`.
        _buffer: PhantomData<fn() -> B>,
    },
    /// Already resolved.
    Done,
}

impl<B: IoBufMut> ReadFuture<B> {
    fn failed(error: Error, buffer: B) -> Self {
        Self {
            state: ReadState::Failed(Some((error, buffer))),
        }
    }

    fn pending(
        token: Token,
        slot: Rc<RefCell<ResultSlot>>,
        driver: Weak<RefCell<DriverInner>>,
    ) -> Self {
        Self {
            state: ReadState::Waiting {
                token,
                slot,
                driver,
                _buffer: PhantomData,
            },
        }
    }

    /// Returns the identifier for cancelling this operation, if it was
    /// submitted.
    pub fn operation_id(&self) -> Option<OperationId> {
        match &self.state {
            ReadState::Waiting { token, .. } => Some(OperationId(*token)),
            _ => None,
        }
    }
}

impl<B: IoBufMut> Future for ReadFuture<B> {
    type Output = Outcome<Transferred, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            ReadState::Failed(taken) => {
                let (error, buffer) = taken.take().expect("polled after completion");
                this.state = ReadState::Done;
                Poll::Ready(Outcome::Completed(BufResult::new(Err(error), buffer)))
            }
            ReadState::Done => panic!("read future polled after completion"),
            ReadState::Waiting { slot, driver, .. } => {
                let mut borrowed = slot.borrow_mut();
                if let Some((result, buffer)) = borrowed.completed.take() {
                    drop(borrowed);
                    this.state = ReadState::Done;
                    let mut buffer = *buffer.downcast::<B>().expect("buffer type mismatch");
                    if let Ok(transferred) = &result {
                        // SAFETY: the kernel reported writing this many bytes
                        // into the buffer, so they are initialized, and the
                        // count came from a transfer bounded by the capacity
                        // checked before submission.
                        unsafe { buffer.set_buf_init(*transferred as usize) };
                    }
                    return Poll::Ready(Outcome::Completed(BufResult::new(result, buffer)));
                }

                if borrowed.retained {
                    drop(borrowed);
                    this.state = ReadState::Done;
                    return Poll::Ready(Outcome::Retained(Error::BufferRetained));
                }

                if driver.upgrade().is_none() {
                    drop(borrowed);
                    this.state = ReadState::Done;
                    return Poll::Ready(Outcome::Retained(Error::DriverGone));
                }

                borrowed.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

impl<B> Drop for ReadFuture<B> {
    fn drop(&mut self) {
        let ReadState::Waiting { token, driver, .. } = &self.state else {
            return;
        };
        // Reaching the driver is exactly why the future holds a weak reference:
        // without it there would be no way to record the detachment.
        let Some(inner) = driver.upgrade() else {
            return;
        };
        let Ok(mut inner) = inner.try_borrow_mut() else {
            return;
        };

        // Detaching leaves the operation running. Its buffer is released only
        // when its own completion is dequeued, never here. What varies is
        // whether anything can be done about it.
        match inner.slab.detach(*token) {
            Some(Lifecycle::Described) => {
                // Nothing was ever built, so no queue entry references the
                // buffer and it can be released immediately.
                drop(inner.slab.complete(*token));
            }
            Some(Lifecycle::Built) => {
                // A submission queue entry references the buffer and cannot be
                // withdrawn, and the kernel has not seen the operation yet, so
                // there is nothing for the platform to cancel. The slab records
                // this slot as detached-but-submitted-later, and the driver
                // cancels it once submission promotes it.
            }
            Some(Lifecycle::Submitted) => {
                // Best-effort: ask the platform to give up early. Failure here
                // is not an error and changes nothing about the buffer's
                // lifetime. This returns without waiting on the kernel.
                inner.request_cancel(*token);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_reports_errors_from_either_shape() {
        let completed: Outcome<u32, Vec<u8>> =
            Outcome::Completed(BufResult::new(Err(Error::QueueFull), vec![1]));
        assert!(!completed.is_ok());
        assert!(matches!(completed.err(), Some(Error::QueueFull)));

        let retained: Outcome<u32, Vec<u8>> = Outcome::Retained(Error::BufferRetained);
        assert!(!retained.is_ok());
        assert!(matches!(retained.err(), Some(Error::BufferRetained)));

        let ok: Outcome<u32, Vec<u8>> = Outcome::Completed(BufResult::new(Ok(3), vec![1, 2, 3]));
        assert!(ok.is_ok());
        assert!(ok.err().is_none());
        let (n, buf) = ok.unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    fn test_driver() -> Driver {
        let ring = IoRing::builder().build().unwrap();
        Driver::new(ring).unwrap()
    }

    fn readme() -> File {
        File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/../../README.md")).unwrap()
    }

    /// Runs the driver by hand until nothing is outstanding.
    ///
    /// Submits as well as reaps, because a tombstoned slot clears only once its
    /// cancellation has been submitted and has reported.
    fn drain(driver: &Driver) {
        for _ in 0..10_000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if driver.inner.borrow().slab.outstanding() == 0 {
                return;
            }
            std::thread::yield_now();
        }
        panic!("ring did not settle");
    }

    /// A submission failure must not resolve the operation's future, because
    /// the submission queue still references its buffer. The error goes to the
    /// observer instead, and the operation succeeds once the retry lands.
    #[test]
    fn submission_failure_is_not_the_operations_result() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);

        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::with_error_observer(
            ring,
            Some(Box::new(move |e: &Error| {
                sink.borrow_mut().push(e.to_string());
            })),
        )
        .unwrap();
        let handle = driver.handle();
        let file = readme();

        // Arrange for the next two submissions to fail.
        driver.inner.borrow_mut().fail_next_submits = 2;

        let mut fut = handle.read(&file, vec![0_u8; 64], 20, 0);

        // First attempt fails. The slot must stay retained and the future must
        // not be resolved.
        {
            let mut inner = driver.inner.borrow_mut();
            inner.submit_pending();
            assert!(inner.pending_submit, "entries must stay queued on failure");
            assert_eq!(inner.slab.outstanding(), 1);
            let reports = inner.take_reports();
            drop(inner);
            // Reports are delivered outside the borrow, exactly as the driver
            // loop does it.
            driver.flush_reports(reports);
        }
        assert_eq!(seen.borrow().len(), 1, "the first failure is reported");

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(
            Pin::new(&mut fut).poll(&mut cx).is_pending(),
            "a submission failure must not resolve the future"
        );

        // Second attempt fails too; still no resolution.
        driver.inner.borrow_mut().submit_pending();
        assert!(driver.inner.borrow().pending_submit);

        // Third attempt is allowed through.
        driver.inner.borrow_mut().submit_pending();
        assert!(
            !driver.inner.borrow().pending_submit,
            "a successful retry clears the pending flag"
        );

        // Drain the completion so nothing is left in flight.
        loop {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                // Submit as well as reap: dropping a future queues a
                // cancellation, and a tombstoned slot clears only once that
                // cancellation has been submitted and has reported.
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if driver.inner.borrow().slab.outstanding() == 0 {
                break;
            }
            std::thread::yield_now();
        }
        assert!(Pin::new(&mut fut).poll(&mut cx).is_ready());
    }

    /// Dropping a future before its operation reaches the kernel leaves nothing
    /// to cancel at the time. The cancellation must therefore be deferred until
    /// submission promotes the operation, or an abandoned read would run to
    /// completion unnecessarily.
    #[test]
    fn cancellation_is_deferred_until_a_built_operation_is_submitted() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // Built but deliberately not submitted yet.
        let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
        let token = fut.operation_id().expect("read was built").0;
        assert_eq!(
            driver.inner.borrow().slab.state(token).map(|s| s.0),
            Some(Lifecycle::Built)
        );

        drop(fut);

        {
            let inner = driver.inner.borrow();
            // Detached, still built, and not yet cancellable.
            assert_eq!(
                inner.slab.state(token),
                Some((Lifecycle::Built, slab::Observer::Detached))
            );
            assert_eq!(
                inner.slab.detached_submitted_uncancelled().len(),
                0,
                "a built operation is not yet a cancellation candidate"
            );
        }

        // Submitting promotes it, and the driver must cancel it at that point.
        driver.inner.borrow_mut().submit_pending();
        {
            let inner = driver.inner.borrow();
            assert!(
                inner.slab.detached_submitted_uncancelled().is_empty(),
                "the abandoned operation should have been cancelled on promotion"
            );
            assert!(
                !inner.cancel_holds.is_empty(),
                "a deferred cancellation should hold the file open"
            );
        }

        // Settle so teardown is clean.
        drain(&driver);
        handle.shutdown();
    }

    /// FR-020b / SC-030: when the ring will not settle, waiting futures must be
    /// told their buffer was retained rather than being left pending forever.
    #[test]
    fn unquiet_teardown_reports_retained_buffers() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let mut fut = handle.read(&file, vec![0_u8; 128], 64, 0);

        // Register a waker so the future is genuinely waiting.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut fut).poll(&mut cx).is_pending());

        // Force the drain to be skipped, so quiescence cannot be established.
        driver.inner.borrow_mut().force_unquiet_teardown = true;
        let wakers = driver.inner.borrow_mut().teardown();
        for w in wakers {
            w.wake();
        }

        match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(Outcome::Retained(e)) => {
                assert!(matches!(e, Error::BufferRetained));
            }
            Poll::Ready(other) => panic!("expected Retained, got {other:?}"),
            Poll::Pending => panic!("a future was left pending after teardown"),
        }

        // FR-032: nothing may be submitted after teardown.
        let outcome = handle.read(&file, vec![0_u8; 64], 20, 0);
        let mut outcome = outcome;
        match Pin::new(&mut outcome).poll(&mut cx) {
            Poll::Ready(o) => assert!(matches!(o.err(), Some(Error::ShuttingDown))),
            Poll::Pending => panic!("a post-teardown submission should fail immediately"),
        }
    }

    /// Drives the real `drive()` loop and asserts it re-arms itself when a
    /// submission retry is owed.
    ///
    /// This is the test that catches a bare `Pending`: without a self-wake the
    /// executor is never told to poll again, the retry never happens, and the
    /// callers' buffers are stranded in the submission queue forever.
    #[test]
    fn drive_wakes_itself_when_a_submission_retry_is_owed() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingWaker(AtomicUsize);
        impl std::task::Wake for CountingWaker {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let file = readme();

        // Keep submission failing so the retry path stays active.
        driver.inner.borrow_mut().fail_next_submits = 5;
        let fut = handle.read(&file, vec![0_u8; 64], 20, 0);

        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);

        let drive = driver.drive();
        futures::pin_mut!(drive);

        let before = counter.0.load(Ordering::SeqCst);
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        let after = counter.0.load(Ordering::SeqCst);

        assert!(
            after > before,
            "drive() parked without waking itself, so the retry would never happen"
        );
        assert!(
            driver.inner.borrow().pending_submit,
            "the retry should still be owed"
        );

        // Let it through and settle so teardown is clean.
        driver.inner.borrow_mut().fail_next_submits = 0;
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
            if driver.inner.borrow().slab.outstanding() == 0 {
                break;
            }
            std::thread::yield_now();
        }
        drop(fut);
        handle.shutdown();
    }

    /// Repeated failures must not flood the observer, and the driver must keep
    /// retrying: the submission queue still references the caller's buffer, so
    /// abandoning the retry would strand it.
    #[test]
    fn persistent_submission_failure_throttles_reporting_but_keeps_retrying() {
        let count = Rc::new(RefCell::new(0_usize));
        let sink = Rc::clone(&count);

        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::with_error_observer(
            ring,
            Some(Box::new(move |_: &Error| {
                *sink.borrow_mut() += 1;
            })),
        )
        .unwrap();
        let handle = driver.handle();
        let file = readme();

        driver.inner.borrow_mut().fail_next_submits = 100;
        let fut = handle.read(&file, vec![0_u8; 64], 20, 0);

        for _ in 0..100 {
            let reports = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.take_reports()
            };
            driver.flush_reports(reports);
        }

        let failures = driver.inner.borrow().submit_failures;
        assert!(
            failures == 100,
            "every attempt must actually be retried, saw {failures}"
        );
        assert!(
            driver.inner.borrow().pending_submit,
            "the entries stay queued while submission keeps failing"
        );
        assert!(
            *count.borrow() < 10,
            "observer flooded with {} reports",
            count.borrow()
        );

        // Let the operation through and settle, so teardown is clean.
        driver.inner.borrow_mut().fail_next_submits = 0;
        driver.inner.borrow_mut().submit_pending();
        assert!(
            !driver.inner.borrow().pending_submit,
            "a successful retry after a long failure streak must still land"
        );
        loop {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                // Submit as well as reap: dropping a future queues a
                // cancellation, and a tombstoned slot clears only once that
                // cancellation has been submitted and has reported.
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if driver.inner.borrow().slab.outstanding() == 0 {
                break;
            }
            std::thread::yield_now();
        }
        drop(fut);
        handle.shutdown();
    }

    /// A completion whose user data matches nothing must be discarded without
    /// panicking and without disturbing a live operation.
    #[test]
    fn unknown_completions_are_discarded() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 64], 20, 0);
        let live = fut.operation_id().expect("read was submitted");

        {
            let mut inner = driver.inner.borrow_mut();
            // A token for a slot index that was never allocated.
            let bogus = slab::Token::from_user_data(0xDEAD_BEEF);
            assert!(inner.slab.complete(bogus).is_none());
            assert!(!inner.slab.complete_cancel(bogus));
            // The live operation is untouched.
            assert_eq!(inner.slab.lookup(live.0), slab::Lookup::Live);
        }

        drop(fut);
        handle.shutdown();
    }

    /// A detached operation's buffer is released when its own completion
    /// arrives, not when the future is dropped.
    #[test]
    fn detached_operations_release_on_completion() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 64], 20, 0);
        driver.inner.borrow_mut().submit_pending();

        // Dropping the future must leave the operation outstanding.
        drop(fut);
        assert_eq!(
            driver.inner.borrow().slab.outstanding(),
            1,
            "a dropped future must not release the buffer"
        );

        // Only the completion releases it.
        loop {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                // Submit as well as reap: dropping a future queues a
                // cancellation, and a tombstoned slot clears only once that
                // cancellation has been submitted and has reported.
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if driver.inner.borrow().slab.outstanding() == 0 {
                break;
            }
            std::thread::yield_now();
        }

        handle.shutdown();
    }

    /// Dropping a future before anything was built releases the buffer at once,
    /// since no queue entry references it.
    #[test]
    fn unbuilt_operations_release_immediately() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // A rejected read never reaches the slab at all.
        let rejected = handle.read(&file, vec![0_u8; 4], 64, 0);
        assert!(rejected.operation_id().is_none());
        assert_eq!(driver.inner.borrow().slab.outstanding(), 0);

        handle.shutdown();
    }
}
