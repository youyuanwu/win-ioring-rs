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

use crate::buf::{BufResult, IoBuf, IoBufMut, check_read_capacity, check_write_initialized};
use crate::error::{Error, Result};
use crate::file::{File, FileState, SequentialGuard};
use crate::io_ring::IoRing;
use crate::io_ring::ops::{ReadOp, SqeFlags};
use crate::sys::AsyncEvent;

use windows::Win32::Storage::FileSystem::{
    FILE_FLUSH_DEFAULT, FILE_FLUSH_MODE, FILE_WRITE_FLAGS, FILE_WRITE_FLAGS_NONE, IORING_OP_FLUSH,
    IORING_OP_READ, IORING_OP_REGISTER_BUFFERS, IORING_OP_REGISTER_FILES, IORING_OP_WRITE,
};

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

/// A completed operation's result, plus the buffer if it carried one.
///
/// Flush carries no caller buffer, which is why the buffer is optional rather
/// than something every completion must produce.
type CompletedOp = (Result<Transferred>, Option<Box<dyn Any>>);

/// Where a completed operation's result is left for its future to collect.
///
/// Shared between the driver-owned payload and the future, so the future can be
/// resolved without the driver knowing its concrete buffer type.
struct ResultSlot {
    /// The result and, for operations that carry one, the buffer.
    completed: Option<CompletedOp>,
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
    /// Keeps the target file's handle open for as long as the kernel may use
    /// it, and lets a cancellation name the same file the operation named.
    ///
    /// Absent for registrations, which name no single file.
    file: Option<Rc<FileState>>,
    /// Shared with the future awaiting this operation.
    slot: Rc<RefCell<ResultSlot>>,
    /// The registered buffer this operation targets, if any.
    ///
    /// Records the registration's generation alongside the index and offset, so
    /// a completion belonging to a superseded registration cannot disturb the
    /// current one's watermarks.
    registered_buffer: Option<(u64, u32, u32)>,
    /// Whether this operation names a registered file handle.
    ///
    /// Recorded for diagnostics and to make the registered path explicit at the
    /// point of submission.
    #[allow(dead_code)]
    uses_registered_file: bool,
    /// Resources for a registration that has not yet completed.
    ///
    /// Held here rather than in the awaiting future so that dropping that
    /// future cannot free memory the platform still references.
    pending_registration: Option<PendingRegistration>,
    /// Clears the file's outstanding-sequential flag when this payload is
    /// dropped, which is exactly at terminal completion.
    _sequential: Option<SequentialGuard>,
}

/// A registration that has been built but whose completion has not arrived.
///
/// The resources live here, inside the driver's slab payload, for exactly the
/// same reason operation buffers do: the platform holds pointers into them from
/// the moment the registration is built, and the future awaiting it can be
/// dropped at any time. Keeping them in the future's locals would free them
/// while the platform was still using them.
enum PendingRegistration {
    /// Buffers, their descriptors, and their extents.
    Buffers {
        buffers: Vec<Box<dyn Any>>,
        extents: Vec<usize>,
        watermarks: Vec<usize>,
        descriptors: Box<[crate::io_ring::BufferInfo]>,
    },
    /// File handles, kept open for the platform.
    Files { files: Vec<Rc<FileState>> },
}

/// An active buffer registration.
///
/// Registered buffers live in the driver at stable addresses. The platform
/// provides no way to release a registration, so one lasts until it is
/// superseded by another or the ring is closed.
struct BufferRegistration {
    /// Distinguishes this registration from those it supersedes, so a
    /// completion belonging to an older one cannot disturb it.
    generation: u64,
    /// The caller's buffers, boxed so their addresses are stable. Retained for
    /// as long as the platform may reference them, which is until the ring is
    /// closed.
    _buffers: Vec<Box<dyn Any>>,
    /// The registered extent of each buffer, in bytes.
    extents: Vec<usize>,
    /// How many leading bytes of each buffer are initialized.
    ///
    /// Reads may target the whole extent, but writes are bounded by this, so
    /// uninitialized memory is never sent to the kernel. It is a single
    /// contiguous prefix rather than a set of ranges, so a read that lands past
    /// the current mark leaves a gap and does **not** raise it — otherwise the
    /// gap would be falsely reported as initialized.
    watermarks: Vec<usize>,
    /// The descriptor array handed to the platform, kept at a stable address
    /// for as long as the registration is active.
    _descriptors: Box<[crate::io_ring::BufferInfo]>,
}

/// An active file handle registration.
struct FileRegistration {
    /// Kept so the registered handles stay open for as long as the platform
    /// may use them.
    _files: Vec<Rc<FileState>>,
    /// How many handles were registered, for index validation.
    count: usize,
}

/// Names a buffer for an operation: the caller's own, or a registered one.
pub enum BufferSource<B> {
    /// A buffer owned by the caller, handed to the driver for the operation.
    Owned(B),
    /// A slice of a previously registered buffer.
    Registered {
        /// The registration index.
        index: u32,
        /// The offset within that buffer.
        offset: u32,
    },
}

/// Names the file an operation targets.
pub enum FileTarget<'a> {
    /// A file the caller owns.
    Owned(&'a File),
    /// A previously registered file handle.
    Registered {
        /// The registration index.
        index: u32,
    },
}

/// The result of a registration attempt.
///
/// On success the driver takes ownership of the resources, because the platform
/// keeps pointers to them until the registration is superseded or the ring
/// closes. On failure they come straight back.
pub enum Registered<T> {
    /// The registration succeeded and the driver now owns the resources.
    Ok,
    /// The registration failed; the caller's resources are returned.
    Failed(Error, Vec<T>),
}

impl<T> Registered<T> {
    /// Returns `true` if the registration succeeded.
    pub fn is_ok(&self) -> bool {
        matches!(self, Registered::Ok)
    }

    /// Returns the error, if the registration failed.
    pub fn err(&self) -> Option<&Error> {
        match self {
            Registered::Ok => None,
            Registered::Failed(e, _) => Some(e),
        }
    }

    /// Unwraps a successful registration.
    ///
    /// # Panics
    ///
    /// Panics if the registration failed.
    pub fn unwrap(self) {
        if let Registered::Failed(e, _) = self {
            panic!("registration failed: {e}");
        }
    }
}

/// Shared driver state, reached by handles and futures.
struct DriverInner {
    ring: IoRing,
    slab: OpSlab,
    /// The active buffer registration, if any.
    buffer_registration: Option<BufferRegistration>,
    /// Increments with each adopted buffer registration, so a completion can
    /// tell whether the registration it named is still the current one.
    registration_generation: u64,
    /// Buffer registrations that have been superseded but are still referenced.
    ///
    /// Their resources return to the caller once the last operation using them
    /// completes.
    retired_buffer_registrations: Vec<BufferRegistration>,
    /// The active file handle registration, if any.
    file_registration: Option<FileRegistration>,
    /// Superseded file registrations still referenced by in-flight operations.
    retired_file_registrations: Vec<FileRegistration>,
    /// Resources a cancellation request needs kept alive.
    ///
    /// A cancellation names the file its target named, and completes
    /// independently — possibly *after* the target. The target's payload, and
    /// with it the target's own file reference, is released when the target
    /// completes, so the cancellation needs its own hold or the handle could
    /// close while the kernel is still working on the cancellation.
    cancel_holds: Vec<(Token, Rc<FileState>)>,
    /// Cancellations requested before their operation reached the kernel.
    ///
    /// The platform can only cancel something it already has, so a request made
    /// while the operation is still described or built is remembered here and
    /// issued once submission promotes it.
    deferred_cancels: Vec<Token>,
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
    /// Test seam: build the next buffer registration with no descriptors.
    ///
    /// The platform accepts a zero-*extent* descriptor but rejects a
    /// zero-*count* registration, and it does so at completion time rather than
    /// at build time. That is the only way to reach the completion-failure path,
    /// and the public API refuses an empty registration up front, so it has to
    /// be injected here.
    #[cfg(test)]
    fail_next_registration: bool,
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

    /// Installs a completed registration, superseding any predecessor.
    ///
    /// A superseded registration is retained rather than released: the platform
    /// offers no way to withdraw it, and operations issued against it may still
    /// be outstanding. Its resources are freed when the driver is torn down.
    fn adopt_registration(&mut self, pending: PendingRegistration) {
        match pending {
            PendingRegistration::Buffers {
                buffers,
                extents,
                watermarks,
                descriptors,
            } => {
                self.registration_generation += 1;
                let previous = self.buffer_registration.replace(BufferRegistration {
                    generation: self.registration_generation,
                    _buffers: buffers,
                    extents,
                    watermarks,
                    _descriptors: descriptors,
                });
                if let Some(previous) = previous {
                    self.retired_buffer_registrations.push(previous);
                }
            }
            PendingRegistration::Files { files } => {
                let previous = self.file_registration.replace(FileRegistration {
                    count: files.len(),
                    _files: files,
                });
                if let Some(previous) = previous {
                    self.retired_file_registrations.push(previous);
                }
            }
        }
    }

    /// Validates a registered buffer reference.
    ///
    /// Reads may target the buffer's whole registered extent; writes are
    /// bounded by how much of it is initialized, so uninitialized memory is
    /// never handed to the kernel.
    fn check_registered_buffer(
        &self,
        index: u32,
        offset: u32,
        len: u32,
        for_write: bool,
    ) -> Result<()> {
        let registration = self
            .buffer_registration
            .as_ref()
            .ok_or(Error::InvalidRegisteredIndex { index })?;
        let extent = *registration
            .extents
            .get(index as usize)
            .ok_or(Error::InvalidRegisteredIndex { index })?;
        let bound = if for_write {
            registration.watermarks[index as usize]
        } else {
            extent
        };

        let end = (offset as u64).saturating_add(len as u64);
        if end > bound as u64 {
            return Err(Error::RegisteredRangeOutOfBounds {
                index,
                offset: offset as u64,
                length: len as u64,
                extent: bound as u64,
            });
        }
        Ok(())
    }

    /// Validates a registered file index.
    fn check_registered_file(&self, index: u32) -> Result<()> {
        let registration = self
            .file_registration
            .as_ref()
            .ok_or(Error::InvalidRegisteredIndex { index })?;
        if (index as usize) < registration.count {
            Ok(())
        } else {
            Err(Error::InvalidRegisteredIndex { index })
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

    /// Cancels operations whose cancellation could not be issued earlier.
    ///
    /// Two groups qualify: those explicitly cancelled before the kernel had
    /// them, and those whose future was dropped while they were still queued.
    /// Both had nothing for the platform to act on at the time.
    fn cancel_abandoned(&mut self) {
        for token in std::mem::take(&mut self.deferred_cancels) {
            self.issue_cancel(token);
        }
        for token in self.slab.detached_submitted_uncancelled() {
            self.issue_cancel(token);
        }
    }

    /// Requests cancellation of an operation, now or as soon as possible.
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
        match self.slab.state(token).map(|(lifecycle, _)| lifecycle) {
            Some(Lifecycle::Submitted) => self.issue_cancel(token),
            // The kernel does not have this operation yet, so there is nothing
            // to cancel. Remember the request and honour it on promotion,
            // rather than silently dropping it.
            Some(Lifecycle::Described | Lifecycle::Built)
                if !self.deferred_cancels.contains(&token) =>
            {
                self.deferred_cancels.push(token);
            }
            _ => {}
        }
    }

    /// Builds and queues a cancellation for an operation the kernel has.
    ///
    /// Does nothing if the operation is not, or is no longer, submitted.
    fn issue_cancel(&mut self, token: Token) {
        if self.shutting_down || self.torn_down {
            return;
        }
        // Only submitted operations can be cancelled. A described one has
        // nothing in the queue, and a built one has not reached the kernel, so
        // there is nothing for the platform to find. A token that has since
        // completed lands here too, and is likewise ignored.
        if self.slab.state(token).map(|(lifecycle, _)| lifecycle) != Some(Lifecycle::Submitted) {
            return;
        }

        // The cancellation must name the same file the target named.
        let Some(file) = self
            .slab
            .payload_mut(token)
            .and_then(|p| p.downcast_mut::<OpPayload>())
            .and_then(|p| p.file.clone())
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

            // A flush carries no buffer, so this is legitimately absent.
            let mut buffer = payload.buffer.take();

            // A registration is adopted here, in completion order, rather than
            // by whoever happens to poll the awaiting future. On failure the
            // resources go back to the caller through the result slot.
            if let Some(pending) = payload.pending_registration.take() {
                if result.is_ok() {
                    self.adopt_registration(pending);
                } else {
                    buffer = Some(Box::new(pending) as Box<dyn Any>);
                }
            }

            // A completed read into a registered buffer may raise that buffer's
            // initialization watermark.
            if let Some((generation, index, offset)) = payload.registered_buffer
                && let Ok(transferred) = &result
                && let Some(registration) = self.buffer_registration.as_mut()
                // Only the registration this operation actually named. An older
                // one's completion must not touch its replacement.
                && registration.generation == generation
                && let Some(mark) = registration.watermarks.get_mut(index as usize)
            {
                let start = offset as usize;
                // The watermark is a contiguous prefix. A read landing past it
                // leaves a gap of genuinely uninitialized bytes before it, so
                // raising the mark would falsely vouch for that gap.
                if start <= *mark {
                    let filled = start + *transferred as usize;
                    *mark = (*mark).max(filled);
                }
            }

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
            // The ring is closed and nothing is outstanding, so the kernel can
            // no longer reach any registered resource: these are safe to free.
            self.buffer_registration = None;
            self.retired_buffer_registrations.clear();
            self.file_registration = None;
            self.retired_file_registrations.clear();
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
        // Registered resources are reachable by the kernel for as long as the
        // ring lives, and this ring could not be quiesced, so they must be
        // abandoned too rather than freed with the driver.
        std::mem::forget(self.buffer_registration.take());
        std::mem::forget(std::mem::take(&mut self.retired_buffer_registrations));
        std::mem::forget(self.file_registration.take());
        std::mem::forget(std::mem::take(&mut self.retired_file_registrations));

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
            // SAFETY: `completion_event` becomes a field of the `Driver`, whose
            // `Drop` impl runs `teardown` — closing the ring — before any field
            // is dropped, so the ring can no longer signal the event by the time
            // its handle closes. On the failure paths below the ring is closed
            // explicitly before the event goes out of scope.
            unsafe { ring.set_io_ring_completion_event(completion_event.handle())? };
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
                buffer_registration: None,
                registration_generation: 0,
                retired_buffer_registrations: Vec::new(),
                file_registration: None,
                retired_file_registrations: Vec::new(),
                cancel_holds: Vec::new(),
                deferred_cancels: Vec::new(),
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
                #[cfg(test)]
                fail_next_registration: false,
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
        self.read_with_flags(file, buffer, len, offset, SqeFlags::NONE)
    }

    /// Reads with explicit submission queue entry flags.
    ///
    /// Use this to order an operation against those already queued, with
    /// [SqeFlags::DRAIN_PRECEDING_OPS].
    pub fn read_with_flags<B: IoBufMut>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
        sqe_flags: SqeFlags,
    ) -> ReadFuture<B> {
        match self.try_read(file, buffer, len, offset, sqe_flags, None) {
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
        sqe_flags: SqeFlags,
        sequential: Option<SequentialGuard>,
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
            file: Some(file.state()),
            slot: Rc::clone(&slot),
            registered_buffer: None,
            uses_registered_file: false,
            pending_registration: None,
            _sequential: sequential,
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
            .with_sqe_flags(sqe_flags)
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
    /// Writes `buffer` to `file` at `offset`.
    ///
    /// The buffer is moved into the driver for the duration of the operation and
    /// returned when it completes, whether it succeeded or failed. Dropping the
    /// returned future is safe.
    ///
    /// The write is bounded by the buffer's *initialized* length, not its
    /// capacity, so uninitialized memory is never handed to the kernel.
    pub fn write<B: IoBuf>(&self, file: &File, buffer: B, len: u32, offset: u64) -> WriteFuture<B> {
        self.write_with_options(
            file,
            buffer,
            len,
            offset,
            FILE_WRITE_FLAGS_NONE,
            SqeFlags::NONE,
        )
    }

    /// Writes `buffer` to `file` at `offset` with explicit platform write flags.
    ///
    /// Use this for write-through and similar behaviour; [`Handle::write`] is
    /// the same thing with no flags set.
    pub fn write_with_options<B: IoBuf>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
        flags: FILE_WRITE_FLAGS,
        sqe_flags: SqeFlags,
    ) -> WriteFuture<B> {
        match self.try_write(
            file,
            buffer,
            len,
            offset,
            WriteOptions {
                flags,
                sqe_flags,
                sequential: None,
            },
        ) {
            Ok(fut) => fut,
            Err((error, buffer)) => WriteFuture::failed(error, buffer),
        }
    }

    fn try_write<B: IoBuf>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
        options: WriteOptions,
    ) -> std::result::Result<WriteFuture<B>, (Error, B)> {
        let WriteOptions {
            flags,
            sqe_flags,
            sequential,
        } = options;
        {
            let inner = self.strong.borrow();
            if inner.shutting_down {
                return Err((Error::ShuttingDown, buffer));
            }
            if let Err(e) = inner.ring.ensure_op_supported(IORING_OP_WRITE) {
                return Err((e, buffer));
            }
        }
        // Bound the write by what the caller has actually initialized. Capacity
        // would not do: the tail of a `Vec`'s allocation is uninitialized, and
        // sending it to the kernel would leak whatever happened to be there.
        if let Err(e) = check_write_initialized(&buffer, len as u64) {
            return Err((e, buffer));
        }

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let mut inner = self.strong.borrow_mut();

        let payload = Box::new(OpPayload {
            buffer: None,
            file: Some(file.state()),
            slot: Rc::clone(&slot),
            registered_buffer: None,
            uses_registered_file: false,
            pending_registration: None,
            _sequential: sequential,
        });
        let token = match inner.slab.insert(payload) {
            Ok(token) => token,
            Err(_) => return Err((Error::QueueFull, buffer)),
        };

        // Box first, then take the address: a buffer stored inline moves when
        // the value moves, so taking the pointer beforehand would give the
        // kernel a stale one.
        let boxed: Box<B> = Box::new(buffer);
        let data_ptr = boxed.buf_ptr();
        {
            let payload = inner
                .slab
                .payload_mut(token)
                .and_then(|p| p.downcast_mut::<OpPayload>())
                .expect("just inserted");
            payload.buffer = Some(boxed as Box<dyn Any>);
        }

        let op = crate::io_ring::ops::WriteOp::builder()
            .with_raw_handle(file.as_raw_handle())
            .with_raw_data_address(data_ptr as *mut _)
            .with_num_of_bytes_to_write(len)
            .with_offset(offset)
            .with_write_flags(flags)
            .with_user_data(token.as_user_data())
            .with_sqe_flags(sqe_flags)
            .build();

        let op = match op {
            Ok(op) => op,
            Err(e) => return Err((e, recover_buffer(&mut inner, token))),
        };

        // SAFETY: the payload lives in the slab until this operation's own
        // completion is dequeued, and the slab never moves a boxed payload, so
        // `data_ptr` stays valid for as long as the kernel may read it. The
        // file's handle is kept open by the `Rc<FileState>` in the payload.
        if let Err(e) = unsafe { inner.ring.build_write_file(op) } {
            return Err((e, recover_buffer(&mut inner, token)));
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        drop(inner);

        let _ = self.wake.signal();

        Ok(WriteFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
            _buffer: PhantomData,
        })
    }

    /// Reads on behalf of [`File::read`], carrying the sequential guard.
    ///
    /// The guard travels in the operation's payload so the file's outstanding
    /// flag clears at terminal completion rather than when the future is
    /// dropped.
    pub(crate) fn read_sequential<B: IoBufMut>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
        guard: SequentialGuard,
    ) -> ReadFuture<B> {
        match self.try_read(file, buffer, len, offset, SqeFlags::NONE, Some(guard)) {
            Ok(fut) => fut,
            Err((error, buffer)) => ReadFuture::failed(error, buffer),
        }
    }

    /// Writes on behalf of [`File::write`], carrying the sequential guard.
    pub(crate) fn write_sequential<B: IoBuf>(
        &self,
        file: &File,
        buffer: B,
        len: u32,
        offset: u64,
        guard: SequentialGuard,
    ) -> WriteFuture<B> {
        match self.try_write(
            file,
            buffer,
            len,
            offset,
            WriteOptions {
                flags: FILE_WRITE_FLAGS_NONE,
                sqe_flags: SqeFlags::NONE,
                sequential: Some(guard),
            },
        ) {
            Ok(fut) => fut,
            Err((error, buffer)) => WriteFuture::failed(error, buffer),
        }
    }

    /// Flushes `file`, using the platform's default flush mode.
    pub fn flush(&self, file: &File) -> FlushFuture {
        self.flush_with_options(file, FILE_FLUSH_DEFAULT, SqeFlags::NONE)
    }

    /// Flushes `file` with an explicit flush mode.
    pub fn flush_with_options(
        &self,
        file: &File,
        mode: FILE_FLUSH_MODE,
        sqe_flags: SqeFlags,
    ) -> FlushFuture {
        match self.try_flush(file, mode, sqe_flags) {
            Ok(fut) => fut,
            Err(e) => FlushFuture {
                state: FlushState::Failed(Some(e)),
            },
        }
    }

    fn try_flush(
        &self,
        file: &File,
        mode: FILE_FLUSH_MODE,
        sqe_flags: SqeFlags,
    ) -> Result<FlushFuture> {
        {
            let inner = self.strong.borrow();
            if inner.shutting_down {
                return Err(Error::ShuttingDown);
            }
            inner.ring.ensure_op_supported(IORING_OP_FLUSH)?;
        }

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let mut inner = self.strong.borrow_mut();

        // A flush carries no caller buffer, but still needs the file kept open
        // for as long as the kernel is working on it.
        let payload = Box::new(OpPayload {
            buffer: None,
            file: Some(file.state()),
            slot: Rc::clone(&slot),
            registered_buffer: None,
            uses_registered_file: false,
            pending_registration: None,
            _sequential: None,
        });
        let token = inner.slab.insert(payload).map_err(|_| Error::QueueFull)?;

        let op = crate::io_ring::ops::FlushOp::builder()
            .with_raw_handle(file.as_raw_handle())
            .with_flush_mode(mode)
            .with_user_data(token.as_user_data())
            .with_sqe_flags(sqe_flags)
            .build();

        let op = match op {
            Ok(op) => op,
            Err(e) => {
                drop(inner.slab.complete(token));
                return Err(e);
            }
        };

        // SAFETY: the file's handle is kept open by the `Rc<FileState>` held in
        // the payload, which lives until this operation's completion.
        if let Err(e) = unsafe { inner.ring.build_flush_file(op) } {
            drop(inner.slab.complete(token));
            return Err(e);
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        drop(inner);

        let _ = self.wake.signal();

        Ok(FlushFuture {
            state: FlushState::Waiting(OpFuture::pending(token, slot, Weak::clone(&self.inner))),
        })
    }

    /// Registers buffers with the ring.
    ///
    /// On success the driver takes ownership: the platform keeps pointers to
    /// these buffers, and offers no way to withdraw a registration, so they
    /// cannot be handed back while the ring lives. Registering again supersedes
    /// this registration; the superseded buffers are retained until the ring is
    /// closed, because the platform may still reference them.
    ///
    /// On failure the buffers come straight back.
    pub async fn register_buffers<B: IoBufMut>(&self, buffers: Vec<B>) -> Registered<B> {
        // All driver borrows are confined to this call, so none is held across
        // the await below.
        let (token, slot) = match self.start_register_buffers(buffers) {
            Ok(started) => started,
            Err((e, buffers)) => return Registered::Failed(e, buffers),
        };
        let _ = self.wake.signal();

        // The driver adopts the registration on completion, in completion
        // order, and hands the resources back through the slot on failure.
        match (RegistrationFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
        })
        .await
        {
            Ok(()) => Registered::Ok,
            Err((e, returned)) => Registered::Failed(e, unbox_pending::<B>(returned)),
        }
    }

    /// Parks the buffers in the driver and builds the registration.
    ///
    /// Split out from [`Handle::register_buffers`] so the driver borrow cannot
    /// outlive the synchronous part.
    #[allow(clippy::type_complexity)]
    fn start_register_buffers<B: IoBufMut>(
        &self,
        buffers: Vec<B>,
    ) -> std::result::Result<(Token, Rc<RefCell<ResultSlot>>), (Error, Vec<B>)> {
        if buffers.is_empty() {
            // The platform accepts an empty registration and then fails it at
            // completion time, so reject it here where the error is useful.
            return Err((Error::MissingField { field: "buffers" }, buffers));
        }

        let mut inner = self.strong.borrow_mut();
        if inner.shutting_down {
            return Err((Error::ShuttingDown, buffers));
        }
        if let Err(e) = inner.ring.ensure_op_supported(IORING_OP_REGISTER_BUFFERS) {
            return Err((e, buffers));
        }

        // Box each buffer so its address is stable for as long as the platform
        // holds a pointer to it.
        let mut boxed: Vec<Box<B>> = buffers.into_iter().map(Box::new).collect();
        let mut extents = Vec::with_capacity(boxed.len());
        let mut watermarks = Vec::with_capacity(boxed.len());
        let mut descriptors = Vec::with_capacity(boxed.len());
        for buffer in &mut boxed {
            // A descriptor length is a `u32`, so the registered extent is what
            // actually fits. Validation reads this same value and never the
            // original capacity, so the two cannot disagree.
            let extent = buffer.buf_capacity().min(u32::MAX as usize);
            let watermark = buffer.buf_len().min(extent);
            let ptr = buffer.buf_mut_ptr();
            extents.push(extent);
            watermarks.push(watermark);
            // SAFETY: `ptr` points into the box, which is moved into the driver
            // below and retained until the ring closes, so the address stays
            // valid for as long as the platform may use it.
            descriptors.push(unsafe {
                crate::io_ring::BufferInfo::from_raw_parts(ptr.cast(), extent as u32)
            });
        }

        // Move the resources into the driver *before* building. From the moment
        // the registration is built the platform holds pointers into them, and
        // the future returned by `register_buffers` can be dropped at any time;
        // leaving them in locals would free them out from under the kernel.
        let descriptors = descriptors.into_boxed_slice();
        let descriptors_ptr: *const [crate::io_ring::BufferInfo] = descriptors.as_ref();

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let payload = Box::new(OpPayload {
            buffer: None,
            file: None,
            slot: Rc::clone(&slot),
            registered_buffer: None,
            uses_registered_file: false,
            pending_registration: Some(PendingRegistration::Buffers {
                buffers: boxed.into_iter().map(|b| b as Box<dyn Any>).collect(),
                extents,
                watermarks,
                descriptors,
            }),
            _sequential: None,
        });
        let token = match inner.slab.insert(payload) {
            Ok(token) => token,
            // Nothing was built, so the payload comes straight back.
            Err(payload) => return Err((Error::QueueFull, unbox_payload::<B>(payload))),
        };

        // SAFETY: `descriptors_ptr` addresses the descriptor slice's heap
        // allocation, which was moved into the payload above. Moving the boxed
        // slice does not move its allocation, so the pointer is still valid,
        // and nothing mutates the descriptors after this point.
        let descriptors: &[crate::io_ring::BufferInfo] = unsafe { &*descriptors_ptr };
        #[cfg(test)]
        let descriptors = if std::mem::take(&mut inner.fail_next_registration) {
            &descriptors[..0]
        } else {
            descriptors
        };

        // SAFETY: the descriptors and the buffers they point at are owned by the
        // driver and retained until the ring closes.
        if let Err(e) = unsafe {
            inner
                .ring
                .build_register_buffers(descriptors, token.as_user_data())
        } {
            let recovered = inner
                .slab
                .complete(token)
                .map(unbox_payload::<B>)
                .unwrap_or_default();
            return Err((e, recovered));
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        Ok((token, slot))
    }

    /// Registers file handles with the ring.
    ///
    /// The registration lasts for the life of the ring, as it does for
    /// [`Handle::register_buffers`]: it cannot be withdrawn, and registering
    /// again supersedes it without releasing the old handles.
    ///
    /// Unlike buffers, this borrows rather than consumes. The driver keeps its
    /// own reference to each handle, so the caller may go on using its
    /// [`File`] values, or drop them, without invalidating the registration.
    pub async fn register_files(&self, files: &[File]) -> Result<()> {
        let (token, slot) = self.start_register_files(files)?;
        let _ = self.wake.signal();

        // Nothing was taken from the caller, so there is nothing to give back.
        (RegistrationFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
        })
        .await
        .map_err(|(e, _)| e)
    }

    /// Parks handle references in the driver and builds the registration.
    ///
    /// Split out from [`Handle::register_files`] so the driver borrow cannot
    /// outlive the synchronous part.
    #[allow(clippy::type_complexity)]
    fn start_register_files(&self, files: &[File]) -> Result<(Token, Rc<RefCell<ResultSlot>>)> {
        if files.is_empty() {
            return Err(Error::MissingField { field: "files" });
        }

        let mut inner = self.strong.borrow_mut();
        if inner.shutting_down {
            return Err(Error::ShuttingDown);
        }
        inner.ring.ensure_op_supported(IORING_OP_REGISTER_FILES)?;

        let handles: Vec<_> = files.iter().map(|f| f.as_raw_handle()).collect();
        // Park references that keep the handles open before building, so
        // dropping the caller's files cannot close them under the platform.
        let states: Vec<Rc<FileState>> = files.iter().map(|f| f.state()).collect();

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let payload = Box::new(OpPayload {
            buffer: None,
            file: None,
            slot: Rc::clone(&slot),
            registered_buffer: None,
            uses_registered_file: false,
            pending_registration: Some(PendingRegistration::Files { files: states }),
            _sequential: None,
        });
        let token = inner.slab.insert(payload).map_err(|_| Error::QueueFull)?;

        // SAFETY: each handle is kept open by the `Rc<FileState>` clone parked
        // in the payload above, which the driver retains until the ring closes.
        // `handles` itself is only read during this call.
        if let Err(e) = unsafe {
            inner
                .ring
                .build_register_file_handles(&handles, token.as_user_data())
        } {
            drop(inner.slab.complete(token));
            return Err(e);
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        Ok((token, slot))
    }

    /// Reads into a previously registered buffer.
    ///
    /// The data lands in the registration rather than in a buffer the caller
    /// owns, so this resolves to the transfer count alone. A successful read
    /// raises that buffer's initialization watermark, so a later write may
    /// source the bytes it just filled.
    pub fn read_into_registered(
        &self,
        target: FileTarget<'_>,
        buffer_index: u32,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
    ) -> RegisteredOpFuture {
        self.registered_op(target, buffer_index, buffer_offset, len, file_offset, false)
    }

    /// Writes from a previously registered buffer.
    ///
    /// The range is bounded by the buffer's initialization watermark, so
    /// uninitialized memory is never sent to the kernel.
    pub fn write_from_registered(
        &self,
        target: FileTarget<'_>,
        buffer_index: u32,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
    ) -> RegisteredOpFuture {
        self.registered_op(target, buffer_index, buffer_offset, len, file_offset, true)
    }

    fn registered_op(
        &self,
        target: FileTarget<'_>,
        buffer_index: u32,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
        is_write: bool,
    ) -> RegisteredOpFuture {
        match self.try_registered_op(
            target,
            buffer_index,
            buffer_offset,
            len,
            file_offset,
            is_write,
        ) {
            Ok(fut) => fut,
            Err(e) => RegisteredOpFuture {
                state: RegisteredOpState::Failed(Some(e)),
            },
        }
    }

    fn try_registered_op(
        &self,
        target: FileTarget<'_>,
        buffer_index: u32,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
        is_write: bool,
    ) -> Result<RegisteredOpFuture> {
        let op_code = if is_write {
            IORING_OP_WRITE
        } else {
            IORING_OP_READ
        };
        {
            let inner = self.strong.borrow();
            if inner.shutting_down {
                return Err(Error::ShuttingDown);
            }
            inner.ring.ensure_op_supported(op_code)?;
            // Reads may fill the whole registered extent; writes may only
            // source what has been initialized.
            inner.check_registered_buffer(buffer_index, buffer_offset, len, is_write)?;
            if let FileTarget::Registered { index } = target {
                inner.check_registered_file(index)?;
            }
        }

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let mut inner = self.strong.borrow_mut();

        let (file_state, uses_registered_file) = match target {
            FileTarget::Owned(file) => (Some(file.state()), false),
            FileTarget::Registered { .. } => (None, true),
        };

        let payload = Box::new(OpPayload {
            buffer: None,
            file: file_state,
            slot: Rc::clone(&slot),
            registered_buffer: Some((
                inner
                    .buffer_registration
                    .as_ref()
                    .map(|r| r.generation)
                    .unwrap_or_default(),
                buffer_index,
                buffer_offset,
            )),
            uses_registered_file,
            pending_registration: None,
            _sequential: None,
        });
        let token = inner.slab.insert(payload).map_err(|_| Error::QueueFull)?;

        let built = if is_write {
            let mut builder = crate::io_ring::ops::WriteOp::builder()
                .with_registered_data_index_and_offset(buffer_index, buffer_offset)
                .with_num_of_bytes_to_write(len)
                .with_offset(file_offset)
                .with_user_data(token.as_user_data());
            builder = match target {
                FileTarget::Owned(file) => builder.with_raw_handle(file.as_raw_handle()),
                FileTarget::Registered { index } => builder.with_registered_handle_index(index),
            };
            builder.build().and_then(|op| {
                // SAFETY: the registered buffer and handle are owned by the
                // driver's registrations, which outlive this operation.
                unsafe { inner.ring.build_write_file(op) }
            })
        } else {
            let mut builder = crate::io_ring::ops::ReadOp::builder()
                .with_registered_data_index_and_offset(buffer_index, buffer_offset)
                .with_num_of_bytes_to_read(len)
                .with_offset(file_offset)
                .with_user_data(token.as_user_data());
            builder = match target {
                FileTarget::Owned(file) => builder.with_raw_handle(file.as_raw_handle()),
                FileTarget::Registered { index } => builder.with_registered_handle_index(index),
            };
            builder.build().and_then(|op| {
                // SAFETY: as above.
                unsafe { inner.ring.build_read_file(op) }
            })
        };

        if let Err(e) = built {
            drop(inner.slab.complete(token));
            return Err(e);
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        drop(inner);

        let _ = self.wake.signal();

        Ok(RegisteredOpFuture {
            state: RegisteredOpState::Waiting(OpFuture::pending(
                token,
                slot,
                Weak::clone(&self.inner),
            )),
        })
    }

    /// Cancellation is best-effort: it may fail, or arrive too late, and neither
    /// is an error. The caller keeps the original future and still observes that
    /// operation's terminal result, which is the only thing that releases the
    /// buffer. Cancelling twice, or cancelling an operation that has already
    /// finished, is a no-op.
    pub fn cancel(&self, id: OperationId) {
        self.strong.borrow_mut().request_cancel(id.0);
    }
}

/// An operation against registered resources.
///
/// The data lives in the driver's registration rather than in a buffer the
/// caller owns, so this resolves to the transfer count alone.
pub struct RegisteredOpFuture {
    state: RegisteredOpState,
}

enum RegisteredOpState {
    Failed(Option<Error>),
    Waiting(OpFuture),
    Done,
}

impl RegisteredOpFuture {
    /// Returns the identifier for cancelling this operation, if it was
    /// submitted.
    pub fn operation_id(&self) -> Option<OperationId> {
        match &self.state {
            RegisteredOpState::Waiting(op) => Some(op.operation_id()),
            _ => None,
        }
    }
}

impl Future for RegisteredOpFuture {
    type Output = Result<Transferred>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            RegisteredOpState::Failed(taken) => {
                let error = taken.take().expect("polled after completion");
                this.state = RegisteredOpState::Done;
                Poll::Ready(Err(error))
            }
            RegisteredOpState::Done => panic!("registered operation polled after completion"),
            RegisteredOpState::Waiting(op) => match op.poll_resolution(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(resolution) => {
                    this.state = RegisteredOpState::Done;
                    Poll::Ready(match resolution {
                        Resolution::Completed(result, _) => result,
                        Resolution::Retained(e) => Err(e),
                    })
                }
            },
        }
    }
}

/// Recovers the caller's buffers from a slot payload whose registration never
/// reached the kernel.
fn unbox_payload<B: 'static>(payload: Box<dyn Any>) -> Vec<B> {
    match payload.downcast::<OpPayload>() {
        Ok(payload) => unbox_pending::<B>(payload.pending_registration),
        Err(_) => Vec::new(),
    }
}

/// Recovers the caller's buffers from a returned pending registration.
fn unbox_pending<B: 'static>(pending: Option<PendingRegistration>) -> Vec<B> {
    match pending {
        Some(PendingRegistration::Buffers { buffers, .. }) => buffers
            .into_iter()
            .filter_map(|b| b.downcast::<B>().ok().map(|b| *b))
            .collect(),
        _ => Vec::new(),
    }
}

/// What varies between the write entry points, kept together so `try_write`
/// does not grow an unreadable argument list.
struct WriteOptions {
    /// Platform write flags, such as write-through.
    flags: FILE_WRITE_FLAGS,
    /// Submission queue entry flags, such as draining preceding operations.
    sqe_flags: SqeFlags,
    /// Present only for a sequential write, whose file must be told when the
    /// operation ends.
    sequential: Option<SequentialGuard>,
}

/// A registration in progress.
///
/// Registration carries the caller's resources but hands back no buffer of its
/// own, so it resolves to a plain result. On failure it also returns whatever
/// the driver was holding, so the caller gets its resources back.
struct RegistrationFuture {
    inner: OpFuture,
}

impl Future for RegistrationFuture {
    type Output = std::result::Result<(), (Error, Option<PendingRegistration>)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.inner.poll_resolution(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Resolution::Completed(result, returned)) => {
                let returned = returned.and_then(|b| b.downcast::<PendingRegistration>().ok());
                Poll::Ready(match result {
                    Ok(_) => Ok(()),
                    Err(e) => Err((e, returned.map(|b| *b))),
                })
            }
            Poll::Ready(Resolution::Retained(e)) => Poll::Ready(Err((e, None))),
        }
    }
}

/// Takes a buffer back out of a slot whose operation never reached the kernel.
///
/// Only correct before the operation has been built into the submission queue,
/// because after that the entry references the buffer and cannot be withdrawn.
fn recover_buffer<B: 'static>(inner: &mut DriverInner, token: Token) -> B {
    let payload = inner
        .slab
        .complete(token)
        .expect("slot was just inserted")
        .downcast::<OpPayload>()
        .unwrap_or_else(|_| unreachable!("payload type mismatch"));
    let buffer = payload.buffer.expect("payload lost its buffer");
    *buffer.downcast::<B>().expect("buffer type mismatch")
}

/// The shared machinery behind every operation future.
///
/// Holds only a token, a shared result slot, and a **weak** reference to the
/// driver. The buffer itself lives in the driver, which is what makes dropping
/// an operation future safe. The reference is weak so that a future cannot keep
/// the driver alive; failing to upgrade it is precisely the signal that the
/// driver is gone and the future must resolve rather than wait forever.
struct OpFuture {
    token: Token,
    slot: Rc<RefCell<ResultSlot>>,
    driver: Weak<RefCell<DriverInner>>,
    /// Set once the operation has resolved, so `Drop` knows there is nothing
    /// left to detach or cancel.
    finished: bool,
}

/// How an operation ended.
enum Resolution {
    /// The kernel reported a terminal result, along with the buffer if the
    /// operation carried one.
    Completed(Result<Transferred>, Option<Box<dyn Any>>),
    /// The buffer could not be recovered, so it was abandoned.
    Retained(Error),
}

impl OpFuture {
    fn pending(
        token: Token,
        slot: Rc<RefCell<ResultSlot>>,
        driver: Weak<RefCell<DriverInner>>,
    ) -> Self {
        Self {
            token,
            slot,
            driver,
            finished: false,
        }
    }

    fn poll_resolution(&mut self, cx: &mut Context<'_>) -> Poll<Resolution> {
        let mut slot = self.slot.borrow_mut();

        if let Some((result, buffer)) = slot.completed.take() {
            drop(slot);
            self.finished = true;
            return Poll::Ready(Resolution::Completed(result, buffer));
        }

        if slot.retained {
            drop(slot);
            self.finished = true;
            return Poll::Ready(Resolution::Retained(Error::BufferRetained));
        }

        if self.driver.upgrade().is_none() {
            drop(slot);
            self.finished = true;
            return Poll::Ready(Resolution::Retained(Error::DriverGone));
        }

        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn operation_id(&self) -> OperationId {
        OperationId(self.token)
    }
}

impl Drop for OpFuture {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Reaching the driver is exactly why this holds a weak reference:
        // without it there would be no way to record the detachment.
        let Some(inner) = self.driver.upgrade() else {
            return;
        };
        let Ok(mut inner) = inner.try_borrow_mut() else {
            return;
        };

        // Detaching leaves the operation running. Its buffer is released only
        // when its own completion is dequeued, never here. What varies is
        // whether anything can be done about it.
        match inner.slab.detach(self.token) {
            Some(Lifecycle::Described) => {
                // Nothing was ever built, so no queue entry references the
                // buffer and it can be released immediately.
                drop(inner.slab.complete(self.token));
            }
            Some(Lifecycle::Built) => {
                // A submission queue entry references the buffer and cannot be
                // withdrawn, and the kernel has not seen the operation yet, so
                // there is nothing for the platform to cancel. The slab records
                // this slot as detached, and the driver cancels it once
                // submission promotes it.
            }
            Some(Lifecycle::Submitted) => {
                // Best-effort: ask the platform to give up early. Failure here
                // is not an error and changes nothing about the buffer's
                // lifetime. This returns without waiting on the kernel.
                inner.request_cancel(self.token);
            }
            None => {}
        }
    }
}

/// Turns a resolution into an outcome, recovering the caller's buffer.
fn into_outcome<B: 'static>(resolution: Resolution) -> Outcome<Transferred, B> {
    match resolution {
        Resolution::Completed(result, buffer) => {
            let buffer = buffer.expect("a buffer-carrying operation lost its buffer");
            let buffer = *buffer.downcast::<B>().expect("buffer type mismatch");
            Outcome::Completed(BufResult::new(result, buffer))
        }
        Resolution::Retained(e) => Outcome::Retained(e),
    }
}

/// A read in progress.
pub struct ReadFuture<B> {
    state: BufOpState<B>,
}

/// A write in progress.
pub struct WriteFuture<B> {
    inner: OpFuture,
    /// Records the buffer type without affecting auto traits.
    _buffer: PhantomData<fn() -> B>,
}

enum BufOpState<B> {
    /// Rejected before anything was submitted; the buffer never left.
    Failed(Option<(Error, B)>),
    /// Submitted and awaiting completion.
    Waiting(OpFuture),
    /// Already resolved.
    Done,
}

impl<B: IoBufMut> ReadFuture<B> {
    pub(crate) fn failed(error: Error, buffer: B) -> Self {
        Self {
            state: BufOpState::Failed(Some((error, buffer))),
        }
    }

    fn pending(
        token: Token,
        slot: Rc<RefCell<ResultSlot>>,
        driver: Weak<RefCell<DriverInner>>,
    ) -> Self {
        Self {
            state: BufOpState::Waiting(OpFuture::pending(token, slot, driver)),
        }
    }

    /// Returns the identifier for cancelling this operation, if it was
    /// submitted.
    pub fn operation_id(&self) -> Option<OperationId> {
        match &self.state {
            BufOpState::Waiting(op) => Some(op.operation_id()),
            _ => None,
        }
    }
}

impl<B: IoBufMut> Future for ReadFuture<B> {
    type Output = Outcome<Transferred, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            BufOpState::Failed(taken) => {
                let (error, buffer) = taken.take().expect("polled after completion");
                this.state = BufOpState::Done;
                Poll::Ready(Outcome::Completed(BufResult::new(Err(error), buffer)))
            }
            BufOpState::Done => panic!("read future polled after completion"),
            BufOpState::Waiting(op) => match op.poll_resolution(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(resolution) => {
                    let mut outcome = into_outcome::<B>(resolution);
                    if let Outcome::Completed(result) = &mut outcome
                        && let Ok(transferred) = &result.result
                    {
                        // SAFETY: the kernel reported writing this many bytes
                        // into the buffer, so they are initialized, and the
                        // count is bounded by the capacity checked before
                        // submission.
                        unsafe { result.buffer.set_buf_init(*transferred as usize) };
                    }
                    this.state = BufOpState::Done;
                    Poll::Ready(outcome)
                }
            },
        }
    }
}

impl<B: IoBuf> WriteFuture<B> {
    pub(crate) fn failed(error: Error, buffer: B) -> Self {
        // A rejected write never reaches the driver, so it needs no token. The
        // shared machinery is only for operations that did.
        Self {
            inner: OpFuture {
                token: Token::from_user_data(0),
                slot: Rc::new(RefCell::new(ResultSlot {
                    completed: Some((Err(error), Some(Box::new(buffer)))),
                    retained: false,
                    waker: None,
                })),
                driver: Weak::new(),
                finished: true,
            },
            _buffer: PhantomData,
        }
    }

    /// Returns the identifier for cancelling this operation, if it was
    /// submitted.
    pub fn operation_id(&self) -> Option<OperationId> {
        if self.inner.finished {
            None
        } else {
            Some(self.inner.operation_id())
        }
    }
}

impl<B: IoBuf> Future for WriteFuture<B> {
    type Output = Outcome<Transferred, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        // A rejected write has its result waiting in the slot already, so this
        // takes the same path as a real completion.
        match this.inner.poll_resolution(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(resolution) => Poll::Ready(into_outcome::<B>(resolution)),
        }
    }
}

/// A flush in progress.
///
/// Flush carries no caller buffer, so it resolves to a plain result rather than
/// an [`Outcome`].
pub struct FlushFuture {
    state: FlushState,
}

enum FlushState {
    Failed(Option<Error>),
    Waiting(OpFuture),
    Done,
}

impl FlushFuture {
    /// Returns the identifier for cancelling this operation, if it was
    /// submitted.
    pub fn operation_id(&self) -> Option<OperationId> {
        match &self.state {
            FlushState::Waiting(op) => Some(op.operation_id()),
            _ => None,
        }
    }
}

impl Future for FlushFuture {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            FlushState::Failed(taken) => {
                let error = taken.take().expect("polled after completion");
                this.state = FlushState::Done;
                Poll::Ready(Err(error))
            }
            FlushState::Done => panic!("flush future polled after completion"),
            FlushState::Waiting(op) => match op.poll_resolution(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(resolution) => {
                    this.state = FlushState::Done;
                    Poll::Ready(match resolution {
                        Resolution::Completed(result, _) => result.map(|_| ()),
                        Resolution::Retained(e) => Err(e),
                    })
                }
            },
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
        File::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testdata/sample.txt"
        ))
        .unwrap()
    }

    /// FR-038: filling the submission queue must produce a distinct, matchable
    /// error, and must hand the caller's buffer straight back.
    ///
    /// The buffer is what makes this worth testing separately from the raw
    /// layer's own queue-full test: a safe API that swallowed it on rejection
    /// would be losing the caller's data.
    #[test]
    fn a_full_submission_queue_returns_the_buffer_with_the_error() {
        let ring = IoRing::builder()
            .with_submission_queue_size(2)
            .with_completion_queue_size(2)
            .build()
            .unwrap();
        let capacity = ring.info().unwrap().submission_queue_size;
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let file = readme();

        // Build without ever submitting, so the queue can only fill up.
        let mut held = Vec::new();
        let mut rejected = None;
        for i in 0..(capacity + 4) {
            let marker = (i % 251) as u8;
            let fut = handle.read(&file, vec![marker; 16], 8, 0);
            // A rejected operation never reached the kernel, so it has no
            // identifier.
            match fut.operation_id() {
                Some(_) => held.push(fut),
                None => {
                    rejected = Some((marker, fut));
                    break;
                }
            }
        }

        let (marker, fut) = rejected.expect("the submission queue never filled up");
        let outcome = futures::executor::block_on(fut);
        assert!(
            matches!(outcome.err(), Some(Error::QueueFull)),
            "expected QueueFull, got {:?}",
            outcome.err()
        );
        let (_, buffer) = outcome.expect_completed().into_parts();
        assert_eq!(
            buffer,
            vec![marker; 16],
            "the rejected read must return the caller's buffer untouched"
        );

        drop(held);
        handle.shutdown();
        drain(&driver);
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

    /// FR-004: cancelling immediately after submitting must work. This is the
    /// obvious usage — take the id, decide to cancel — and the kernel does not
    /// have the operation yet at that point, so the request has to be
    /// remembered rather than dropped on the floor.
    #[test]
    fn cancelling_before_submission_is_honoured_on_promotion() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
        let id = fut.operation_id().expect("read was built");
        let token = id.0;

        // The operation is only built; the kernel has never seen it.
        assert_eq!(
            driver.inner.borrow().slab.state(token).map(|s| s.0),
            Some(Lifecycle::Built)
        );

        handle.cancel(id);
        // Cancelling repeatedly must not queue the request twice.
        handle.cancel(id);
        handle.cancel(id);

        {
            let inner = driver.inner.borrow();
            assert_eq!(
                inner.deferred_cancels.len(),
                1,
                "the request should be remembered exactly once"
            );
            assert!(
                inner.cancel_holds.is_empty(),
                "nothing can be cancelled before the kernel has the operation"
            );
        }

        // Submission promotes the operation, at which point the remembered
        // request must actually be issued.
        driver.inner.borrow_mut().submit_pending();
        {
            let inner = driver.inner.borrow();
            assert!(
                inner.deferred_cancels.is_empty(),
                "the remembered request should have been consumed"
            );
            assert!(
                !inner.cancel_holds.is_empty(),
                "cancelling before submission was silently lost"
            );
        }

        // The caller still observes the operation's own terminal result.
        drain(&driver);
        drop(fut);
        handle.shutdown();
    }

    /// Dropping a future whose operation the kernel already has must cancel it
    /// immediately, rather than deferring like the built case.
    ///
    /// This lives here rather than in an integration test because only from
    /// inside the crate can the operation be observed to have actually reached
    /// `Submitted` before the drop; `Handle::outstanding` counts built
    /// operations too, so it cannot distinguish the two paths.
    #[test]
    fn dropping_a_submitted_operation_cancels_it_immediately() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
        let token = fut.operation_id().expect("read was built").0;

        // Hand it to the kernel, so the drop below takes the submitted path.
        driver.inner.borrow_mut().submit_pending();
        assert_eq!(
            driver.inner.borrow().slab.state(token).map(|s| s.0),
            Some(Lifecycle::Submitted),
            "the operation must be submitted for this test to mean anything"
        );
        assert!(
            driver.inner.borrow().cancel_holds.is_empty(),
            "nothing should be cancelled yet"
        );

        drop(fut);

        {
            let inner = driver.inner.borrow();
            // A cancellation was issued straight away, and it holds the file
            // open independently of the target's own payload.
            assert!(
                !inner.cancel_holds.is_empty(),
                "dropping a submitted operation must cancel it immediately"
            );
            assert!(
                inner.slab.detached_submitted_uncancelled().is_empty(),
                "the operation should no longer be a deferred candidate"
            );
        }

        drain(&driver);
        handle.shutdown();
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

    /// SC-017: an unquiet teardown must abandon registered resources rather
    /// than free them, since the kernel may still reach them.
    ///
    /// Freeing them here would be a use-after-free, so the test asserts through
    /// a drop counter that nothing was dropped.
    #[test]
    fn an_unquiet_teardown_abandons_registered_buffers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        /// A registrable buffer that records its own destruction.
        struct CountingBuf(Vec<u8>);

        impl Drop for CountingBuf {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        // SAFETY: the pointer and lengths come straight from the inner `Vec`,
        // which is never reallocated because nothing here grows it, and the
        // buffer has no interior mutability.
        unsafe impl crate::buf::IoBuf for CountingBuf {
            fn buf_ptr(&self) -> *const u8 {
                self.0.as_ptr()
            }
            fn buf_len(&self) -> usize {
                self.0.len()
            }
        }

        // SAFETY: as above, and `set_buf_init` only ever reports bytes the
        // kernel actually wrote into the `Vec`'s capacity.
        unsafe impl crate::buf::IoBufMut for CountingBuf {
            fn buf_mut_ptr(&mut self) -> *mut u8 {
                self.0.as_mut_ptr()
            }
            fn buf_capacity(&self) -> usize {
                self.0.capacity()
            }
            unsafe fn set_buf_init(&mut self, len: usize) {
                // SAFETY: the caller guarantees `len` bytes are initialized.
                unsafe { self.0.set_len(len) }
            }
        }

        let driver = test_driver();
        let handle = driver.handle();

        let mut fut = Box::pin(handle.register_buffers(vec![CountingBuf(vec![0_u8; 32])]));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Let the registration land so the driver adopts it.
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if fut.as_mut().poll(&mut cx).is_ready() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            driver.inner.borrow().buffer_registration.is_some(),
            "the registration must have been adopted"
        );
        assert_eq!(DROPS.load(Ordering::SeqCst), 0);

        // Strand an operation — built but never submitted — so quiescence
        // cannot be established, then tear down without draining.
        let stranded = handle.read(&readme(), vec![0_u8; 64], 20, 0);
        assert_eq!(driver.inner.borrow().slab.outstanding(), 1);
        driver.inner.borrow_mut().force_unquiet_teardown = true;
        let wakers = driver.inner.borrow_mut().teardown();
        for w in wakers {
            w.wake();
        }
        drop(stranded);

        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            0,
            "an unquiet teardown must not free a buffer the kernel may reach"
        );
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

    /// SC-017: a registration that the platform accepts and then fails at
    /// completion must hand the caller's buffers back intact.
    ///
    /// This is the only failure path that cannot be provoked through the public
    /// API, so it uses the `fail_next_registration` seam.
    #[test]
    fn a_registration_failing_at_completion_returns_the_buffers() {
        let driver = test_driver();
        let handle = driver.handle();

        driver.inner.borrow_mut().fail_next_registration = true;
        let mut fut = Box::pin(handle.register_buffers(vec![vec![7_u8; 16]]));

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "the registration must reach the platform before it can fail"
        );

        // Drive until the failing completion arrives.
        let mut resolved = None;
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(result) = fut.as_mut().poll(&mut cx) {
                resolved = Some(result);
                break;
            }
            std::thread::yield_now();
        }

        match resolved.expect("the registration must resolve") {
            Registered::Failed(_, returned) => {
                assert_eq!(returned, vec![vec![7_u8; 16]], "the buffer must come back");
            }
            Registered::Ok => panic!("an empty registration must be refused by the platform"),
        }

        // Nothing was adopted, so the driver still has no registration.
        assert!(driver.inner.borrow().buffer_registration.is_none());

        handle.shutdown();
    }
}
