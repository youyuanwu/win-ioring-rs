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
use crate::sys::ArmedEvent;

use windows::Win32::Storage::FileSystem::{
    FILE_FLUSH_DEFAULT, FILE_FLUSH_MODE, FILE_WRITE_FLAGS, FILE_WRITE_FLAGS_NONE, IORING_OP_FLUSH,
    IORING_OP_READ, IORING_OP_REGISTER_BUFFERS, IORING_OP_REGISTER_FILES, IORING_OP_WRITE,
};

pub mod registry;
pub mod slab;

pub use registry::{RegisteredBuf, RegisteredBuffers};

use slab::{Lifecycle, OpSlab, Token, TokenKind};

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
    waker: Option<Waker>,
}

impl ResultSlot {
    fn new() -> Self {
        Self {
            completed: None,
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
    /// Records the index and offset within the registration, so a completed
    /// transfer can extend the right buffer's initialized prefix. The
    /// registration itself is reached through the handle the operation carries,
    /// so a superseded one's completion extends that one and cannot disturb its
    /// replacement.
    registered_buffer: Option<(u32, u32)>,
    /// The registered file index this operation names, if it named one instead
    /// of an owned file.
    ///
    /// Cancelling an operation requires naming the same file it named, so a
    /// cancellation for a registered-handle operation needs the index back.
    registered_file: Option<u32>,
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
        /// The base address of each buffer, as recorded in its descriptor.
        pointers: Vec<*mut u8>,
        extents: Vec<usize>,
        watermarks: Vec<usize>,
        descriptors: Box<[crate::io_ring::BufferInfo]>,
    },
    /// File handles, kept open for the platform.
    Files { files: Vec<Rc<FileState>> },
}

/// An active file handle registration.
struct FileRegistration {
    /// Kept so the registered handles stay open for as long as the platform
    /// may use them.
    _files: Vec<Rc<FileState>>,
    /// How many handles were registered, for index validation.
    count: usize,
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
/// On success the registration's buffers stay mapped for the life of the ring —
/// the platform offers no way to withdraw them — but the application keeps
/// access to them through the returned collection. On failure the buffers come
/// straight back.
pub enum Registered<T> {
    /// The registration succeeded, yielding the collection to check buffers out
    /// of.
    Ok(RegisteredBuffers),
    /// The registration failed and the caller's resources are returned.
    ///
    /// There is no case in which they are not: teardown drains until every
    /// registration attempt has reported, so a failure always hands the
    /// resources back.
    Failed(Error, Vec<T>),
}

impl<T> Registered<T> {
    /// Returns `true` if the registration succeeded.
    pub fn is_ok(&self) -> bool {
        matches!(self, Registered::Ok(_))
    }

    /// Returns the error, if the registration failed.
    pub fn err(&self) -> Option<&Error> {
        match self {
            Registered::Ok(_) => None,
            Registered::Failed(e, _) => Some(e),
        }
    }

    /// Unwraps a successful registration, yielding its collection.
    ///
    /// # Panics
    ///
    /// Panics if the registration failed.
    pub fn unwrap(self) -> RegisteredBuffers {
        match self {
            Registered::Ok(buffers) => buffers,
            Registered::Failed(e, _) => panic!("registration failed: {e}"),
        }
    }
}

/// How far along shutdown is.
///
/// Ordered so that escalation is a maximum and downgrade is impossible: a
/// graceful shutdown may become immediate, but an immediate one never relaxes
/// back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ShutdownMode {
    /// Accepting work normally.
    Running,
    /// No longer accepting work; operations already in flight are left to
    /// finish on their own.
    Graceful,
    /// No longer accepting work; cancellation is requested for everything in
    /// flight that the platform will accept one for.
    Immediate,
}

/// Shared driver state, reached by handles and futures.
pub(crate) struct DriverInner {
    ring: IoRing,
    slab: OpSlab,
    /// The active buffer registration, if any.
    ///
    /// Shared rather than owned: the platform may hold pointers into it until
    /// the ring closes, and an application handle may outlive the driver, so the
    /// memory must survive whichever of the three ends last.
    buffer_registration: Option<Rc<registry::RegistryInner>>,
    /// Buffer registrations that have been superseded but are still referenced.
    ///
    /// Retained until the ring closes: the platform offers no way to withdraw a
    /// registration, so its pointers may still be live.
    retired_buffer_registrations: Vec<Rc<registry::RegistryInner>>,
    /// Set while a *buffer* registration is in flight.
    ///
    /// A registration is adopted when it completes, not when it is requested, so
    /// this covers the window between. Deliberately scoped to buffer
    /// registrations: `register_files` shares this payload field, the slab and
    /// the future, so a variant-blind flag would let a file registration
    /// completing first clear it while a buffer registration was still pending —
    /// reopening the very window it exists to close.
    registration_in_flight: bool,
    /// A weak reference to the enclosing `Rc`, so a registration can ask the
    /// driver about its own state without keeping the driver alive.
    self_weak: Weak<RefCell<DriverInner>>,
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
    /// Durable record that work was queued or shutdown was requested.
    ///
    /// A flag rather than an edge on purpose. Within one pass the driver
    /// releases its borrow and calls back into caller code twice — waking
    /// futures and delivering reports — and either can queue work, *after* the
    /// pass has already read whether it should keep going. A signal that had to
    /// be caught in flight could be raised in that window and missed; a flag
    /// raised there is still set when the park consults it.
    ///
    /// Set by [`DriverInner::mark_nudged`], consumed only by the park.
    nudged: bool,
    /// The waker of whichever task is currently parked in [`Driver::drive`].
    ///
    /// Occupied **only while the driver is parked**. That is what makes it safe
    /// for code reached from inside the driver's own pass to raise a nudge
    /// without waking anything: there is no waker to take, and the running
    /// driver consults `nudged` before it parks.
    driver_waker: Option<Waker>,
    /// Set when entries are queued but not yet accepted by the kernel.
    ///
    /// While set, a retry is owed and no completion can arrive to prompt it, so
    /// the driver must schedule its own wake.
    ///
    /// Deliberately *not* the same state as `nudged`, though the two are raised
    /// at the same moments. This one means "entries are queued and the kernel
    /// has not taken them" and is cleared by a successful `submit_pending` at
    /// the head of every pass, before the park decision is reached; it is also
    /// never set by `Handle::escalate`. Reusing it as the wakeup record would
    /// leave a shutdown request with no record at all, and would tie the wakeup
    /// guarantee to submission-retry state, so a later change to when
    /// submission is retried would silently change when the driver can be woken.
    pending_submit: bool,
    /// How far along shutdown is. Escalation is monotonic: a graceful shutdown
    /// can become immediate, never the reverse.
    shutdown: ShutdownMode,
    /// Test seam: set when teardown is entered, before any draining happens.
    ///
    /// Deliberately not consulted by anything in production. Its purpose is to
    /// record that no re-entry guard keys on "teardown has begun" — the guards
    /// key on [`DriverInner::torn_down`], meaning *finished*. A drain abandoned
    /// part-way, because the future driving it was dropped or a caller's waker
    /// panicked, has to be resumed rather than skipped; a guard on "started"
    /// would let a half-torn-down driver release memory the kernel can still
    /// reach. Tests use it to prove they abandoned a drain that had really
    /// started.
    #[cfg(test)]
    teardown_started: bool,
    /// Set only once the ring is closed and every resource has been released.
    ///
    /// Gates cancellation and submission, and resolves shutdown-completion
    /// waiters.
    torn_down: bool,
    /// Wakers of futures awaiting the end of teardown, each tagged so a
    /// re-poll replaces its own entry rather than adding a second, and a drop
    /// removes it. Without both, a `select!` loop would grow this without bound.
    shutdown_waiters: Vec<(u64, Waker)>,
    /// Source of the tags above.
    next_waiter_id: u64,
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
    /// Test seam: withhold this many rounds of completion reaping.
    ///
    /// Produces a driver that refuses to *observe* completions, so an operation
    /// stays outstanding for a known number of drain steps. That is what makes
    /// "the buffer was not released before its operation reported" observable at
    /// all.
    ///
    /// Counted rather than a flag on purpose: the drain is unbounded, so a seam
    /// that withheld reaping unconditionally would produce a test that hangs
    /// forever instead of failing — a CI timeout rather than an assertion.
    #[cfg(test)]
    withhold_reaps: u32,
    /// Test seam: refuse to enqueue this many cancellation requests.
    ///
    /// `BuildIoRingCancelRequest` has no reliably reproducible failure mode, so
    /// the retry path — the one that keeps a refused request from making an
    /// operation permanently uncancellable — cannot be exercised without
    /// injecting one.
    ///
    /// Counted rather than a flag for the same reason reaping is: the drain
    /// retries until it succeeds, so an unconditional refusal under an immediate
    /// shutdown produces a test that never terminates.
    #[cfg(test)]
    fail_next_cancels: u32,
    /// Test seam: how many times a cancellation reached the enqueue attempt.
    ///
    /// Counting attempts is what stops the retry test passing vacuously: an
    /// operation that simply completed on its own would satisfy "everything
    /// resolved" without a single retry having happened.
    #[cfg(test)]
    cancel_attempts: u32,
    /// Test seam: how many times the driver actually suspended.
    ///
    /// Counted rather than a flag because the park tests need both "did this
    /// pass park at all" and "how many times", and a boolean answers neither.
    #[cfg(test)]
    parks: u32,
    /// Test seam: how many passes of the driver loop have run.
    ///
    /// Distinct from `parks`, and the distinction is load-bearing. A driver
    /// whose park ignored the nudge entirely would still park once per poll, so
    /// `parks` alone cannot tell "the nudge caused another pass" from "the poll
    /// re-armed the wait and suspended again". Only a pass counter separates
    /// them, and that is exactly what the wakeup tests must assert on.
    #[cfg(test)]
    passes: u32,
    /// Test seam: build the next buffer registration with no descriptors.
    ///
    /// The platform accepts a zero-*extent* descriptor but rejects a
    /// zero-*count* registration, and it does so at completion time rather than
    /// at build time. That is the only way to reach the completion-failure path,
    /// and the public API refuses an empty registration up front, so it has to
    /// be injected here.
    #[cfg(test)]
    fail_next_registration: bool,
    /// How many submissions the driver has completed successfully.
    ///
    /// Paired with [`DriverInner::submitted_entries`] to make the crate's
    /// central design claim — that one `SubmitIoRing` covers every entry built
    /// since the last one — an *observed* number rather than an asserted one.
    /// The design documentation presents that amortisation as the main thing a
    /// completion-based design offers over a thread pool, and until these two
    /// counters existed nothing outside this module could tell a submission
    /// covering sixty-four entries from sixty-four submissions covering one.
    ///
    /// Present in normal builds, not behind `#[cfg(test)]`, precisely because
    /// the benchmark crate compiles `win-ioring` as an ordinary dependency and
    /// is where the observation is wanted. **This is not dead weight: it is the
    /// only seam through which batching is visible from outside.**
    ///
    /// A plain `u64` rather than an atomic or a `Cell`: `DriverInner` is
    /// `!Send` and `!Sync` — it holds `Rc`, `Weak` and `RefCell` — so it is
    /// reachable from exactly one thread, and every mutation here happens under
    /// the `&mut self` of a single `RefCell` borrow. `u64` rather than `u32`
    /// because entries accumulate across a whole benchmark run rather than
    /// counting passes.
    submissions: u64,
    /// How many entries those submissions covered in total.
    ///
    /// See [`DriverInner::submissions`]. Dividing this by that yields the mean
    /// number of entries a submission carried, which is the figure the batching
    /// claim is about.
    submitted_entries: u64,
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

    /// Records that the driver owes a pass.
    ///
    /// Safe to call from anywhere, including from inside the driver's own pass:
    /// it touches only durable state and wakes nothing. Code reached from
    /// within a pass may call this alone, because no waker can be stored while
    /// the driver is running and the running driver consults the flag before it
    /// parks. Code entering from outside a pass must also take the waker — see
    /// [`DriverInner::nudge`].
    fn mark_nudged(&mut self) {
        self.nudged = true;
    }

    /// Takes the parked driver's waker, for the caller to wake after releasing
    /// its borrow.
    ///
    /// Returns `None` when the driver is not parked, which is the common case
    /// for anything called from inside a pass.
    #[must_use = "the returned waker must be woken after releasing the borrow"]
    fn take_driver_waker(&mut self) -> Option<Waker> {
        self.driver_waker.take()
    }

    /// Records that the driver owes a pass and takes its waker if it is parked.
    ///
    /// The form every external entry point uses: raise the durable record, then
    /// wake whoever is waiting on it, once the borrow is gone.
    #[must_use = "the returned waker must be woken after releasing the borrow"]
    fn nudge(&mut self) -> Option<Waker> {
        self.mark_nudged();
        self.take_driver_waker()
    }

    /// Consumes the nudge, reporting whether one was outstanding.
    ///
    /// Called only from the park. Consuming anywhere else would let the driver
    /// park on work it has been told about and never told about again.
    #[must_use = "consuming the nudge without acting on it loses the wakeup"]
    fn take_nudge(&mut self) -> bool {
        std::mem::take(&mut self.nudged)
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
            Ok(entries) => {
                // The one place batching becomes observable. Recorded before
                // anything else in this arm so the count cannot be skipped by a
                // later early return, and with no branch of its own: the
                // submission path's behaviour must be exactly what it was.
                self.submissions += 1;
                self.submitted_entries += u64::from(entries);
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
    /// be outstanding. Its resources are freed when the ring closes.
    ///
    /// Returns the new buffer registration, so the caller can hand a reference
    /// to it back to the application. A file registration yields nothing.
    fn adopt_registration(
        &mut self,
        pending: PendingRegistration,
    ) -> Option<Rc<registry::RegistryInner>> {
        match pending {
            PendingRegistration::Buffers {
                buffers,
                pointers,
                extents,
                watermarks,
                descriptors,
            } => {
                let registry = registry::RegistryInner::new(
                    buffers,
                    pointers,
                    extents,
                    watermarks,
                    descriptors,
                    self.self_weak.clone(),
                );
                let previous = self.buffer_registration.replace(Rc::clone(&registry));
                if let Some(previous) = previous {
                    self.retired_buffer_registrations.push(previous);
                }
                Some(registry)
            }
            PendingRegistration::Files { files } => {
                let previous = self.file_registration.replace(FileRegistration {
                    count: files.len(),
                    _files: files,
                });
                if let Some(previous) = previous {
                    self.retired_file_registrations.push(previous);
                }
                None
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
        let extent = registration
            .extent(index)
            .ok_or(Error::InvalidRegisteredIndex { index })?;
        let bound = if for_write {
            registration
                .initialized(index)
                .ok_or(Error::InvalidRegisteredIndex { index })?
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
        // Only a completed teardown refuses. A shutdown in progress is exactly
        // when cancellation matters most: the drain issues its requests through
        // here, and refusing them would leave the drain waiting for operations
        // nobody has asked to stop.
        if self.torn_down {
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
        // As in `request_cancel`: only a completed teardown refuses. Gating this
        // on "a shutdown is in progress" is what previously made teardown unable
        // to cancel anything, since teardown set that flag before draining.
        if self.torn_down {
            return;
        }
        // The platform reads a cancellation target of zero as "everything on
        // this handle", which would abort operations this driver never issued —
        // including another ring's, elsewhere in the process. Token encoding
        // makes zero unreachable (see `FIRST_GENERATION` in `slab`); this is the
        // backstop, because the consequence of that invariant lapsing is silent
        // damage outside the crate rather than a failure inside it. Declining to
        // cancel is safe: the operation simply runs to completion, and the drain
        // waits for it.
        if token.as_user_data() == 0 {
            debug_assert!(false, "an operation token must never be zero");
            return;
        }
        // Only submitted operations can be cancelled. A described one has
        // nothing in the queue, and a built one has not reached the kernel, so
        // there is nothing for the platform to find. A token that has since
        // completed lands here too, and is likewise ignored.
        if self.slab.state(token).map(|(lifecycle, _)| lifecycle) != Some(Lifecycle::Submitted) {
            return;
        }

        // The cancellation must name the same file the target named — an owned
        // file by handle, or a registered one by index. An operation with
        // neither is a registration, which has nothing to cancel.
        let target = {
            let payload = self
                .slab
                .payload_mut(token)
                .and_then(|p| p.downcast_mut::<OpPayload>());
            match payload {
                Some(p) => match (p.file.clone(), p.registered_file) {
                    (Some(file), _) => CancelTarget::Owned(file),
                    (None, Some(index)) => CancelTarget::Registered(index),
                    (None, None) => return,
                },
                None => return,
            }
        };

        // Refused if a cancellation has ever been issued for this operation,
        // which is what makes a repeat request a no-op.
        let Some(cancel_token) = self.slab.register_cancel(token) else {
            return;
        };

        let builder = crate::io_ring::ops::CancelOp::builder()
            .with_op_to_cancel(token.as_user_data())
            .with_user_data(cancel_token.as_user_data());
        let op = match &target {
            CancelTarget::Owned(file) => builder.with_raw_handle(file.raw_handle()),
            CancelTarget::Registered(index) => builder.with_registered_handle_index(*index),
        }
        .build();
        let Ok(op) = op else {
            // Nothing was queued, so withdraw the bookkeeping we just took.
            // Recorded as never-enqueued rather than completed, so a later
            // attempt — the drain's, in particular — can try again.
            self.slab.cancel_request_not_enqueued(cancel_token);
            return;
        };

        // Hold the file open until the cancellation's own completion arrives.
        // Do this before building, so the hold is in place no matter what. A
        // registered handle needs no hold: the driver owns it for the life of
        // the ring.
        if let CancelTarget::Owned(file) = target {
            self.cancel_holds.push((cancel_token, file));
        }

        #[cfg(test)]
        {
            self.cancel_attempts = self.cancel_attempts.saturating_add(1);
        }
        #[cfg(test)]
        let refused = {
            let refused = self.fail_next_cancels > 0;
            if refused {
                self.fail_next_cancels -= 1;
            }
            refused
        };
        #[cfg(not(test))]
        let refused = false;

        // `||` short-circuits, so an injected refusal never reaches the platform.
        //
        // SAFETY: an owned file's handle is kept open by the hold just pushed,
        // which is released only when this cancellation's own completion is
        // dequeued; a registered handle is owned by the driver until the ring
        // closes.
        let not_enqueued = refused || unsafe { self.ring.build_cancel_request(op) }.is_err();
        if not_enqueued {
            // Failing to enqueue a cancellation is explicitly not an error: the
            // target simply runs to completion. Undo the bookkeeping so the
            // target's slot is not left waiting for a completion that will
            // never arrive — and record it as never-enqueued rather than
            // completed, so the drain can request it again. Without that, one
            // refused request would silently make the operation permanently
            // uncancellable, turning an immediate shutdown into a graceful one.
            self.cancel_holds.retain(|(t, _)| *t != cancel_token);
            self.slab.cancel_request_not_enqueued(cancel_token);
            return;
        }

        self.pending_submit = true;
        // Only mark. This is reached from inside the driver's own pass — via
        // `submit_pending` and `drain_step` — where no waker can be stored, and
        // from `request_cancel`, whose external callers take the waker
        // themselves once their borrow is gone.
        self.mark_nudged();
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
        #[cfg(test)]
        if self.withhold_reaps > 0 {
            self.withhold_reaps -= 1;
            return wakers;
        }
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
            // by whoever happens to poll the awaiting future. On success the
            // collection goes to the caller through the result slot; on failure
            // the resources do.
            if let Some(pending) = payload.pending_registration.take() {
                // Scoped to buffer registrations: `register_files` shares this
                // field and this fork, so clearing on a file completion would
                // reopen the window while a buffer registration was still
                // pending.
                if matches!(pending, PendingRegistration::Buffers { .. }) {
                    self.registration_in_flight = false;
                }
                if result.is_ok() {
                    if let Some(registered) = self.adopt_registration(pending) {
                        buffer = Some(
                            Box::new(registry::RegisteredBuffers::from_inner(registered))
                                as Box<dyn Any>,
                        );
                    }
                } else {
                    buffer = Some(Box::new(pending) as Box<dyn Any>);
                }
            }

            // A completed transfer into a registered buffer may extend that
            // buffer's initialized prefix. The registration is reached through
            // the handle the operation carried, so a completion belonging to a
            // superseded registration extends that one and cannot disturb its
            // replacement.
            if let Some((index, offset)) = payload.registered_buffer
                && let Ok(transferred) = &result
                && let Some(handle) = buffer
                    .as_ref()
                    .and_then(|b| b.downcast_ref::<registry::RegisteredBuf>())
            {
                handle
                    .registry()
                    .raise_initialized(index, offset as usize, *transferred as usize);
            }

            // A registered handle's own view of how much it holds is a cached
            // copy, so refresh it now the registration has been updated.
            if let Some(handle) = buffer
                .as_mut()
                .and_then(|b| b.downcast_mut::<registry::RegisteredBuf>())
            {
                handle.refresh();
            }

            let mut slot = payload.slot.borrow_mut();
            slot.completed = Some((result, buffer));
            if let Some(waker) = slot.waker.take() {
                wakers.push(waker);
            }
        }
    }

    /// Performs one bounded unit of teardown work.
    ///
    /// Returns the wakers of any futures resolved along the way, for the caller
    /// to wake after releasing its borrow, together with what the step
    /// established. Splitting teardown into steps is what lets the caller wake
    /// futures, deliver reports, and re-read the shutdown mode between them —
    /// none of which can happen while this borrow is held.
    #[must_use = "the returned wakers must be woken after releasing the borrow"]
    fn drain_step(&mut self) -> (Vec<Waker>, StepOutcome) {
        #[cfg(test)]
        {
            self.teardown_started = true;
        }

        // Slots that never reached the queue hold nothing the platform can
        // touch, so they can be resolved here and now. Believed unreachable in
        // practice — every insert is followed by a build or a cleanup within the
        // same borrow — but the drain must not depend on that, because such a
        // slot would otherwise be counted as outstanding forever.
        let mut wakers = self.resolve_unqueued();

        // Hand over anything queued but not yet taken. One attempt per step: a
        // retry loop here would spin under this borrow and starve the reporting
        // that a stalled drain depends on.
        self.submit_pending();

        if self.shutdown >= ShutdownMode::Immediate {
            // Ask the platform to abandon everything it currently holds.
            // Requests that fail to enqueue are recorded as never-requested, so
            // the next step tries them again.
            for token in self.slab.submitted_uncancelled() {
                self.issue_cancel(token);
            }
        }

        // Never issue a waiting submission while a queue entry is still
        // unaccepted. `SubmitIoRing` submits *and* waits in one call, and the
        // wrapper discards its submitted-entry count on error — so a waiting
        // call that times out could hand entries to the kernel while leaving
        // them still marked unsubmitted. Residue detection would then conclude
        // the kernel held nothing and release buffers it is writing into.
        let built = self.slab.built().len();
        if built == 0 {
            let waiting = self.slab.outstanding();
            if waiting > 0 {
                // A wait count blocks until that many completions are available
                // or the timeout expires, which is how the driver waits without
                // a timer of its own. A timeout is reported as an error and is
                // normal progress, not a failure.
                let _ = self.ring.submit(waiting, DRAIN_TIMEOUT_MS);
            }
        }

        wakers.extend(self.reap_completions());

        if self.slab.outstanding() == 0 {
            return (wakers, StepOutcome::Quiescent);
        }

        // Nothing the platform accepted remains, yet entries are still queued
        // and submission keeps failing. Those entries were never seen by the
        // kernel, so closing the ring discards them and their buffers were never
        // exposed. This is the only case in which abandoning a queue entry is
        // sound, and it is bounded so it cannot be reached by a merely slow ring.
        if self.slab.awaiting_kernel() == 0 && self.submit_failures >= RESIDUE_ATTEMPTS {
            return (wakers, StepOutcome::UnsubmittableResidue);
        }

        (wakers, StepOutcome::Progressing)
    }

    /// Resolves slots that hold no queue entry, returning their buffers.
    #[must_use = "the returned wakers must be woken after releasing the borrow"]
    fn resolve_unqueued(&mut self) -> Vec<Waker> {
        let mut wakers = Vec::new();
        for token in self.slab.described() {
            if let Some(payload) = self.slab.complete(token) {
                wakers.extend(resolve_abandoned(payload));
            }
        }
        wakers
    }

    /// Closes the ring and releases everything it was keeping alive.
    ///
    /// Only ever called once nothing the platform accepted remains outstanding.
    /// Aborts if the ring cannot be closed: while it is open the kernel may
    /// still reach these resources, so releasing them would be a use-after-free
    /// and keeping them would leak with no prospect of recovery.
    #[must_use = "the returned wakers must be woken after releasing the borrow"]
    fn close_and_release(&mut self) -> Vec<Waker> {
        let mut attempts = 0;
        while self.ring.close().is_err() {
            attempts += 1;
            if attempts >= CLOSE_ATTEMPTS {
                // Not recoverable in either direction. Aborting is the only
                // option that is not a use-after-free.
                abort_with("the I/O ring could not be closed at shutdown");
            }
        }

        // The ring is closed, so nothing here is reachable by the kernel any
        // more. Any payload still present belongs to a queue entry the platform
        // never accepted; its future is owed a result and its buffer back.
        let mut wakers = Vec::new();
        for payload in self.slab.drain() {
            wakers.extend(resolve_abandoned(payload));
        }
        self.cancel_holds.clear();
        self.buffer_registration = None;
        self.retired_buffer_registrations.clear();
        self.registration_in_flight = false;
        self.file_registration = None;
        self.retired_file_registrations.clear();
        // Not needed for correctness — a resolved park leaves the slot empty,
        // and nothing consumes a nudge after teardown — but both fields are
        // documented as transient, and leaving either set on a torn-down driver
        // reads as an oversight. A waker surviving here would additionally keep
        // the driving task's allocation alive through a reference cycle.
        self.driver_waker = None;
        self.nudged = false;
        self.torn_down = true;
        // Everything is released, so anyone awaiting the end of teardown can be
        // told. Collected rather than woken here, like every other waker: an
        // executor may poll inline and call straight back into the driver.
        wakers.extend(self.shutdown_waiters.drain(..).map(|(_, w)| w));
        wakers
    }

    /// Reports a drain that is taking an unusual number of steps.
    ///
    /// A drain is unbounded by design, so a stalled one would otherwise be
    /// indistinguishable from a hang. Throttled the same way persistent
    /// submission failures are, so a long shutdown does not flood the observer.
    fn report_slow_drain(&mut self, steps: u32) {
        if steps < SLOW_DRAIN_STEPS || !steps.is_multiple_of(SLOW_DRAIN_INTERVAL) {
            return;
        }
        let outstanding = self.slab.outstanding();
        if outstanding == 0 {
            return;
        }
        self.deferred_reports
            .push(Error::ShutdownStalled { outstanding });
    }
}

impl Drop for DriverInner {
    fn drop(&mut self) {
        // A backstop, not a routine path: `Driver` holds a strong reference, so
        // this can only run after `Drop for Driver` has already torn down. If it
        // ever runs with the ring still open, the kernel may still reach the
        // buffers, file handles and registrations about to be freed — so ending
        // the process is the only option that is not a use-after-free.
        //
        // Deliberately not also requiring work to be outstanding: registrations
        // stay reachable by the kernel until the ring closes, so an open ring is
        // unsafe to release from even with nothing in flight.
        if !self.torn_down {
            abort_with("the driver was destroyed while the I/O ring was still open");
        }
    }
}

/// After how many drain steps a shutdown is considered slow enough to report.
const SLOW_DRAIN_STEPS: u32 = 8;

/// How often to repeat the report once a drain is considered slow.
const SLOW_DRAIN_INTERVAL: u32 = 8;

/// Resolves a payload the driver ended itself, returning the caller's resources.
fn resolve_abandoned(mut payload: Box<dyn Any>) -> Option<Waker> {
    let payload = payload.downcast_mut::<OpPayload>()?;
    // A flush carries no buffer, and a registration carries its resources
    // somewhere else entirely — so take both, exactly as the completion path
    // does. Returning only `buffer` would hand a registration's caller an empty
    // vector while its buffers were dropped here.
    let mut resources = payload.buffer.take();
    if let Some(pending) = payload.pending_registration.take() {
        resources = Some(Box::new(pending) as Box<dyn Any>);
    }
    let mut slot = payload.slot.borrow_mut();
    // They must travel with the result. Resolving without them would leave the
    // caller's future to panic when it tries to recover its buffer.
    slot.completed = Some((Err(Error::AbandonedAtShutdown), resources));
    slot.waker.take()
}

/// Ends the process, reporting why.
///
/// Reached only where every alternative is worse than stopping: releasing memory
/// the kernel may still write into, or continuing with a ring that can neither
/// drain nor close.
fn abort_with(reason: &str) -> ! {
    eprintln!("win-ioring: aborting: {reason}");
    std::process::abort()
}

/// What one drain step established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepOutcome {
    /// Nothing is outstanding; the ring can be closed.
    Quiescent,
    /// Work remains and the drain should continue.
    Progressing,
    /// Only queue entries the platform has repeatedly refused remain. They were
    /// never accepted, so closing the ring discards them safely.
    UnsubmittableResidue,
}

/// How many consecutive submission failures mark a queue entry unsubmittable.
const RESIDUE_ATTEMPTS: u32 = 8;

/// How many times to retry closing the ring before giving up and aborting.
const CLOSE_ATTEMPTS: u32 = 3;

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
///
/// # Dropping blocks
///
/// Dropping a `Driver` tears the ring down synchronously, and that drain is
/// unbounded: it ends when the kernel holds nothing. Closing the ring neither
/// cancels in-flight work nor waits for it, and the platform may keep writing
/// into buffers afterwards, so giving up early would mean freeing memory the
/// kernel is still using.
///
/// In practice this is brief, but an operation that never completes and cannot
/// be cancelled has no exit, and dropping the driver will not return. Prefer
/// awaiting [`Driver::drive`], which drains cooperatively and can be escalated
/// with [`Handle::shutdown_now`] while it runs.
pub struct Driver {
    inner: Rc<RefCell<DriverInner>>,
    /// Held outside `DriverInner` so it is never invoked under the driver's
    /// borrow; an observer may call back into the driver.
    on_error: Option<ErrorObserver>,
    /// Signalled by the kernel when completions are available, and waited on
    /// through one thread-pool registration armed for the driver's whole life.
    ///
    /// The driver's only waited object, and — until named pipes — the crate's
    /// only thread boundary. A [`crate::pipe::Server`]'s accept is the second:
    /// `ConnectNamedPipe` is not an operation the ring can build, so it runs as
    /// an overlapped Win32 call whose completion reaches the crate through the
    /// same `RegisterWaitForSingleObject` machinery, on the same thread pool.
    /// It is a boundary this driver never sees — the accept occupies no slab
    /// entry and the drain does not wait for it — but it is a boundary, and the
    /// claim that there is only one has been wrong since pipes shipped.
    ///
    /// What used to be a second event — carrying "the application queued work
    /// for you" — is now a flag and a waker inside `DriverInner`, because every
    /// type that could raise it holds `Rc`s and so lives on the driver's own
    /// thread. Routing a same-thread signal through a kernel object and an
    /// operating-system thread pool cost more than the I/O it was announcing.
    completion_wait: ArmedEvent,
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
        // Allocate everything that can fail *before* handing the ring its
        // completion event. Registering the event first would mean a later
        // failure dropped the event's handle while the ring still referred to
        // it, which is exactly what `set_io_ring_completion_event` forbids.
        // Arming the wait is part of that: it can fail too, and must fail here
        // rather than leaving a driver that can never be woken.
        let completion_wait = match ArmedEvent::new() {
            Ok(completion_wait) => completion_wait,
            Err(e) => {
                // Nothing else owns the ring yet, and it has no `Drop`.
                let _ = ring.close();
                return Err(e);
            }
        };

        // SAFETY: `completion_wait` becomes a field of the `Driver` built below,
        // and the ring is always closed before that handle is. Four things
        // uphold that, and all four are load-bearing:
        //
        // - `Drop for Driver` drains and then calls `close_and_release`, which
        //   closes the ring. It runs to completion before any `Driver` field —
        //   `completion_wait` among them — is dropped.
        // - that loop is fenced by `AbortOnUnwind`, because a panic escaping it
        //   would start dropping those fields with the ring still open, and
        //   `Drop for Driver` would not run again to fix it.
        // - `close_and_release` aborts rather than returning if the ring cannot
        //   be closed, and `Drop for DriverInner` aborts if it is ever reached
        //   with the ring still open.
        // - `ArmedEvent` owns both the thread-pool registration and the handle,
        //   and drops them in that order, so the wait is never left pending on a
        //   closed handle. Because the ring is closed first, no new signal can
        //   arrive while that unregister runs.
        //
        // Nothing fallible remains between here and that `Driver`, so there is
        // no path on which the event is dropped first.
        if let Err(e) = unsafe { ring.set_io_ring_completion_event(completion_wait.handle()) } {
            let _ = ring.close();
            return Err(e);
        }

        Ok(Self {
            // `new_cyclic` rather than `new` so the driver can hand a weak
            // reference to itself to each registration, which needs to ask about
            // shutdown and supersession without keeping the driver alive.
            inner: Rc::new_cyclic(|self_weak| {
                RefCell::new(DriverInner {
                    ring,
                    slab: OpSlab::new(),
                    buffer_registration: None,
                    retired_buffer_registrations: Vec::new(),
                    registration_in_flight: false,
                    self_weak: self_weak.clone(),
                    file_registration: None,
                    retired_file_registrations: Vec::new(),
                    cancel_holds: Vec::new(),
                    deferred_cancels: Vec::new(),
                    nudged: false,
                    driver_waker: None,
                    pending_submit: false,
                    shutdown: ShutdownMode::Running,
                    #[cfg(test)]
                    teardown_started: false,
                    shutdown_waiters: Vec::new(),
                    next_waiter_id: 0,
                    torn_down: false,
                    deferred_reports: Vec::new(),
                    submit_failures: 0,
                    #[cfg(test)]
                    fail_next_submits: 0,
                    #[cfg(test)]
                    withhold_reaps: 0,
                    #[cfg(test)]
                    fail_next_cancels: 0,
                    #[cfg(test)]
                    cancel_attempts: 0,
                    #[cfg(test)]
                    parks: 0,
                    #[cfg(test)]
                    passes: 0,
                    #[cfg(test)]
                    fail_next_registration: false,
                    submissions: 0,
                    submitted_entries: 0,
                })
            }),
            on_error: observer,
            completion_wait,
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
        }
    }

    /// Runs the driver until shutdown is requested, then closes the ring.
    ///
    /// Spawn this on your executor.
    ///
    /// # Poll exactly one of these per driver
    ///
    /// The driver records the waker of whichever task is parked in a single
    /// slot. Two `drive` futures polled concurrently against one `Driver` would
    /// each overwrite the other's waker, and the one whose waker was evicted
    /// would not be re-polled by a completion. Nothing enforces this — `&self`
    /// permits a second call — so it is stated here.
    pub async fn drive(&self) {
        loop {
            let (shutting_down, retry_owed, wakers, reports) = {
                let mut inner = self.inner.borrow_mut();
                #[cfg(test)]
                {
                    inner.passes += 1;
                }
                inner.submit_pending();
                let wakers = inner.reap_completions();
                let reports = inner.take_reports();
                (
                    inner.shutdown != ShutdownMode::Running,
                    inner.pending_submit,
                    wakers,
                    reports,
                )
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

            self.park().await;
        }

        self.run_teardown_async().await;
    }

    /// Suspends until the driver owes another pass.
    ///
    /// The whole wakeup guarantee lives in [`Park::poll`]; see its comment.
    fn park(&self) -> Park<'_> {
        Park { driver: self }
    }

    /// Drains cooperatively, yielding between steps.
    ///
    /// Yielding is what keeps a graceful shutdown escalatable. Everything here
    /// is `!Send`, so `Handle::shutdown_now` can only be called from this very
    /// thread — a drain that blocked it outright would make escalation
    /// unreachable, and graceful mode never cancels, so escalation is the only
    /// way out of a stalled one. The mode is therefore re-read every step.
    ///
    /// No unwind guard is needed here. A panic escaping this unwinds out of
    /// `drive`, the `Driver` is dropped, and `Drop for Driver` re-enters and
    /// finishes the drain — which works only because re-entry is guarded on
    /// `torn_down` (finished) rather than on teardown having merely started.
    async fn run_teardown_async(&self) {
        let mut steps: u32 = 0;
        loop {
            let (wakers, reports, outcome) = {
                let mut inner = self.inner.borrow_mut();
                if inner.torn_down {
                    return;
                }
                let (wakers, outcome) = inner.drain_step();
                steps = steps.saturating_add(1);
                inner.report_slow_drain(steps);
                let reports = inner.take_reports();
                (wakers, reports, outcome)
            };
            for waker in wakers {
                waker.wake();
            }
            self.flush_reports(reports);

            if outcome != StepOutcome::Progressing {
                break;
            }
            yield_now().await;
        }

        let (wakers, reports) = {
            let mut inner = self.inner.borrow_mut();
            let wakers = inner.close_and_release();
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
        // A drop nobody asked for is an abrupt one: nothing is left to observe
        // results, so waiting for work to finish on its own would be waiting for
        // nothing. Escalate before draining so the drain cancels.
        {
            let mut inner = self.inner.borrow_mut();
            if inner.torn_down {
                return;
            }
            inner.shutdown = inner.shutdown.max(ShutdownMode::Immediate);
        }

        // Unlike the async loop, this one *does* need an unwind guard. A panic
        // escaping here starts unwinding at a point where `Drop for Driver` will
        // not run again, while the remaining fields still drop in declaration
        // order — including the completion event, whose handle must not close
        // while the ring can still signal it.
        let guard = AbortOnUnwind;
        let mut steps: u32 = 0;
        loop {
            let (wakers, reports, outcome) = {
                let mut inner = self.inner.borrow_mut();
                let (wakers, outcome) = inner.drain_step();
                steps = steps.saturating_add(1);
                inner.report_slow_drain(steps);
                let reports = inner.take_reports();
                (wakers, reports, outcome)
            };
            for waker in wakers {
                waker.wake();
            }
            self.flush_reports(reports);

            if outcome != StepOutcome::Progressing {
                break;
            }
        }

        let (wakers, reports) = {
            let mut inner = self.inner.borrow_mut();
            let wakers = inner.close_and_release();
            let reports = inner.take_reports();
            (wakers, reports)
        };
        // The ring is closed and everything is released, so nothing the kernel
        // can reach remains and the fields below are safe to drop. Ending the
        // process for a panic in a caller's waker past this point would exceed
        // the guard's stated justification, so it is released here rather than
        // after the callbacks.
        std::mem::forget(guard);
        for waker in wakers {
            waker.wake();
        }
        self.flush_reports(reports);
    }
}

/// Ends the process if dropped while a panic is unwinding.
///
/// Used to fence teardown sections that call back into caller code — waking
/// futures and delivering reports — because unwinding out of a half-finished
/// teardown would drop the driver's fields while the ring is still open.
///
/// `pub(crate)` so the pipe server's teardown can take the same fence. Its
/// half-finished state is the same shape: an `OVERLAPPED` the kernel is writing
/// into, released only once a cancel-and-collect has established otherwise.
pub(crate) struct AbortOnUnwind;
impl Drop for AbortOnUnwind {
    fn drop(&mut self) {
        abort_with("a panic escaped teardown while the I/O ring was still open");
    }
}

/// Suspends the driver until something gives it work to do.
///
/// Two things can: the application queuing work or asking for shutdown, which
/// happens on this very thread, and the platform announcing completions, which
/// does not.
struct Park<'a> {
    driver: &'a Driver,
}

impl Future for Park<'_> {
    type Output = ();

    /// # Why no wakeup can be lost here
    ///
    /// The nudge is consumed and the platform wait is armed inside this one
    /// `poll`, on one thread, with no `await`, no callback into caller code and
    /// no re-entrant path between the two. A nudge can only be raised by code
    /// holding an `Rc` to the driver, which is to say by code on the driver's
    /// own thread — and while this `poll` runs, that thread is running this
    /// `poll`. So a nudge is either
    ///
    /// - raised before the check below, and observed by it; or
    /// - raised after this returns `Pending`, by which time the waker stored
    ///   below is visible to `take_driver_waker`, so the nudger wakes us.
    ///
    /// There is no third case. The borrow released between the two steps runs
    /// only driver-private code and cannot reach a site that raises a nudge.
    ///
    /// Being woken from inside the driver's own pass — which happens when a
    /// future woken by this pass queues more work inline — is harmless: an
    /// executor cannot re-enter a future it is currently polling, so the wake
    /// degrades to marking the task ready, which is what the flag already
    /// guarantees.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;

        // Any waker this park evicts is dropped *after* the borrow is released.
        // Dropping a `Waker` runs executor code, and if it is the last reference
        // to a task holding an operation future, that future's `Drop` re-enters
        // the driver — which under a live borrow would panic. Same discipline as
        // the `#[must_use]` waker returns elsewhere in this file.
        let evicted;
        {
            let mut inner = this.driver.inner.borrow_mut();
            if inner.take_nudge() {
                evicted = inner.driver_waker.take();
                drop(inner);
                drop(evicted);
                // Give up the waker the armed wait may be holding for us. No
                // completion can be lost by doing so: its signal is sticky, so
                // one raised with no waker recorded is still seen by the next
                // poll.
                this.driver.completion_wait.release_waker();
                return Poll::Ready(());
            }
            // Record who is driving *before* arming, so a nudge raised from here
            // on has something to wake.
            evicted = inner.driver_waker.replace(cx.waker().clone());
        }
        drop(evicted);

        match this.driver.completion_wait.poll_signalled(cx) {
            Poll::Ready(()) => {
                let evicted = this
                    .driver
                    .inner
                    .try_borrow_mut()
                    .ok()
                    .and_then(|mut inner| inner.driver_waker.take());
                drop(evicted);
                Poll::Ready(())
            }
            Poll::Pending => {
                #[cfg(test)]
                {
                    this.driver.inner.borrow_mut().parks += 1;
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for Park<'_> {
    fn drop(&mut self) {
        // A park abandoned part-way — which happens when the `drive` future
        // itself is dropped — must not leave its waker behind. Nothing is
        // stranded if it does, because `nudged` is durable and the next driver
        // to run observes it; but the slot is documented as occupied only while
        // parked, and a waker left there keeps the abandoned task alive.
        //
        // `try_borrow_mut` because this can run while the driver is mid-pass,
        // and the waker is dropped after the borrow for the reason in `poll`.
        let evicted = self
            .driver
            .inner
            .try_borrow_mut()
            .ok()
            .and_then(|mut inner| inner.driver_waker.take());
        drop(evicted);
        self.driver.completion_wait.release_waker();
    }
}

/// Resolves once the driver has finished tearing down.
///
/// Created by [`Handle::shutdown_complete`].
pub struct ShutdownComplete {
    driver: Rc<RefCell<DriverInner>>,
    /// Assigned on first poll, so a re-poll replaces this future's waker rather
    /// than registering a second one, and dropping removes exactly this entry.
    id: Option<u64>,
}

impl Future for ShutdownComplete {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let mut inner = this.driver.borrow_mut();
        if inner.torn_down {
            return Poll::Ready(());
        }

        let id = match this.id {
            Some(id) => id,
            None => {
                let id = inner.next_waiter_id;
                inner.next_waiter_id += 1;
                this.id = Some(id);
                id
            }
        };

        // Replace rather than append, or a future polled repeatedly — which is
        // to say any future in a `select!` loop — would accumulate wakers.
        if let Some(slot) = inner.shutdown_waiters.iter_mut().find(|(w, _)| *w == id) {
            slot.1 = cx.waker().clone();
        } else {
            inner.shutdown_waiters.push((id, cx.waker().clone()));
        }
        Poll::Pending
    }
}

impl Drop for ShutdownComplete {
    fn drop(&mut self) {
        let Some(id) = self.id else {
            return;
        };
        // Abandoning the wait must not leave a waker behind. `try_borrow_mut`
        // because this can run while the driver is mid-teardown.
        if let Ok(mut inner) = self.driver.try_borrow_mut() {
            inner.shutdown_waiters.retain(|(w, _)| *w != id);
        }
    }
}

/// How much work the driver's submissions to the kernel have carried.
///
/// Returned by [`Handle::submission_counts`]. The ratio of `entries` to
/// `submissions` is the mean number of entries one `SubmitIoRing` covered,
/// which is the quantity the crate's batching claim is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubmissionCounts {
    /// How many submissions to the kernel have succeeded.
    pub submissions: u64,
    /// How many entries those submissions covered in total.
    pub entries: u64,
}

/// A cloneable, single-threaded reference to a [`Driver`].
#[derive(Clone)]
pub struct Handle {
    /// Weak, so futures holding a handle cannot keep the driver alive.
    inner: Weak<RefCell<DriverInner>>,
    /// Strong, so a handle held by the caller keeps the driver usable.
    strong: Rc<RefCell<DriverInner>>,
}

impl Handle {
    /// Requests a graceful shutdown.
    ///
    /// The driver stops accepting new work, lets operations already in flight
    /// finish on their own, and closes the ring once they have. Returns
    /// immediately; see [`Handle::shutdown_complete`] to await the result.
    ///
    /// Because a graceful shutdown never cancels, it waits for as long as its
    /// slowest outstanding operation takes. Escalate with
    /// [`Handle::shutdown_now`] if that is not acceptable.
    pub fn shutdown(&self) {
        self.escalate(ShutdownMode::Graceful);
    }

    /// Requests an immediate shutdown.
    ///
    /// The driver stops accepting new work and asks the platform to cancel
    /// everything in flight that it will accept a cancellation for, then closes
    /// the ring once every operation has reported. Returns immediately; see
    /// [`Handle::shutdown_complete`] to await the result.
    ///
    /// Cancellation is a request, not a revocation: an operation may still
    /// complete normally, and registrations cannot be cancelled at all.
    pub fn shutdown_now(&self) {
        self.escalate(ShutdownMode::Immediate);
    }

    /// Records that the driver owes a pass, and wakes it if it is parked.
    ///
    /// The borrow is confined to this call and the wake happens after it, since
    /// an executor may poll the woken driver inline and call straight back in.
    fn nudge_driver(&self) {
        let waker = self.strong.borrow_mut().nudge();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Raises the shutdown mode, never lowers it.
    fn escalate(&self, mode: ShutdownMode) {
        let waker = {
            let mut inner = self.strong.borrow_mut();
            if inner.shutdown >= mode {
                return;
            }
            inner.shutdown = mode;
            inner.nudge()
        };
        // After the borrow: an executor may poll the woken task inline and call
        // straight back into the driver.
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Returns `true` if the driver has been asked to shut down, by either
    /// [`Handle::shutdown`] or [`Handle::shutdown_now`].
    pub fn is_shutting_down(&self) -> bool {
        self.strong.borrow().shutdown != ShutdownMode::Running
    }

    /// Resolves once teardown has finished and every resource has been released.
    ///
    /// Resolves immediately if that has already happened. Useful for code that
    /// holds a [`Handle`] but not the [`Driver`]; if you have the driver,
    /// awaiting [`Driver::drive`] tells you the same thing.
    ///
    /// This cannot resolve unless something is driving the driver — either
    /// [`Driver::drive`] is running, or the driver is dropped. Nothing here
    /// performs the drain on its own.
    pub fn shutdown_complete(&self) -> ShutdownComplete {
        ShutdownComplete {
            driver: Rc::clone(&self.strong),
            id: None,
        }
    }

    /// Returns the number of operations the driver is still tracking.
    pub fn outstanding(&self) -> usize {
        self.strong.borrow().slab.outstanding()
    }

    /// Returns how much work each submission to the kernel has carried.
    ///
    /// The driver batches implicitly: entries are built as operations are
    /// started and handed to the kernel by a single `SubmitIoRing` covering
    /// every entry built since the last one. That amortisation is the main
    /// thing this design offers over a thread pool, and this accessor is the
    /// **only way to see it from outside the runtime** — without it, a
    /// submission covering sixty-four entries and sixty-four submissions
    /// covering one entry each are indistinguishable to any observer.
    ///
    /// Deliberately available in normal builds rather than behind a test
    /// configuration, because the caller that wants it is the benchmark
    /// harness, which compiles this crate as an ordinary dependency. It exists
    /// to make batching measurable; it is not dead weight.
    ///
    /// Counts accumulate over the driver's whole life and never decrease, so
    /// callers interested in one interval should read the value at each end and
    /// take the difference.
    pub fn submission_counts(&self) -> SubmissionCounts {
        let inner = self.strong.borrow();
        SubmissionCounts {
            submissions: inner.submissions,
            entries: inner.submitted_entries,
        }
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
            if inner.shutdown != ShutdownMode::Running {
                return Err((Error::ShuttingDown, buffer));
            }
            if let Err(e) = inner.ring.ensure_op_supported(IORING_OP_READ) {
                return Err((e, buffer));
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
            registered_file: None,
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
        let waker = inner.nudge();
        drop(inner);

        // After the borrow: an executor may poll the woken driver inline.
        if let Some(waker) = waker {
            waker.wake();
        }

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
            if inner.shutdown != ShutdownMode::Running {
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
            registered_file: None,
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
        let waker = inner.nudge();
        drop(inner);

        // After the borrow: an executor may poll the woken driver inline.
        if let Some(waker) = waker {
            waker.wake();
        }

        Ok(WriteFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
            done: false,
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
            if inner.shutdown != ShutdownMode::Running {
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
            registered_file: None,
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
        let waker = inner.nudge();
        drop(inner);

        // After the borrow: an executor may poll the woken driver inline.
        if let Some(waker) = waker {
            waker.wake();
        }

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
        self.nudge_driver();

        // The driver adopts the registration on completion, in completion
        // order, and hands the resources back through the slot on failure.
        match (RegistrationFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
        })
        .await
        {
            Ok(registered) => Registered::Ok(
                registered.expect("a successful buffer registration yields its collection"),
            ),
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
        if inner.shutdown != ShutdownMode::Running {
            return Err((Error::ShuttingDown, buffers));
        }
        // Route 5: two registrations in flight at once would let the first adopt,
        // a handle be checked out of it, and the second retire that registration
        // underneath the handle.
        if inner.registration_in_flight {
            return Err((Error::RegistrationPending, buffers));
        }
        // Route 1: a handle outstanding when a new registration is adopted would
        // be left naming a set the platform no longer resolves against. Refusing
        // the request is what makes a stale handle unreachable rather than
        // merely detectable.
        if let Some(current) = inner.buffer_registration.as_ref()
            && let Some(index) = current.any_checked_out()
        {
            return Err((Error::BufferCheckedOut { index }, buffers));
        }
        if let Err(e) = inner.ring.ensure_op_supported(IORING_OP_REGISTER_BUFFERS) {
            return Err((e, buffers));
        }

        // Box each buffer so its address is stable for as long as the platform
        // holds a pointer to it.
        let mut boxed: Vec<Box<B>> = buffers.into_iter().map(Box::new).collect();
        let mut pointers = Vec::with_capacity(boxed.len());
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
            pointers.push(ptr);
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
            registered_file: None,
            pending_registration: Some(PendingRegistration::Buffers {
                buffers: boxed.into_iter().map(|b| b as Box<dyn Any>).collect(),
                pointers,
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
        // Set only once every fallible step above has succeeded, so a rejected
        // request never leaves it stuck. Cleared when the completion is reaped,
        // on either arm, and at teardown.
        inner.registration_in_flight = true;
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
        self.nudge_driver();

        // Nothing was taken from the caller, so there is nothing to give back.
        (RegistrationFuture {
            inner: OpFuture::pending(token, slot, Weak::clone(&self.inner)),
        })
        .await
        .map(|_| ())
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
        if inner.shutdown != ShutdownMode::Running {
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
            registered_file: None,
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

    /// Reads into a registered buffer.
    ///
    /// Takes the buffer's handle and gives it back with the result, so the bytes
    /// that arrived can be read directly. `buffer_offset` and `len` name a range
    /// within the buffer; a read may target its whole registered extent.
    ///
    /// This is the operation that names the registration. Passing a handle to
    /// [`Handle::read`] instead is valid — the memory is registered either way —
    /// but issues an ordinary read and so forgoes the optimisation.
    pub fn read_registered(
        &self,
        target: FileTarget<'_>,
        buffer: RegisteredBuf,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
    ) -> RegisteredOpFuture {
        self.registered_op(target, buffer, buffer_offset, len, file_offset, false)
    }

    /// Writes from a registered buffer.
    ///
    /// The range is bounded by how much of the buffer is initialized, so
    /// uninitialized memory is never sent to the kernel. The handle comes back
    /// with the result.
    ///
    /// As with [`Handle::read_registered`], this is the form that names the
    /// registration.
    pub fn write_registered(
        &self,
        target: FileTarget<'_>,
        buffer: RegisteredBuf,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
    ) -> RegisteredOpFuture {
        self.registered_op(target, buffer, buffer_offset, len, file_offset, true)
    }

    fn registered_op(
        &self,
        target: FileTarget<'_>,
        buffer: RegisteredBuf,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
        is_write: bool,
    ) -> RegisteredOpFuture {
        match self.try_registered_op(target, buffer, buffer_offset, len, file_offset, is_write) {
            Ok(fut) => fut,
            // Every rejection hands the caller's handle straight back, exactly
            // as the ordinary operations hand back their buffer.
            Err((e, buffer)) => RegisteredOpFuture {
                state: RegisteredOpState::Failed(Some((e, buffer))),
            },
        }
    }

    #[allow(clippy::type_complexity)]
    fn try_registered_op(
        &self,
        target: FileTarget<'_>,
        buffer: RegisteredBuf,
        buffer_offset: u32,
        len: u32,
        file_offset: u64,
        is_write: bool,
    ) -> std::result::Result<RegisteredOpFuture, (Error, RegisteredBuf)> {
        let op_code = if is_write {
            IORING_OP_WRITE
        } else {
            IORING_OP_READ
        };
        let buffer_index = buffer.index();
        {
            let inner = self.strong.borrow();
            macro_rules! reject {
                ($e:expr) => {
                    return Err(($e, buffer))
                };
            }
            if inner.shutdown != ShutdownMode::Running {
                reject!(Error::ShuttingDown);
            }
            if let Err(e) = inner.ring.ensure_op_supported(op_code) {
                reject!(e);
            }
            // The handle must belong to *this* driver's current registration.
            // Nothing else catches a handle from another driver: registrations
            // are per-driver, so an index alone cannot distinguish them, and the
            // platform would resolve it against whichever registration this ring
            // holds — writing into the wrong buffer, silently.
            match inner.buffer_registration.as_ref() {
                Some(current) if Rc::ptr_eq(current, buffer.registry()) => {}
                Some(_) => reject!(Error::RegistrationSuperseded),
                None => reject!(Error::InvalidRegisteredIndex {
                    index: buffer_index
                }),
            }
            // Reads may fill the whole registered extent; writes may only
            // source what has been initialized.
            if let Err(e) =
                inner.check_registered_buffer(buffer_index, buffer_offset, len, is_write)
            {
                reject!(e);
            }
            if let FileTarget::Registered { index } = target
                && let Err(e) = inner.check_registered_file(index)
            {
                reject!(e);
            }
        }

        let slot = Rc::new(RefCell::new(ResultSlot::new()));
        let mut inner = self.strong.borrow_mut();

        let (file_state, registered_file) = match target {
            FileTarget::Owned(file) => (Some(file.state()), None),
            FileTarget::Registered { index } => (None, Some(index)),
        };

        let payload = Box::new(OpPayload {
            // The handle is the operation's buffer, and travels exactly as an
            // owned one does.
            buffer: Some(Box::new(buffer) as Box<dyn Any>),
            file: file_state,
            slot: Rc::clone(&slot),
            registered_buffer: Some((buffer_index, buffer_offset)),
            registered_file,
            pending_registration: None,
            _sequential: None,
        });
        let token = match inner.slab.insert(payload) {
            Ok(token) => token,
            // Nothing was built, so the handle comes straight back.
            Err(payload) => {
                return Err((
                    Error::QueueFull,
                    unbox_payload_one::<RegisteredBuf>(payload),
                ));
            }
        };

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
            // The entry never reached the queue, so the handle can be taken
            // back out of the slot and returned.
            let buffer = recover_buffer::<RegisteredBuf>(&mut inner, token);
            return Err((e, buffer));
        }

        inner.slab.set_lifecycle(token, Lifecycle::Built);
        inner.pending_submit = true;
        let waker = inner.nudge();
        drop(inner);

        // After the borrow: an executor may poll the woken driver inline.
        if let Some(waker) = waker {
            waker.wake();
        }

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
        let waker = {
            let mut inner = self.strong.borrow_mut();
            inner.request_cancel(id.0);
            // `request_cancel` only marks, because it is also reached from
            // inside the driver's own pass. Entering from outside one, this is
            // where the parked driver gets woken.
            inner.take_driver_waker()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// An operation against a registered buffer.
///
/// Resolves to the transfer count together with the buffer's handle, so the
/// bytes that arrived can be read directly. The handle comes back whether the
/// operation succeeded or failed, and whether or not it ever reached the kernel.
pub struct RegisteredOpFuture {
    state: RegisteredOpState,
}

enum RegisteredOpState {
    Failed(Option<(Error, RegisteredBuf)>),
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
    type Output = BufResult<Transferred, RegisteredBuf>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            RegisteredOpState::Failed(taken) => {
                let (error, buffer) = taken.take().expect("polled after completion");
                this.state = RegisteredOpState::Done;
                Poll::Ready(BufResult::new(Err(error), buffer))
            }
            RegisteredOpState::Done => panic!("registered operation polled after completion"),
            RegisteredOpState::Waiting(op) => match op.poll_resolution(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(resolution) => {
                    this.state = RegisteredOpState::Done;
                    Poll::Ready(into_outcome::<RegisteredBuf>(resolution))
                }
            },
        }
    }
}

/// Recovers a single buffer from a slot payload whose operation never reached
/// the kernel.
fn unbox_payload_one<B: 'static>(payload: Box<dyn Any>) -> B {
    let payload = payload
        .downcast::<OpPayload>()
        .unwrap_or_else(|_| unreachable!("payload type mismatch"));
    let buffer = payload.buffer.expect("payload lost its buffer");
    *buffer.downcast::<B>().expect("buffer type mismatch")
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

/// How a cancellation names the file its target named.
///
/// The platform requires the cancellation to name the same file as the
/// operation it cancels, and a registered-handle operation named its file by
/// index rather than by handle.
enum CancelTarget {
    /// An owned file, held open until the cancellation reports.
    Owned(Rc<FileState>),
    /// A registered handle, owned by the driver for the life of the ring.
    Registered(u32),
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
    type Output =
        std::result::Result<Option<RegisteredBuffers>, (Error, Option<PendingRegistration>)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.inner.poll_resolution(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Resolution(result, returned)) => Poll::Ready(match result {
                // A buffer registration yields its collection here; a file
                // registration yields nothing, and both take this arm.
                Ok(_) => Ok(returned
                    .and_then(|b| b.downcast::<RegisteredBuffers>().ok())
                    .map(|b| *b)),
                Err(e) => Err((
                    e,
                    returned
                        .and_then(|b| b.downcast::<PendingRegistration>().ok())
                        .map(|b| *b),
                )),
            }),
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

/// How an operation ended: the kernel's terminal result, along with the buffer
/// if the operation carried one.
///
/// There is no "buffer abandoned" case. Teardown drains until every operation
/// has reported before it releases anything, so a caller always gets its buffer
/// back.
struct Resolution(Result<Transferred>, Option<Box<dyn Any>>);

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
            return Poll::Ready(Resolution(result, buffer));
        }

        // There is deliberately no third case here. Teardown resolves every
        // slot before the driver's state can be destroyed, and no operation can
        // be started afterwards, so an unresolved slot with a dead driver cannot
        // occur. Reaching this point with neither a result nor a live driver
        // would mean that invariant had been broken, and the alternatives are a
        // silent hang or an outcome with no buffer to return.
        debug_assert!(
            self.driver.strong_count() > 0,
            "an operation outlived its driver without being resolved by teardown"
        );

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
        let waker = match inner.slab.detach(self.token) {
            Some(Lifecycle::Described) => {
                // Nothing was ever built, so no queue entry references the
                // buffer and it can be released immediately.
                drop(inner.slab.complete(self.token));
                None
            }
            Some(Lifecycle::Built) => {
                // A submission queue entry references the buffer and cannot be
                // withdrawn, and the kernel has not seen the operation yet, so
                // there is nothing for the platform to cancel. The slab records
                // this slot as detached, and the driver cancels it once
                // submission promotes it.
                None
            }
            Some(Lifecycle::Submitted) => {
                // Best-effort: ask the platform to give up early. Failure here
                // is not an error and changes nothing about the buffer's
                // lifetime. This returns without waiting on the kernel.
                inner.request_cancel(self.token);
                // That only marks the nudge, because it is also reached from
                // inside the driver's own pass. A future dropped by application
                // code is outside one, so the parked driver is woken here.
                inner.take_driver_waker()
            }
            None => None,
        };
        drop(inner);
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Turns a resolution into a buffer result, recovering the caller's buffer.
fn into_outcome<B: 'static>(resolution: Resolution) -> BufResult<Transferred, B> {
    let Resolution(result, buffer) = resolution;
    let buffer = buffer.expect("a buffer-carrying operation lost its buffer");
    let buffer = *buffer.downcast::<B>().expect("buffer type mismatch");
    BufResult::new(result, buffer)
}

/// A read in progress.
pub struct ReadFuture<B> {
    state: BufOpState<B>,
}

/// A write in progress.
pub struct WriteFuture<B> {
    inner: OpFuture,
    /// Set once a result has been handed out, so a second poll panics rather
    /// than falling through to a path that cannot produce a buffer.
    done: bool,
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
    type Output = BufResult<Transferred, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        match &mut this.state {
            BufOpState::Failed(taken) => {
                let (error, buffer) = taken.take().expect("polled after completion");
                this.state = BufOpState::Done;
                Poll::Ready(BufResult::new(Err(error), buffer))
            }
            BufOpState::Done => panic!("read future polled after completion"),
            BufOpState::Waiting(op) => match op.poll_resolution(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(resolution) => {
                    let mut result = into_outcome::<B>(resolution);
                    if let Ok(transferred) = &result.result {
                        // SAFETY: the kernel reported writing this many bytes
                        // into the buffer, so they are initialized, and the
                        // count is bounded by the capacity checked before
                        // submission.
                        unsafe { result.buffer.set_buf_init(*transferred as usize) };
                    }
                    this.state = BufOpState::Done;
                    Poll::Ready(result)
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
                    waker: None,
                })),
                driver: Weak::new(),
                finished: true,
            },
            done: false,
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
    type Output = BufResult<Transferred, B>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        if this.done {
            panic!("write future polled after completion");
        }
        // A rejected write has its result waiting in the slot already, so this
        // takes the same path as a real completion.
        match this.inner.poll_resolution(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(resolution) => {
                this.done = true;
                Poll::Ready(into_outcome::<B>(resolution))
            }
        }
    }
}

/// A flush in progress.
///
/// Flush carries no caller buffer, so it resolves to a plain result rather than
/// a [`BufResult`].
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
                    let Resolution(result, _) = resolution;
                    Poll::Ready(result.map(|_| ()))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_buffer_result_reports_success_and_failure() {
        let failed: BufResult<u32, Vec<u8>> = BufResult::new(Err(Error::QueueFull), vec![1]);
        assert!(!failed.is_ok());
        assert!(matches!(failed.err(), Some(Error::QueueFull)));

        let ok: BufResult<u32, Vec<u8>> = BufResult::new(Ok(3), vec![1, 2, 3]);
        assert!(ok.is_ok());
        assert!(ok.err().is_none());
        let (n, buf) = ok.unwrap();
        assert_eq!(n, 3);
        assert_eq!(buf, vec![1, 2, 3]);
    }

    /// SC-021: the completion signal resolves once teardown has finished, and
    /// resolves immediately if it already has.
    #[test]
    fn shutdown_complete_resolves_after_teardown() {
        let driver = test_driver();
        let handle = driver.handle();

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut before = Box::pin(handle.shutdown_complete());
        assert!(
            before.as_mut().poll(&mut cx).is_pending(),
            "teardown has not happened yet"
        );
        assert_eq!(
            driver.inner.borrow().shutdown_waiters.len(),
            1,
            "the waiter must be registered"
        );

        // Re-polling must replace this future's waker, not add a second.
        assert!(before.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().shutdown_waiters.len(),
            1,
            "re-polling must not accumulate wakers"
        );

        drop(driver);

        assert!(
            before.as_mut().poll(&mut cx).is_ready(),
            "the signal must resolve once teardown has finished"
        );
        let mut after = Box::pin(handle.shutdown_complete());
        assert!(
            after.as_mut().poll(&mut cx).is_ready(),
            "the signal must resolve immediately when teardown is already done"
        );
    }

    /// SC-022: several waiters are all resolved, and a waiter dropped before
    /// resolution leaves nothing behind.
    #[test]
    fn shutdown_complete_handles_several_waiters_and_abandonment() {
        let driver = test_driver();
        let handle = driver.handle();

        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut a = Box::pin(handle.shutdown_complete());
        let mut b = Box::pin(handle.shutdown_complete());
        let mut abandoned = Box::pin(handle.shutdown_complete());
        assert!(a.as_mut().poll(&mut cx).is_pending());
        assert!(b.as_mut().poll(&mut cx).is_pending());
        assert!(abandoned.as_mut().poll(&mut cx).is_pending());
        assert_eq!(driver.inner.borrow().shutdown_waiters.len(), 3);

        // Abandoning a wait must deregister it, or a `select!` loop would pin
        // wakers for every branch it ever lost.
        drop(abandoned);
        assert_eq!(
            driver.inner.borrow().shutdown_waiters.len(),
            2,
            "a dropped waiter must be removed"
        );

        drop(driver);
        assert!(a.as_mut().poll(&mut cx).is_ready());
        assert!(b.as_mut().poll(&mut cx).is_ready());
    }

    /// FR-006: a buffer held by an operation returns to its collection when the
    /// operation *reports*, not when its future is dropped.
    ///
    /// The distinction matters: the kernel may still be writing into the buffer
    /// after the future is gone, so handing it out again at drop would let the
    /// application read bytes an operation is still filling.
    #[test]
    fn a_dropped_future_returns_its_handle_only_once_the_operation_reports() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut registration = Box::pin(handle.register_buffers(vec![vec![0_u8; 64]]));
        let mut collection = None;
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(registered) = registration.as_mut().poll(&mut cx) {
                collection = Some(registered.unwrap());
                break;
            }
            std::thread::yield_now();
        }
        let collection = collection.expect("the registration must complete");

        let buffer = collection.check_out(0).expect("index 0 exists");
        let fut = handle.read_registered(FileTarget::Owned(&file), buffer, 0, 8, 0);
        driver.inner.borrow_mut().submit_pending();

        // Withhold reaping so the operation cannot report, then abandon it.
        driver.inner.borrow_mut().withhold_reaps = 4;
        drop(fut);
        assert!(
            matches!(
                collection.check_out(0),
                Err(Error::BufferCheckedOut { index: 0 })
            ),
            "the buffer must stay out while the kernel may still be writing it"
        );

        // Once the driver observes the completion, the handle is released.
        driver.inner.borrow_mut().withhold_reaps = 0;
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if collection.check_out(0).is_ok() {
                break;
            }
            std::thread::yield_now();
        }
        collection
            .check_out(0)
            .expect("the buffer must return once the operation reports");

        handle.shutdown_now();
        drop(driver);
    }

    /// Every future in the crate panics on re-poll rather than hanging; the
    /// registered one carries a handle now, so its state machine changed shape.
    #[test]
    #[should_panic(expected = "polled after completion")]
    fn re_polling_a_completed_registered_operation_panics() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);

        // A rejection resolves immediately, which is enough to reach `Done`.
        let mut registration = Box::pin(handle.register_buffers(vec![vec![0_u8; 8]]));
        let mut collection = None;
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(registered) = registration.as_mut().poll(&mut cx) {
                collection = Some(registered.unwrap());
                break;
            }
            std::thread::yield_now();
        }
        let collection = collection.expect("the registration must complete");
        let buffer = collection.check_out(0).expect("index 0 exists");

        // Asking for more than the extent is refused before submission.
        let mut fut = Box::pin(handle.read_registered(FileTarget::Owned(&file), buffer, 0, 99, 0));
        assert!(fut.as_mut().poll(&mut cx).is_ready());
        let _ = fut.as_mut().poll(&mut cx);
    }

    /// Drives a registration to completion and returns its collection.
    fn register(driver: &Driver, handle: &Handle, sizes: &[usize]) -> RegisteredBuffers {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let buffers: Vec<Vec<u8>> = sizes.iter().map(|&n| vec![0_u8; n]).collect();
        let mut fut = Box::pin(handle.register_buffers(buffers));
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(registered) = fut.as_mut().poll(&mut cx) {
                return registered.unwrap();
            }
            std::thread::yield_now();
        }
        panic!("the registration never completed");
    }

    /// Route 1 (FR-011): a handle outstanding when a new registration is
    /// requested would be left naming a set the platform no longer resolves
    /// against, so the request is refused rather than the handle staled.
    #[test]
    fn re_registering_is_refused_while_a_handle_is_checked_out() {
        let driver = test_driver();
        let handle = driver.handle();
        let collection = register(&driver, &handle, &[64]);

        let held = collection.check_out(0).expect("index 0 exists");

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = Box::pin(handle.register_buffers(vec![vec![0_u8; 32]]));
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Registered::Failed(Error::BufferCheckedOut { index }, returned)) => {
                assert_eq!(index, 0);
                assert_eq!(returned.len(), 1, "the caller's buffers must come back");
            }
            other => panic!("expected BufferCheckedOut, got {:?}", other.is_ready()),
        }

        // Returning the handle lifts the refusal.
        drop(held);
        drop(fut);
        let second = register(&driver, &handle, &[32]);
        assert_eq!(second.len(), 1);

        handle.shutdown_now();
        drop(driver);
    }

    /// Route 5 (FR-011a): two registrations in flight at once would let the
    /// first adopt, a handle be taken from it, and the second retire that
    /// registration underneath the handle. A per-registration flag cannot see
    /// this, because the registration the handle came from did not exist when
    /// the second request was made.
    #[test]
    fn a_second_registration_is_refused_while_one_is_in_flight() {
        let driver = test_driver();
        let handle = driver.handle();

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut first = Box::pin(handle.register_buffers(vec![vec![0_u8; 64]]));
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(
            driver.inner.borrow().registration_in_flight,
            "the first request must be recorded as in flight"
        );

        let mut second = Box::pin(handle.register_buffers(vec![vec![0_u8; 32]]));
        match second.as_mut().poll(&mut cx) {
            Poll::Ready(Registered::Failed(Error::RegistrationPending, returned)) => {
                assert_eq!(returned.len(), 1, "the caller's buffers must come back");
            }
            other => panic!("expected RegistrationPending, got {:?}", other.is_ready()),
        }

        handle.shutdown_now();
        drop(driver);
    }

    /// Route 2 (FR-011a): a registration is adopted when it completes, not when
    /// it is requested, so a handle taken in that window would name a set about
    /// to be superseded.
    #[test]
    fn checkout_is_refused_while_a_registration_is_in_flight() {
        let driver = test_driver();
        let handle = driver.handle();
        let collection = register(&driver, &handle, &[64]);

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = Box::pin(handle.register_buffers(vec![vec![0_u8; 32]]));
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        assert!(
            matches!(collection.check_out(0), Err(Error::RegistrationPending)),
            "no handle may be taken while a registration is in flight"
        );

        handle.shutdown_now();
        drop(driver);
    }

    /// Route 3 (FR-011b): a collection retained across a successful
    /// re-registration must stop yielding, or it would hand out handles naming
    /// buffers the current registration does not contain.
    #[test]
    fn a_superseded_collection_stops_yielding_handles() {
        let driver = test_driver();
        let handle = driver.handle();
        let first = register(&driver, &handle, &[64]);
        let second = register(&driver, &handle, &[32]);

        assert!(
            matches!(first.check_out(0), Err(Error::RegistrationSuperseded)),
            "the superseded collection must refuse"
        );
        second
            .check_out(0)
            .expect("the current collection still yields");

        handle.shutdown_now();
        drop(driver);
    }

    /// Route 4: once the driver is gone, no operation can be issued, so a handle
    /// taken from a collection would be useless — but the bytes any *existing*
    /// handle holds must stay readable (FR-009).
    #[test]
    fn checkout_is_refused_once_the_driver_is_gone_but_held_bytes_survive() {
        let driver = test_driver();
        let handle = driver.handle();
        let collection = register(&driver, &handle, &[16]);

        let mut held = collection.check_out(0).expect("index 0 exists");
        held.fill(b"survives").expect("room enough");

        handle.shutdown_now();
        drop(driver);

        assert!(
            matches!(collection.check_out(1), Err(Error::ShuttingDown)),
            "a dead driver can issue no operation, so it yields no handle"
        );
        assert_eq!(
            &held[..8],
            b"survives",
            "an outstanding handle must keep its bytes readable"
        );
    }

    /// FR-009 from the other side: a retired registration's memory is not
    /// released while the platform may still hold pointers into it, which on
    /// this platform means until the ring closes.
    #[test]
    fn a_retired_registration_survives_until_nothing_holds_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        struct CountingBuf(Vec<u8>);
        impl Drop for CountingBuf {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }
        // SAFETY: the pointer and lengths come from the inner `Vec`, which
        // nothing here grows, and the buffer has no interior mutability.
        unsafe impl crate::buf::IoBuf for CountingBuf {
            fn buf_ptr(&self) -> *const u8 {
                self.0.as_ptr()
            }
            fn buf_len(&self) -> usize {
                self.0.len()
            }
        }
        // SAFETY: as above; `set_buf_init` only reports bytes the kernel wrote.
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

        DROPS.store(0, Ordering::SeqCst);

        let driver = test_driver();
        let handle = driver.handle();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);

        let mut fut = Box::pin(handle.register_buffers(vec![CountingBuf(vec![0_u8; 32])]));
        let first = loop {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(registered) = fut.as_mut().poll(&mut cx) {
                break registered.unwrap();
            }
        };

        // Supersede it, then let the driver go.
        let _second = register(&driver, &handle, &[16]);
        assert_eq!(DROPS.load(Ordering::SeqCst), 0);

        handle.shutdown_now();
        drop(driver);
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            0,
            "a collection still held must keep its buffers alive"
        );

        drop(first);
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            1,
            "releasing the last reference is what frees it"
        );
    }

    /// A **file** registration completing must not clear the flag a **buffer**
    /// registration set.
    ///
    /// The two share a payload field, the slab, and the completion fork, so a
    /// variant-blind clear would let a file registration finishing first reopen
    /// the window Route 2 exists to close — and a handle taken in it would name
    /// a set the pending buffer registration is about to retire.
    ///
    /// Asserted against the flag directly rather than by racing two
    /// registrations, because which completes first is the kernel's choice and a
    /// test that only sometimes observes the window would prove nothing.
    #[test]
    fn a_file_registration_does_not_clear_the_buffer_registration_flag() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);

        // Stand in for a buffer registration in flight.
        driver.inner.borrow_mut().registration_in_flight = true;

        let mut files = Box::pin(handle.register_files(std::slice::from_ref(&file)));
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if files.as_mut().poll(&mut cx).is_ready() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            driver.inner.borrow().file_registration.is_some(),
            "the file registration must have completed for this test to mean anything"
        );

        assert!(
            driver.inner.borrow().registration_in_flight,
            "a file registration must leave a pending buffer registration's flag alone"
        );

        // Restore, so teardown is not left believing a registration is pending.
        driver.inner.borrow_mut().registration_in_flight = false;
        handle.shutdown_now();
        drop(driver);
    }

    fn test_driver() -> Driver {
        let ring = IoRing::builder().build().unwrap();
        Driver::new(ring).unwrap()
    }

    /// Drives the driver until `fut` resolves, failing rather than hanging.
    ///
    /// These tests have no executor, so a future that needs the ring to make
    /// progress needs someone to submit and reap on its behalf. The bounded loop
    /// is deliberate: the failure mode these tests exist to catch is an
    /// operation that never completes, and an unbounded loop would present it as
    /// a hang rather than a failure.
    fn pump<F: Future>(driver: &Driver, fut: F) -> F::Output {
        let mut fut = Box::pin(fut);
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        for _ in 0..10_000 {
            if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
                return out;
            }
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            std::thread::yield_now();
        }
        panic!("the operation did not complete within the pump budget");
    }

    fn readme() -> File {
        File::open(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../testdata/sample.txt"
        ))
        .unwrap()
    }

    /// A connected named-pipe pair opened for overlapped I/O.
    ///
    /// The only source of a genuinely long-running operation these tests have:
    /// a read on the server end stays in the kernel until the client writes, so
    /// "still in flight at shutdown" becomes a fact rather than a race. Every
    /// other handle available here — a small local file — completes so quickly
    /// that a test asserting an operation is still outstanding would be
    /// asserting a timing accident.
    struct Pipe {
        server: File,
        client: std::fs::File,
    }

    impl Pipe {
        fn new() -> Self {
            use std::os::windows::fs::OpenOptionsExt;
            use std::sync::atomic::{AtomicUsize, Ordering};
            use windows::Win32::Storage::FileSystem::{FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX};
            use windows::Win32::System::Pipes::{
                CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
            };

            // Unique per pipe, so tests running concurrently in the same process
            // never collide on a name.
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let name = format!(
                r"\\.\pipe\win-ioring-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            );
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();

            // SAFETY: the name is a NUL-terminated wide string that outlives the
            // call, and no security attributes are supplied.
            let raw = unsafe {
                CreateNamedPipeW(
                    windows::core::PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            assert!(
                !raw.is_invalid(),
                "creating the test pipe failed: {}",
                std::io::Error::last_os_error()
            );

            // The client connects to the waiting instance; no `ConnectNamedPipe`
            // is needed on the server side for the connection to be established.
            let client = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED.0)
                .open(&name)
                .expect("connecting to the test pipe");

            // SAFETY: `raw` is a freshly created handle that nothing else owns.
            let server = unsafe { File::from_raw_handle(raw) };
            Self { server, client }
        }

        /// Unblocks a pending read on the server end.
        fn write_from_client(&mut self, bytes: &[u8]) {
            use std::io::Write;
            self.client.write_all(bytes).expect("writing to the pipe");
            self.client.flush().ok();
        }
    }

    /// SC-013a: the sequential guard refuses a pipe and permits a file.
    ///
    /// The pair is the criterion, not either half. The refusal alone would pass
    /// in an implementation that refused everything; the permission alone would
    /// pass in one that refused nothing. Both halves run against the same method
    /// so that neither can be satisfied by a different code path.
    ///
    /// Read and write are covered separately because they are separate call
    /// sites: a guard added to one and forgotten on the other is precisely the
    /// mistake this shape catches, and it is a plausible one.
    #[test]
    fn the_sequential_api_refuses_a_pipe_and_permits_a_file() {
        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();
        let mut server = pipe.server.clone();

        // Negative half: a pipe.
        //
        // Polled once rather than awaited. The refusal is constructed, not
        // awaited — it resolves on the first poll without the ring — so a single
        // poll is sufficient *and* necessary: awaiting a future the guard failed
        // to refuse would wait forever on a driver nothing is pumping, turning a
        // failed assertion into a hang. Three mutation rows found exactly that.
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);

        let mut read = Box::pin(server.read(&handle, vec![0_u8; 16], 16));
        let Poll::Ready(outcome) = read.as_mut().poll(&mut cx) else {
            panic!(
                "a sequential read on a pipe must be refused at construction; \
                 it was accepted and left outstanding"
            );
        };
        let (result, buffer) = outcome.into_parts();
        assert!(
            matches!(result, Err(Error::NoFileOffset { .. })),
            "a sequential read on a pipe must be refused, not permitted with a \
             meaningless offset; got {result:?}"
        );
        assert_eq!(
            buffer.len(),
            16,
            "and the refusal must return the caller's buffer, which is what \
             makes it expressible in the BufResult shape"
        );
        drop(read);

        let mut write = Box::pin(server.write(&handle, vec![0_u8; 16], 16));
        let Poll::Ready(outcome) = write.as_mut().poll(&mut cx) else {
            panic!(
                "and a sequential write must be refused at construction too; \
                 the two are separate call sites and a guard on one is not a \
                 guard on the other"
            );
        };
        let (result, buffer) = outcome.into_parts();
        assert!(
            matches!(result, Err(Error::NoFileOffset { .. })),
            "and so must a sequential write; got {result:?}"
        );
        assert_eq!(buffer.len(), 16);
        drop(write);

        // Positive half: an ordinary file, through the same methods. Without
        // this the test above passes against a guard that refuses everything.
        let path = std::env::temp_dir().join(format!(
            "win-ioring-guard-{}-{:?}.bin",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&path, b"0123456789abcdef").expect("writing the fixture");
        let mut file = File::open(&path).expect("opening the fixture");

        let read = file.read(&handle, vec![0_u8; 16], 16);
        let (result, _) = pump(&driver, read).into_parts();
        let n = result.expect("a sequential read on a real file must be permitted");
        assert_eq!(n, 16);
        drop(file);

        // A separate writable handle: `File::open` is read-only, so reusing it
        // here would fail with an access error that says nothing about the guard.
        let mut file = File::create(&path).expect("opening the fixture for writing");
        let write = file.write(&handle, vec![0x5A_u8; 4], 4);
        let (result, _) = pump(&driver, write).into_parts();
        result.expect("and so must a sequential write");

        drop(file);
        let _ = std::fs::remove_file(&path);
    }

    /// SC-013b: the guard fails open on a type it does not recognise.
    ///
    /// `FILE_TYPE_UNKNOWN` cannot be produced on demand from a real handle —
    /// which is why this injects it, on the same reasoning that makes
    /// `fail_next_arm` necessary in `sys/event.rs`.
    ///
    /// The direction matters. A guard that refused the unknown case would newly
    /// break handle kinds nobody anticipated, for the sake of a contract nobody
    /// has established is violated there. Failing open leaves them exactly where
    /// they are today.
    #[test]
    fn the_guard_permits_an_unrecognised_handle_type() {
        use windows::Win32::Storage::FileSystem::FILE_TYPE_UNKNOWN;

        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();
        let mut server = pipe.server.clone();

        crate::file::force_next_file_type(FILE_TYPE_UNKNOWN.0);
        let mut read = Box::pin(server.read(&handle, vec![0_u8; 16], 16));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        // The refusal, when it happens, happens at construction and resolves on
        // the first poll without the ring. A permitted read on an empty pipe
        // stays pending, so "not immediately refused" is the whole observation.
        match read.as_mut().poll(&mut cx) {
            Poll::Ready(r) => {
                let (result, _) = r.into_parts();
                panic!(
                    "an unrecognised handle type must be permitted, not \
                     refused; the guard is deliberately fail-open. Got \
                     {result:?}"
                );
            }
            Poll::Pending => {}
        }
    }

    /// SC-013e: `FILE_TYPE_CHAR` is refused, not permitted.
    ///
    /// Its own criterion because the character-device leg is the one obligation
    /// FR-019 gained without a natural gate: no pipe work requires it, nothing
    /// in this crate opens a character device, and an ungated obligation is the
    /// one that gets quietly dropped in a later refactor.
    ///
    /// Injected rather than opened. `NUL` is reachable through `File::open`, but
    /// it returns end-of-file to every read, so a test against it could not tell
    /// a working guard from a broken one — it would report success either way.
    #[test]
    fn the_guard_refuses_a_character_device() {
        use windows::Win32::Storage::FileSystem::FILE_TYPE_CHAR;

        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();
        let mut server = pipe.server.clone();

        crate::file::force_next_file_type(FILE_TYPE_CHAR.0);
        let mut read = Box::pin(server.read(&handle, vec![0_u8; 16], 16));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let Poll::Ready(outcome) = read.as_mut().poll(&mut cx) else {
            panic!(
                "a character device must be refused at construction; it was \
                 accepted and left outstanding, which is the shape a narrowed \
                 guard produces"
            );
        };
        let (result, buffer) = outcome.into_parts();
        assert!(
            matches!(
                result,
                Err(Error::NoFileOffset {
                    file_type
                }) if file_type == FILE_TYPE_CHAR.0
            ),
            "a character device must be refused, and the error must carry the \
             type that caused it; got {result:?}"
        );
        assert_eq!(buffer.len(), 16, "with the buffer returned");
    }

    /// SC-013c: the positional path is *not* guarded.
    ///
    /// The asymmetry is deliberate and this states it as a decision rather than
    /// leaving it to be inferred from the guard's absence. `Handle::read` and
    /// `Handle::write` at an explicit offset are what the pipe types themselves
    /// use to move bytes; guarding them would break pipe I/O entirely. The
    /// platform ignores the offset here too — the difference is that the caller
    /// supplied it knowingly rather than having the crate invent one.
    #[test]
    fn the_positional_api_is_not_guarded_on_a_pipe() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        pipe.write_from_client(b"hello");

        let fut = handle.read(&pipe.server, vec![0_u8; 5], 5, 4096);
        let (result, buffer) = pump(&driver, fut).into_parts();
        let n = result.expect("the positional path must still work on a pipe");
        assert_eq!(n, 5);
        assert_eq!(
            &buffer[..5],
            b"hello",
            "and it reads from the head of the stream regardless of the offset \
             the caller passed, which is the behaviour the sequential guard \
             exists to stop the crate from relying on"
        );
    }

    /// SC-013: the behaviours the guard exists to prevent, asserted directly.
    ///
    /// The guard stops callers reaching these, but the documentation still
    /// asserts things about them, and documentation drifts from a platform that
    /// nobody re-measures. These assertions are what keep FR-019a's rustdoc
    /// honest: if Windows ever started honouring the offset on a pipe, this
    /// fails and the paragraph explaining why the sequential API is refused
    /// becomes visibly wrong rather than quietly so.
    ///
    /// Reached through the positional API, which is unguarded by design — so
    /// this is testing the platform's behaviour, not a hole in the guard.
    ///
    /// **(a)** Two reads at different non-zero offsets return *consecutive*
    /// head-of-stream bytes. The first read alone already refutes "the offset is
    /// honoured"; what the pair establishes is the stronger and more useful
    /// claim, that the stream advances by the bytes transferred rather than
    /// jumping to `offset + len`. That is the version a caller would actually
    /// hit, and it is the one the cursor would get wrong.
    #[test]
    fn a_pipe_ignores_the_offset_and_consumes_from_the_head() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        pipe.write_from_client(b"ABCDEFGH");

        // Deliberately far apart, and neither is 0. An implementation that
        // honoured either would produce different bytes than an implementation
        // that ignores both.
        let fut = handle.read(&pipe.server, vec![0_u8; 4], 4, 4096);
        let (result, first) = pump(&driver, fut).into_parts();
        assert_eq!(result.expect("the first read"), 4);
        assert_eq!(
            &first[..4],
            b"ABCD",
            "the first read must come from the head of the stream, not from \
             offset 4096"
        );

        let fut = handle.read(&pipe.server, vec![0_u8; 4], 4, 64);
        let (result, second) = pump(&driver, fut).into_parts();
        assert_eq!(result.expect("the second read"), 4);
        assert_eq!(
            &second[..4],
            b"EFGH",
            "and the second must continue from where the first stopped -- not \
             from offset 64, and not from the head again. This is the claim the \
             cursor cannot express: the stream advanced by 4, the cursor would \
             say 4100."
        );
    }

    /// SC-013(b): a write at a non-zero offset appends.
    ///
    /// The write half of the same finding, and it needs saying separately
    /// because the failure looks different: a read from the wrong place returns
    /// wrong bytes, while a write to the wrong place corrupts a stream the
    /// caller believes it is positioning within.
    #[test]
    fn a_pipe_write_at_a_non_zero_offset_appends() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        let fut = handle.write(&pipe.server, b"first".to_vec(), 5, 0);
        let (result, _) = pump(&driver, fut).into_parts();
        assert_eq!(result.expect("the first write"), 5);

        // If the offset meant anything, this would leave a gap.
        let fut = handle.write(&pipe.server, b"second".to_vec(), 6, 100_000);
        let (result, _) = pump(&driver, fut).into_parts();
        assert_eq!(result.expect("the second write"), 6);

        let mut got = vec![0_u8; 11];
        {
            use std::io::Read;
            pipe.client
                .read_exact(&mut got)
                .expect("reading what the server wrote");
        }
        assert_eq!(
            &got, b"firstsecond",
            "the second write must append rather than land at offset 100000; a \
             pipe that honoured the offset could not produce this"
        );
    }

    /// SC-013(c): a flush on an undrained pipe stays outstanding, and retires.
    ///
    /// Three claims, and the third is the one that keeps the other two from
    /// being a hang dressed up as a measurement: the flush does not complete
    /// while the peer has not read, it completes once the peer does, and its
    /// operation retires when the future is dropped.
    ///
    /// This is the mechanism behind the shutdown finding recorded in
    /// `docs/pending-work.md`: the crate's drain is unbounded because it never
    /// abandons memory, and a flush that waits on a peer is a flush the crate
    /// cannot bound. Measured separately at over 60 s against a 135 µs control.
    #[test]
    fn a_flush_on_an_undrained_pipe_waits_for_the_peer() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        let fut = handle.write(&pipe.server, b"unread".to_vec(), 6, 0);
        let (result, _) = pump(&driver, fut).into_parts();
        assert_eq!(result.expect("the write"), 6);

        let mut flush = Box::pin(handle.flush(&pipe.server));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(flush.as_mut().poll(&mut cx).is_pending());

        // A bounded number of passes, because the claim is "does not complete",
        // and an unbounded loop would assert it by never asking.
        for _ in 0..200 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            std::thread::yield_now();
        }
        assert!(
            flush.as_mut().poll(&mut cx).is_pending(),
            "a flush must not complete while the peer has not read: this is why \
             the crate cannot promise a bounded shutdown with a pipe flush \
             outstanding"
        );

        // The peer drains. The same flush must now complete, which is what makes
        // the assertion above a measurement rather than a flush that never
        // completes under any circumstances.
        {
            use std::io::Read;
            let mut got = vec![0_u8; 6];
            pipe.client.read_exact(&mut got).expect("the peer reads");
            assert_eq!(&got, b"unread");
        }
        let resolved = pump(&driver, flush);
        resolved.expect("the flush must complete once the peer has drained");

        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            0,
            "and the operation retires rather than being left with the kernel"
        );
    }

    /// A pipe accept occupies no slab entry, while a ring operation does.
    ///
    /// SC-008. The ring operation is not decoration: without one this asserts
    /// zero against zero, which any test doing no I/O would satisfy — the
    /// "structurally guaranteed assertion" shape `docs/testing.md` names. With
    /// one outstanding the count is 1 whether or not the accept consumed an
    /// entry, so the assertion can actually fail.
    ///
    /// This lives here rather than beside the pipe server because the slab is
    /// the driver's own state and reading it is what the criterion asks for.
    #[test]
    fn a_pipe_accept_occupies_no_slab_entry() {
        use crate::pipe::Server;

        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();

        // One real ring operation, parked in the kernel on an empty pipe.
        let mut read = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(read.as_mut().poll(&mut cx).is_pending());
        {
            let mut inner = driver.inner.borrow_mut();
            inner.submit_pending();
        }
        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            1,
            "the ring operation must actually be in the kernel, or the count \
             below is being compared against nothing"
        );

        // And one pipe accept, which goes nowhere near the ring.
        let name = format!(
            r"\\.\pipe\win-ioring-slab-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let mut server = Server::create(&name).expect("a pipe instance");
        let mut accept = Box::pin(server.accept());
        assert!(accept.as_mut().poll(&mut cx).is_pending());

        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            1,
            "still one. The accept is an overlapped Win32 call with its own \
             OVERLAPPED and its own event -- it has no completion to reap and no \
             slot to occupy. Two would mean it had taken one"
        );

        drop(accept);
        drop(server);
        drop(read);
    }

    /// Establishes the premise every long-running shutdown test rests on: a read
    /// on an empty pipe really does stay in the kernel, and really does complete
    /// once data arrives.
    ///
    /// Without this, a test that "proves" an operation was still in flight would
    /// only be proving that a read had not been reaped yet.
    #[test]
    fn a_read_on_an_empty_pipe_stays_in_flight() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        let mut fut = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        for _ in 0..50 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            std::thread::yield_now();
        }
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "a read on an empty pipe must not complete on its own"
        );
        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            1,
            "the kernel must still be holding the read"
        );

        pipe.write_from_client(b"hello");

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

        let (result, buffer) = resolved
            .expect("the read must complete once data arrives")
            .into_parts();
        assert_eq!(result.expect("the read must succeed") as usize, 5);
        assert_eq!(&buffer[..5], b"hello");

        handle.shutdown();
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
        let (_, buffer) = outcome.into_parts();
        assert_eq!(
            buffer,
            vec![marker; 16],
            "the rejected read must return the caller's buffer untouched"
        );

        drop(held);
        handle.shutdown();
        drain(&driver);
    }

    /// An operation naming a registered file handle must be cancellable.
    ///
    /// A cancellation has to name the same file its target named, and a
    /// registered-handle operation named its file by index rather than by
    /// handle. Reaching for the handle alone silently skipped these, so
    /// dropping such a future left it uncancelled — contradicting the drop
    /// policy the whole crate is documented around.
    #[test]
    fn registered_handle_operations_can_be_cancelled() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // The driver only advances when this test pumps it, so registrations
        // have to be driven by hand rather than blocked on.
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut register_files = Box::pin(handle.register_files(std::slice::from_ref(&file)));
        let mut register_buffers = Box::pin(handle.register_buffers(vec![vec![0_u8; 64]]));
        let mut files_done = false;
        let mut collection = None;
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if !files_done {
                files_done =
                    matches!(register_files.as_mut().poll(&mut cx), Poll::Ready(r) if r.is_ok());
            }
            if collection.is_none()
                && let Poll::Ready(registered) = register_buffers.as_mut().poll(&mut cx)
            {
                collection = Some(registered.unwrap());
            }
            if files_done && collection.is_some() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(driver.inner.borrow().file_registration.is_some());
        assert!(driver.inner.borrow().buffer_registration.is_some());
        let collection = collection.expect("the buffer registration must complete");

        let buffer = collection.check_out(0).expect("index 0 exists");
        let fut = handle.read_registered(FileTarget::Registered { index: 0 }, buffer, 0, 8, 0);
        let id = fut
            .operation_id()
            .expect("the registered read must have been built");

        // Reach the submitted state, which is the only one a cancellation can
        // act on, and clear the submit flag so the next one is attributable.
        driver.inner.borrow_mut().submit_pending();
        driver.inner.borrow_mut().pending_submit = false;

        handle.cancel(id);
        assert!(
            driver.inner.borrow().pending_submit,
            "a registered-handle operation must accept a cancellation"
        );
        assert!(
            driver.inner.borrow().cancel_holds.is_empty(),
            "a registered handle needs no hold: the driver owns it already"
        );

        // Everything must settle, which is what proves the cancellation is a
        // real entry the platform completes rather than one that strands the
        // operation's slot in a tombstone forever.
        drop(fut);
        drain(&driver);
        handle.shutdown();
    }

    /// Operations built before the driver next runs share one submission.
    ///
    /// This is the crate's whole batching story — there is no explicit batch
    /// API — so it is worth pinning. A regression that submitted per operation
    /// would still pass every other test, just more slowly and with more
    /// syscalls, which is exactly the kind of change nothing else would catch.
    #[test]
    fn operations_built_together_share_one_submission() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // Three reads issued without awaiting any of them, so the driver has
        // had no opportunity to submit in between.
        let a = handle.read(&file, vec![0_u8; 64], 8, 0);
        let b = handle.read(&file, vec![0_u8; 64], 8, 8);
        let c = handle.read(&file, vec![0_u8; 64], 8, 16);
        assert_eq!(
            driver.inner.borrow().slab.outstanding(),
            3,
            "all three must be built before anything is submitted"
        );
        assert!(driver.inner.borrow().pending_submit);

        // A single submission covers all three: nothing is left owed.
        driver.inner.borrow_mut().submit_pending();
        assert!(
            !driver.inner.borrow().pending_submit,
            "one SubmitIoRing must cover every entry built since the last one"
        );

        drop((a, b, c));
        drain(&driver);
        handle.shutdown();
    }

    /// One submission covers every entry built since the last one — *counted*.
    ///
    /// The sibling test above can only assert that nothing is left owed, which a
    /// driver submitting once per entry would also satisfy: it would clear
    /// `pending_submit` too, just after three syscalls instead of one. Only the
    /// entries-per-submission ratio separates those two behaviours, and this is
    /// the test that would fail if the driver stopped batching.
    #[test]
    fn one_submission_covers_every_entry_built_since_the_last() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let before = handle.submission_counts();

        // Four reads issued without awaiting any of them, so the driver has had
        // no opportunity to submit in between.
        let futures: Vec<_> = (0..4)
            .map(|i| handle.read(&file, vec![0_u8; 64], 8, i * 8))
            .collect();
        assert_eq!(
            driver.inner.borrow().slab.outstanding(),
            4,
            "all four must be built before anything is submitted"
        );

        driver.inner.borrow_mut().submit_pending();

        let after = handle.submission_counts();
        assert_eq!(
            after.submissions - before.submissions,
            1,
            "four operations built together must cost exactly one submission"
        );
        assert_eq!(
            after.entries - before.entries,
            4,
            "that one submission must have covered all four entries"
        );

        drop(futures);
        drain(&driver);
        handle.shutdown();
    }

    /// The counters start at zero and never decrease.
    #[test]
    fn submission_counts_start_at_zero_and_only_grow() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let initial = handle.submission_counts();
        assert_eq!(
            initial,
            SubmissionCounts {
                submissions: 0,
                entries: 0
            },
            "a driver that has submitted nothing must report nothing"
        );

        let mut previous = initial;
        let mut futures = Vec::new();
        for round in 0..3 {
            futures.push(handle.read(&file, vec![0_u8; 64], 8, round * 8));
            driver.inner.borrow_mut().submit_pending();
            let now = handle.submission_counts();
            assert!(
                now.submissions >= previous.submissions && now.entries >= previous.entries,
                "counts must never decrease: {previous:?} then {now:?}"
            );
            previous = now;
        }
        assert!(
            previous.submissions > 0 && previous.entries > 0,
            "three submitted rounds must have moved both counters"
        );

        drop(futures);
        drain(&driver);
        handle.shutdown();
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

    /// SC-018: shutdown is idempotent, and requesting it from several handles
    /// is indistinguishable from requesting it once.
    #[test]
    fn shutdown_requests_are_idempotent_across_handles() {
        let driver = test_driver();
        let a = driver.handle();
        let b = driver.handle();

        assert!(!a.is_shutting_down());
        a.shutdown();
        assert!(a.is_shutting_down());
        assert!(b.is_shutting_down(), "handles share one driver state");

        b.shutdown();
        a.shutdown();
        assert_eq!(
            driver.inner.borrow().shutdown,
            ShutdownMode::Graceful,
            "repeating a graceful request must not change the mode"
        );
    }

    /// SC-019 (first clause): a graceful shutdown escalates to immediate, and
    /// SC-011 in part: the escalation is not suppressed by a shutdown already
    /// being in progress.
    #[test]
    fn shutdown_escalates_but_never_downgrades() {
        let driver = test_driver();
        let handle = driver.handle();

        handle.shutdown();
        assert_eq!(driver.inner.borrow().shutdown, ShutdownMode::Graceful);

        handle.shutdown_now();
        assert_eq!(
            driver.inner.borrow().shutdown,
            ShutdownMode::Immediate,
            "graceful must escalate to immediate"
        );

        // The reverse must not happen: an immediate shutdown has already asked
        // the platform to abandon work, and pretending otherwise would leave
        // callers expecting results that are not coming.
        handle.shutdown();
        assert_eq!(
            driver.inner.borrow().shutdown,
            ShutdownMode::Immediate,
            "immediate must not downgrade to graceful"
        );
    }

    /// SC-016: once shutdown is requested, submissions fail immediately rather
    /// than being accepted and abandoned later. Checked for both modes, since
    /// they are separate gates.
    #[test]
    fn submissions_are_refused_once_shutdown_is_requested() {
        for escalate in [false, true] {
            let driver = test_driver();
            let handle = driver.handle();
            let file = readme();

            if escalate {
                handle.shutdown_now();
            } else {
                handle.shutdown();
            }

            let mut fut = Box::pin(handle.read(&file, vec![0_u8; 32], 32, 0));
            let waker = futures::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(out) => assert!(
                    out.err().is_some(),
                    "a post-shutdown submission must fail, not succeed"
                ),
                Poll::Pending => panic!("a post-shutdown submission must not pend"),
            }
        }
    }

    /// The defect this work exists to fix: teardown used to set its
    /// shutting-down flag first, and cancellation refused whenever that flag was
    /// set, so teardown could never cancel anything. Cancellation must now be
    /// refused only once teardown has actually finished.
    #[test]
    fn cancellation_is_not_suppressed_by_a_shutdown_in_progress() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
        let token = fut.operation_id().expect("read was built").0;
        driver.inner.borrow_mut().submit_pending();
        assert_eq!(
            driver.inner.borrow().slab.state(token).map(|s| s.0),
            Some(Lifecycle::Submitted),
            "the operation must be submitted for this test to mean anything"
        );

        // Shutdown is under way but teardown has not completed.
        handle.shutdown_now();
        assert!(!driver.inner.borrow().torn_down);

        drop(fut);
        assert!(
            !driver.inner.borrow().cancel_holds.is_empty(),
            "a shutdown in progress must not suppress cancellation"
        );

        drain(&driver);
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

    /// SC-001 and SC-004, the headline property of this work: teardown releases
    /// every caller buffer, and never before the operation holding it has
    /// reported.
    ///
    /// The withhold seam is what makes the second half observable — it keeps an
    /// operation outstanding for a known number of steps, so "not yet released"
    /// can be checked at a moment when release would have been wrong.
    #[test]
    fn teardown_releases_caller_buffers_but_never_early() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static DROPS: AtomicUsize = AtomicUsize::new(0);

        /// A caller buffer that records its own destruction.
        struct CountingBuf(Vec<u8>);

        impl Drop for CountingBuf {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        // SAFETY: pointer and lengths come from the inner `Vec`, which nothing
        // here grows, and the buffer has no interior mutability.
        unsafe impl crate::buf::IoBuf for CountingBuf {
            fn buf_ptr(&self) -> *const u8 {
                self.0.as_ptr()
            }
            fn buf_len(&self) -> usize {
                self.0.len()
            }
        }

        // SAFETY: as above; `set_buf_init` only reports bytes the kernel wrote.
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

        DROPS.store(0, Ordering::SeqCst);

        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // Detach the operation, so nothing but the driver owns the buffer and
        // only teardown can release it.
        let fut = handle.read(&file, CountingBuf(vec![0_u8; 128]), 64, 0);
        let token = fut.operation_id().expect("read was built").0;
        driver.inner.borrow_mut().submit_pending();
        assert_eq!(
            driver.inner.borrow().slab.state(token).map(|s| s.0),
            Some(Lifecycle::Submitted),
            "the operation must reach the kernel for this test to mean anything"
        );
        drop(fut);

        // Refuse to observe completions for several steps. The operation stays
        // outstanding, and its buffer must not be released while it is.
        // Deliberately more than the loop below consumes, so that reaping is
        // still withheld when teardown starts — that is what forces teardown to
        // keep draining rather than give up, and what makes a give-up-and-leak
        // teardown fail this test.
        driver.inner.borrow_mut().withhold_reaps = 6;
        for _ in 0..3 {
            let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
            drop(wakers);
            assert_eq!(
                outcome,
                StepOutcome::Progressing,
                "the operation should still be outstanding while reaping is withheld"
            );
            assert_eq!(
                DROPS.load(Ordering::SeqCst),
                0,
                "a buffer must not be released before its operation has reported"
            );
        }

        drop(driver);
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            1,
            "teardown must release the caller's buffer once the operation reports"
        );
    }

    /// The inverse of what this test used to assert. Teardown no longer
    /// abandons a waiting future's buffer: it drains until the operation
    /// reports, so the future receives a real outcome and its buffer back.
    #[test]
    fn teardown_resolves_waiting_futures_and_returns_their_buffers() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let mut fut = handle.read(&file, vec![0_u8; 128], 64, 0);

        // Register a waker so the future is genuinely waiting.
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut fut).poll(&mut cx).is_pending());

        drop(driver);

        match Pin::new(&mut fut).poll(&mut cx) {
            Poll::Ready(outcome) => {
                let buffer = outcome.into_parts().1;
                // The length reflects what was transferred; the capacity proves
                // this is the caller's original allocation coming back rather
                // than something reconstructed.
                assert_eq!(
                    buffer.capacity(),
                    128,
                    "the caller's own buffer must come back"
                );
            }
            Poll::Pending => panic!("a future was left pending after teardown"),
        }

        // Nothing may be submitted after teardown.
        let mut outcome = handle.read(&file, vec![0_u8; 64], 20, 0);
        match Pin::new(&mut outcome).poll(&mut cx) {
            Poll::Ready(o) => assert!(o.err().is_some()),
            Poll::Pending => panic!("a post-teardown submission should fail immediately"),
        }
    }

    /// SC-003, and the inverse of what this test used to assert. Registered
    /// buffers were previously abandoned whenever the ring would not settle.
    /// Teardown now drains first, so by the time it releases them the kernel can
    /// no longer reach them — and they must actually be freed, not leaked.
    #[test]
    fn teardown_releases_registered_buffers() {
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

        // Strand an operation — built but never submitted — so the drain has
        // real work to do, then tear down. Everything must come back.
        let stranded = handle.read(&readme(), vec![0_u8; 64], 20, 0);
        assert_eq!(driver.inner.borrow().slab.outstanding(), 1);
        drop(driver);
        drop(stranded);

        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            1,
            "teardown must release registered buffers once the ring is closed"
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
            Registered::Ok(_) => panic!("an empty registration must be refused by the platform"),
        }

        // Nothing was adopted, so the driver still has no registration.
        assert!(driver.inner.borrow().buffer_registration.is_none());

        handle.shutdown();
    }

    /// FR-008 in ordinary use rather than at shutdown: cancelling one operation
    /// must not disturb another on the same file.
    ///
    /// The first operation a driver issues is the one that matters. Its token
    /// occupies slot zero at the first generation, which is the only combination
    /// that could encode to a user data of zero — and the platform reads a
    /// cancellation target of zero as "everything on this handle". Dropping that
    /// first future would then abort every other read in flight on the same
    /// file, which is why this test drops the *first* of the two.
    #[test]
    fn cancelling_one_operation_leaves_its_siblings_alone() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        assert_eq!(
            driver.inner.borrow().slab.outstanding(),
            0,
            "the next operation must be the driver's first, so it takes slot zero"
        );
        let first = handle.read(&pipe.server, vec![0_u8; 16], 16, 0);
        let mut second = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        assert_ne!(
            first
                .operation_id()
                .expect("the read was built")
                .0
                .as_user_data(),
            0,
            "the first operation's user data must not be the platform's cancel-everything value"
        );

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(second.as_mut().poll(&mut cx).is_pending());
        driver.inner.borrow_mut().submit_pending();

        // Dropping the future cancels just this one operation.
        drop(first);

        let mut resolved = None;
        for _ in 0..200 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(result) = second.as_mut().poll(&mut cx) {
                resolved = Some(result);
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            resolved.is_none(),
            "the surviving read must still be in flight: {:?}",
            resolved.map(|r| r.into_parts().0)
        );

        // It is still a working operation, not merely an unreported one.
        pipe.write_from_client(b"kept");
        for _ in 0..1000 {
            let wakers = {
                let mut inner = driver.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions()
            };
            for waker in wakers {
                waker.wake();
            }
            if let Poll::Ready(result) = second.as_mut().poll(&mut cx) {
                resolved = Some(result);
                break;
            }
            std::thread::yield_now();
        }

        let (transferred, buffer) = resolved
            .expect("the surviving read must complete once data arrives")
            .into_parts();
        assert_eq!(
            transferred.expect("the surviving read must not have been cancelled") as usize,
            4
        );
        assert_eq!(&buffer[..4], b"kept");

        handle.shutdown_now();
        drop(driver);
    }

    /// SC-026: a drain that waits and times out is making normal progress, not
    /// failing.
    ///
    /// `submit` reports a wait timeout as an error, which is a trap: feeding it
    /// to the submission-failure count would make an ordinary slow shutdown look
    /// like a stuck queue, and the drain would eventually conclude the kernel
    /// held nothing and abandon queue entries that are still live. The pipe read
    /// here never completes, so every wait times out.
    #[test]
    fn a_wait_timeout_during_the_drain_is_not_a_failure() {
        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();

        let mut fut = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Graceful, so nothing cancels the read and every wait must time out.
        handle.shutdown();

        // More rounds than it takes for repeated submission failures to be
        // treated as unsubmittable residue.
        for _ in 0..=RESIDUE_ATTEMPTS {
            let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
            drop(wakers);
            assert_eq!(
                outcome,
                StepOutcome::Progressing,
                "a timed-out wait must never be mistaken for an unsubmittable queue"
            );
        }
        assert_eq!(
            driver.inner.borrow().submit_failures,
            0,
            "a wait timeout must not count as a submission failure"
        );

        handle.shutdown_now();
        drop(driver);
        assert!(fut.as_mut().poll(&mut cx).is_ready());
    }

    /// SC-005c and SC-008a: when only queue entries the kernel has repeatedly
    /// refused remain, the drain stops waiting for reports that can never come,
    /// closes the ring, and still resolves the futures with their buffers.
    ///
    /// This is the one case where abandoning a queue entry is sound, and only
    /// because the entry was never accepted and the ring is closed first. The
    /// error must be the named teardown one: an operation the driver ended
    /// itself did not fail at I/O, and telling the caller it did would be a lie.
    #[test]
    fn entries_the_kernel_never_accepted_are_abandoned_only_after_the_ring_closes() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        // More refusals than it takes to declare the queue unsubmittable.
        driver.inner.borrow_mut().fail_next_submits = RESIDUE_ATTEMPTS * 4;
        let mut fut = Box::pin(handle.read(&file, vec![0_u8; 128], 64, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().slab.built().len(),
            1,
            "the entry must be queued but unaccepted for this test to mean anything"
        );

        handle.shutdown_now();
        drop(driver);

        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                let (outcome, buffer) = result.into_parts();
                assert!(
                    matches!(outcome, Err(Error::AbandonedAtShutdown)),
                    "an operation the driver ended itself must say so: {outcome:?}"
                );
                assert_eq!(
                    buffer.capacity(),
                    128,
                    "the caller's own buffer must come back"
                );
            }
            Poll::Pending => panic!("an unsubmittable entry left its future pending"),
        }
    }

    /// The registration counterpart of the test above, and the gap that let a
    /// real bug through: a registration carries the caller's buffers in a
    /// different field from an ordinary operation, so a teardown path that
    /// returned only `buffer` dropped them and handed back an empty vector.
    #[test]
    fn a_registration_the_kernel_never_accepted_still_returns_its_buffers() {
        let driver = test_driver();
        let handle = driver.handle();

        driver.inner.borrow_mut().fail_next_submits = RESIDUE_ATTEMPTS * 4;
        let mut fut = Box::pin(handle.register_buffers(vec![vec![7_u8; 16], vec![9_u8; 32]]));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().slab.built().len(),
            1,
            "the registration must be queued but unaccepted for this test to mean anything"
        );

        handle.shutdown_now();
        drop(driver);

        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Registered::Failed(e, returned)) => {
                assert!(
                    matches!(e, Error::AbandonedAtShutdown),
                    "an abandoned registration must report the teardown error: {e:?}"
                );
                assert_eq!(
                    returned,
                    vec![vec![7_u8; 16], vec![9_u8; 32]],
                    "the caller's own buffers must come back"
                );
            }
            Poll::Ready(Registered::Ok(_)) => {
                panic!("a registration the kernel never accepted must not report success")
            }
            Poll::Pending => panic!("an unsubmittable registration left its future pending"),
        }
    }

    /// SC-002: a completed shutdown closes the file handles operations held, not
    /// merely the ring.
    ///
    /// Proved by reopening with sharing denied, which the platform refuses while
    /// any handle to the file is still open. The caller's own handle is dropped
    /// first, so the driver's clone is the only thing that could keep it alive.
    #[test]
    fn a_completed_shutdown_closes_the_files_its_operations_held() {
        let path = std::env::temp_dir().join(format!(
            "win-ioring-shutdown-{}-{:p}.tmp",
            std::process::id(),
            &0_u8 as *const u8
        ));
        std::fs::write(&path, b"some bytes to read").unwrap();

        let exclusively_openable = |path: &std::path::Path| {
            use std::os::windows::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(0)
                .open(path)
                .is_ok()
        };

        let driver = test_driver();
        let handle = driver.handle();
        let file = File::open(&path).unwrap();

        let fut = handle.read(&file, vec![0_u8; 8], 8, 0);
        // Leave the driver holding the only reference.
        drop(file);
        drop(fut);
        assert!(
            !exclusively_openable(&path),
            "the driver must still be holding the file for this test to mean anything"
        );

        handle.shutdown_now();
        drop(driver);

        assert!(
            exclusively_openable(&path),
            "a completed shutdown must have closed the file the operation held"
        );
        std::fs::remove_file(&path).ok();
    }

    /// SC-011b: a registration in flight at an immediate shutdown is drained to
    /// its natural completion.
    ///
    /// It cannot be cancelled — a registration names no file, and a cancellation
    /// must name the file its target named — so the only correct handling is to
    /// wait for it, which is what makes it worth pinning down separately.
    #[test]
    fn a_registration_in_flight_at_an_immediate_shutdown_completes_naturally() {
        let driver = test_driver();
        let handle = driver.handle();

        let mut fut = Box::pin(handle.register_buffers(vec![vec![7_u8; 16]]));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(
            fut.as_mut().poll(&mut cx).is_pending(),
            "the registration must reach the platform before the shutdown"
        );
        driver.inner.borrow_mut().submit_pending();
        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            1,
            "the kernel must hold the registration when the shutdown begins"
        );

        handle.shutdown_now();
        assert_eq!(
            driver.inner.borrow().cancel_attempts,
            0,
            "a registration names no file, so nothing can be cancelled for it"
        );

        let mut outcome = StepOutcome::Progressing;
        for _ in 0..32 {
            let (wakers, step) = driver.inner.borrow_mut().drain_step();
            for waker in wakers {
                waker.wake();
            }
            outcome = step;
            if step != StepOutcome::Progressing {
                break;
            }
        }
        assert_eq!(outcome, StepOutcome::Quiescent);

        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(Registered::Ok(_)) => {}
            Poll::Ready(Registered::Failed(e, _)) => {
                panic!("the registration must complete naturally, not fail: {e:?}")
            }
            Poll::Pending => panic!("the registration was left pending by the drain"),
        }

        drop(driver);
    }

    /// SC-023: a shutdown with nothing outstanding must not wait on the kernel.
    ///
    /// The drain's wait has a fixed timeout, so a shutdown that issued one
    /// needlessly would stall for it — the difference between an instant
    /// shutdown and a visibly sluggish one.
    #[test]
    fn a_shutdown_with_nothing_outstanding_does_not_wait() {
        let driver = test_driver();
        let handle = driver.handle();

        assert_eq!(driver.inner.borrow().slab.outstanding(), 0);
        handle.shutdown();

        let started = std::time::Instant::now();
        let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
        drop(wakers);
        let elapsed = started.elapsed();

        assert_eq!(outcome, StepOutcome::Quiescent);
        assert!(
            elapsed < std::time::Duration::from_millis(DRAIN_TIMEOUT_MS as u64),
            "an empty drain waited {elapsed:?}, so it must have issued a wait it did not need"
        );

        drop(driver);
    }

    /// SC-025: a drain that will not converge is unbounded by design, so it must
    /// say so — otherwise a stalled shutdown is indistinguishable from a hang.
    ///
    /// Throttled for the same reason submission failures are: a long shutdown
    /// must not bury the observer. Reaping is withheld for more rounds than the
    /// reporting threshold, so the drain genuinely cannot converge while the
    /// reports are being counted.
    #[test]
    fn a_drain_that_will_not_converge_reports_itself_but_is_throttled() {
        let reports = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&reports);

        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::with_error_observer(
            ring,
            Some(Box::new(move |e: &Error| {
                sink.borrow_mut().push(format!("{e}"));
            })),
        )
        .unwrap();
        let handle = driver.handle();
        let file = readme();

        let fut = handle.read(&file, vec![0_u8; 128], 64, 0);
        driver.inner.borrow_mut().submit_pending();
        drop(fut);

        // Long enough that the drain cannot converge before the throttle has had
        // to make a decision more than once.
        let rounds = SLOW_DRAIN_INTERVAL * 3;
        driver.inner.borrow_mut().withhold_reaps = rounds;
        drop(driver);

        let reports = reports.borrow();
        let stalls: Vec<_> = reports
            .iter()
            .filter(|r| r.contains("outstanding"))
            .collect();
        assert!(
            !stalls.is_empty(),
            "a drain that will not converge must report itself, saw {reports:?}"
        );
        assert!(
            stalls.len() <= (rounds / SLOW_DRAIN_INTERVAL) as usize,
            "reports must be throttled to one per interval, not emitted every pass: {} for {rounds} rounds",
            stalls.len()
        );
        assert!(
            stalls[0].contains('1'),
            "the report must name the outstanding count: {}",
            stalls[0]
        );
    }

    /// SC-020a: a drain abandoned by dropping the driver's future must be
    /// resumable, not leave the driver wedged.
    ///
    /// This is why re-entry is guarded on teardown having *finished* rather than
    /// merely having started. Guarding on the latter would make the second call
    /// return immediately, leaving the ring open and everything in it stranded.
    #[test]
    fn a_drain_abandoned_by_dropping_the_driver_future_can_be_resumed() {
        let driver = test_driver();
        let handle = driver.handle();
        let file = readme();

        let mut fut = Box::pin(handle.read(&file, vec![0_u8; 128], 64, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        handle.shutdown_now();

        // Poll the driver far enough to enter teardown, then abandon it.
        let mut drive = Box::pin(driver.drive());
        driver.inner.borrow_mut().withhold_reaps = 64;
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert!(
            driver.inner.borrow().teardown_started,
            "teardown must have begun for abandoning it to mean anything"
        );
        assert!(
            !driver.inner.borrow().torn_down,
            "teardown must be unfinished for abandoning it to mean anything"
        );
        drop(drive);

        // A fresh call resumes rather than returning as though it were done.
        driver.inner.borrow_mut().withhold_reaps = 0;
        futures::executor::block_on(driver.drive());
        assert!(
            driver.inner.borrow().torn_down,
            "a resumed drain must finish teardown"
        );
        assert!(fut.as_mut().poll(&mut cx).is_ready());

        drop(driver);
    }

    /// SC-009: a cancellation the platform refuses to enqueue must be tried
    /// again, or one refusal would silently make an operation permanently
    /// uncancellable and turn an immediate shutdown into a graceful one.
    ///
    /// The attempt count is what stops this passing vacuously. "Everything
    /// resolved" is satisfied by operations that simply finished on their own,
    /// which is why the operations here are pipe reads that cannot: nothing
    /// writes to the pipe, so only a cancellation can end them.
    #[test]
    fn a_drain_retries_cancellations_the_platform_refuses() {
        const OPERATIONS: u32 = 2;

        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();

        let mut first = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let mut second = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());

        handle.shutdown_now();
        // Enough refusals for two full rounds against both operations.
        driver.inner.borrow_mut().fail_next_cancels = 2 * OPERATIONS;

        for _ in 0..2 {
            let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
            drop(wakers);
            assert_eq!(
                outcome,
                StepOutcome::Progressing,
                "nothing can finish while every cancellation is refused"
            );
        }

        assert!(
            driver.inner.borrow().cancel_attempts > OPERATIONS,
            "a refused cancellation must be retried, not recorded as done: {} attempts for {OPERATIONS} operations",
            driver.inner.borrow().cancel_attempts
        );
        assert!(
            first.as_mut().poll(&mut cx).is_pending() && second.as_mut().poll(&mut cx).is_pending(),
            "a refused cancellation must not resolve its operation"
        );

        // With the injected refusals exhausted, the retries get through and the
        // drain finishes.
        drop(driver);
        assert!(
            first.as_mut().poll(&mut cx).is_ready() && second.as_mut().poll(&mut cx).is_ready(),
            "every operation must be resolved by the time teardown ends"
        );
    }

    /// SC-011a: a graceful drain already under way can still be escalated, and
    /// the escalation reaches the operations still in flight.
    ///
    /// The single most load-bearing test of the design. Everything here is
    /// `!Send`, so `shutdown_now` can only be called from the driver's own
    /// thread — a drain that blocked that thread outright would make escalation
    /// unreachable, and a graceful drain never cancels, so escalation is the
    /// only way out of one that has stalled. A single blocking drain loop shared
    /// by `drive` and `Drop` would hang here forever.
    #[test]
    fn a_graceful_drain_can_be_escalated_to_immediate() {
        let driver = test_driver();
        let handle = driver.handle();
        let pipe = Pipe::new();

        let mut fut = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        handle.shutdown();

        // A graceful drain waits for an operation that will never finish, and
        // does not cancel it.
        for _ in 0..2 {
            let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
            drop(wakers);
            assert_eq!(
                outcome,
                StepOutcome::Progressing,
                "a graceful drain cannot finish while an operation is stuck"
            );
        }
        assert_eq!(
            driver.inner.borrow().cancel_attempts,
            0,
            "a graceful shutdown must not cancel anything"
        );
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Escalate part-way through, exactly as another task sharing the
        // driver's thread would.
        handle.shutdown_now();

        let mut outcome = StepOutcome::Progressing;
        for _ in 0..32 {
            let (wakers, step) = driver.inner.borrow_mut().drain_step();
            for waker in wakers {
                waker.wake();
            }
            outcome = step;
            if step != StepOutcome::Progressing {
                break;
            }
        }
        assert_eq!(
            outcome,
            StepOutcome::Quiescent,
            "escalation must let a stalled drain finish"
        );
        assert!(
            driver.inner.borrow().cancel_attempts > 0,
            "escalation must reach the operations still in flight"
        );
        assert!(
            fut.as_mut().poll(&mut cx).is_ready(),
            "the escalated operation must be resolved"
        );

        drop(driver);
    }

    /// SC-024: an operation that reports only once the drain is already under
    /// way must still have its report delivered, which is what "the ring is not
    /// closed while the kernel still holds something" looks like from outside.
    ///
    /// The pipe is what makes the ordering a fact: the read cannot complete
    /// before the write, and the write happens after the drain has taken a step.
    /// The shutdown is graceful so that nothing cancels the read — the result
    /// asserted below is the operation's own, not a cancellation's.
    #[test]
    fn an_operation_reporting_mid_drain_is_still_delivered() {
        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        let mut fut = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        handle.shutdown();

        let (wakers, outcome) = driver.inner.borrow_mut().drain_step();
        drop(wakers);
        assert_eq!(outcome, StepOutcome::Progressing);
        assert_eq!(
            driver.inner.borrow().slab.awaiting_kernel(),
            1,
            "the kernel must still hold the read once the drain has begun"
        );
        assert!(fut.as_mut().poll(&mut cx).is_pending());

        // Only now does the operation become able to report.
        pipe.write_from_client(b"late");

        let mut outcome = StepOutcome::Progressing;
        for _ in 0..32 {
            let (wakers, step) = driver.inner.borrow_mut().drain_step();
            for waker in wakers {
                waker.wake();
            }
            outcome = step;
            if step != StepOutcome::Progressing {
                break;
            }
        }
        assert_eq!(outcome, StepOutcome::Quiescent);

        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(result) => {
                let (transferred, buffer) = result.into_parts();
                let transferred =
                    transferred.expect("a report that arrived during the drain must survive it");
                assert_eq!(transferred as usize, 4);
                assert_eq!(
                    &buffer[..4],
                    b"late",
                    "the caller's own data must come back"
                );
            }
            Poll::Pending => panic!("a report delivered during the drain was lost"),
        }

        drop(driver);
    }

    /// SC-013: an immediate shutdown must not disturb an operation the driver
    /// never issued, even on a file handle it uses itself.
    ///
    /// A cancellation names one specific operation's user data, so it cannot
    /// reach a foreign ring's work — but that is an argument, and this makes it
    /// an observation. The external read is genuinely still pending when the
    /// shutdown happens: nothing has been written to the pipe yet, so it cannot
    /// pass by having already finished.
    #[test]
    fn an_immediate_shutdown_leaves_operations_it_did_not_issue_alone() {
        use crate::io_ring::ops::ReadOp;

        const EXTERNAL_USER_DATA: usize = 0xE0E0;

        let driver = test_driver();
        let handle = driver.handle();
        let mut pipe = Pipe::new();

        // A second ring, sharing the pipe handle but nothing else.
        let mut external = IoRing::builder().build().unwrap();
        let mut external_buf = vec![0_u8; 16];
        let read = ReadOp::builder()
            .with_raw_handle(pipe.server.as_raw_handle())
            .with_raw_data_address(external_buf.as_mut_ptr().cast())
            .with_num_of_bytes_to_read(external_buf.len() as u32)
            .with_offset(0)
            .with_user_data(EXTERNAL_USER_DATA)
            .build()
            .unwrap();
        // SAFETY: `pipe` and `external_buf` both outlive `external`, which is
        // closed at the end of this test before either is dropped.
        unsafe { external.build_read_file(read) }.unwrap();
        external.submit(0, 0).unwrap();

        // The driver's own operation on the same handle, queued after the
        // external one so the external read is first in line for any data.
        let mut ours = Box::pin(handle.read(&pipe.server, vec![0_u8; 16], 16, 0));
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        assert!(ours.as_mut().poll(&mut cx).is_pending());

        assert!(
            external.pop_completion().unwrap().is_none(),
            "the external read must still be pending for this test to mean anything"
        );

        handle.shutdown_now();
        let mut outcome = StepOutcome::Progressing;
        for _ in 0..32 {
            let (wakers, step) = driver.inner.borrow_mut().drain_step();
            for waker in wakers {
                waker.wake();
            }
            outcome = step;
            if step != StepOutcome::Progressing {
                break;
            }
        }
        assert_eq!(
            outcome,
            StepOutcome::Quiescent,
            "the driver's own operation must be cancelled and drained"
        );
        assert!(ours.as_mut().poll(&mut cx).is_ready());
        drop(driver);

        // The external operation survived the shutdown untouched: still pending,
        // and still able to report its own outcome.
        assert!(
            external.pop_completion().unwrap().is_none(),
            "an immediate shutdown must not cancel an operation the driver never issued"
        );

        pipe.write_from_client(b"mine");
        external.submit(1, 5000).unwrap();
        let cqe = external
            .pop_completion()
            .unwrap()
            .expect("the external read must report its own outcome");
        assert_eq!(cqe.UserData, EXTERNAL_USER_DATA);
        cqe.ResultCode.ok().expect("the external read must succeed");
        assert_eq!(cqe.Information, 4);
        assert_eq!(&external_buf[..4], b"mine");

        external.close().unwrap();
    }

    // ---- the same-thread nudge -------------------------------------------
    //
    // These are the tests for the wakeup guarantee. Each asserts the `parks`
    // counter or a wake count rather than merely that the work finished: a
    // driver that reached the same end state by parking and being woken by an
    // unrelated completion would satisfy "the read completed" while the
    // mechanism under test was broken.

    /// A waker that counts wakes, so a test can tell being woken from being
    /// polled again for some other reason.
    struct CountingWake(std::sync::atomic::AtomicUsize);

    impl std::task::Wake for CountingWake {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        fn wake_by_ref(self: &std::sync::Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl CountingWake {
        fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(CountingWake(std::sync::atomic::AtomicUsize::new(0)))
        }
        fn count(self: &std::sync::Arc<Self>) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[test]
    fn work_queued_before_the_first_poll_is_seen_on_the_first_pass() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let file = readme();

        // Queue before `drive` has ever been polled, so there is no waker to
        // take and the flag is the only record that survives.
        let mut fut = Box::pin(handle.read(&file, vec![0_u8; 64], 20, 0));
        assert!(
            driver.inner.borrow().nudged,
            "queuing work must leave a durable record even with no driver parked"
        );

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert!(
            !driver.inner.borrow().nudged,
            "the first pass must consume the nudge"
        );
        assert!(
            driver.inner.borrow().passes >= 1,
            "the nudge raised before any poll must produce a pass"
        );

        // Settle so teardown is clean.
        for _ in 0..200 {
            if fut.as_mut().poll(&mut cx).is_ready() {
                break;
            }
            let _ = drive.as_mut().poll(&mut cx);
        }
        handle.shutdown_now();
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    #[test]
    fn work_queued_while_the_driver_runs_is_seen_without_parking() {
        // The flag-only path: work is queued while the driver is *running*, so
        // there is no waker to take and nothing wakes anything. The next park
        // must consume the flag and resolve rather than suspend.
        //
        // Asserted on passes rather than parks, because a park that ignored the
        // flag would also suspend exactly once per poll — see the vacuity trap
        // recorded in docs/testing.md.
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        // A poll with nothing outstanding: one pass, then it parks.
        let before = driver.inner.borrow().passes;
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().passes - before,
            1,
            "an idle poll should run exactly one pass"
        );

        // Control: polling again while still parked resumes *inside* the park,
        // so with nothing to report it runs no pass at all. This is what makes
        // the assertion below meaningful rather than a count of polls.
        let before = driver.inner.borrow().passes;
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().passes - before,
            0,
            "a re-poll with nothing outstanding must not run a pass"
        );

        // Raise the flag the way code inside a pass does — mark only, no waker
        // taken, nothing woken.
        let wakes_before = waker.count();
        driver.inner.borrow_mut().mark_nudged();
        assert_eq!(
            waker.count(),
            wakes_before,
            "marking must not wake; this is the path where no waker exists"
        );

        // The park must now resolve on the flag alone and let a pass run,
        // rather than suspending on work it has already been told about.
        let before = driver.inner.borrow().passes;
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().passes - before,
            1,
            "the park must resolve on the flag and run a pass, not suspend"
        );
        assert!(
            !driver.inner.borrow().nudged,
            "the park must consume the flag"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    #[test]
    fn a_nudge_between_the_check_and_the_park_still_causes_a_pass() {
        // The narrowest window in the design: the driver has satisfied itself
        // that nothing is outstanding, and work arrives before it is parked.
        // Reached here by raising the nudge directly between two polls, which
        // is the same state the window produces.
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        // Park with nothing to do.
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        let passes = driver.inner.borrow().passes;
        assert_eq!(
            driver.inner.borrow().parks,
            1,
            "the driver should have parked"
        );

        // Raise a nudge with the driver parked. It must both wake the task and
        // leave a record, so the next poll runs another pass.
        let before = waker.count();
        handle.nudge_driver();
        assert!(
            waker.count() > before,
            "a nudge raised against a parked driver must wake it"
        );

        assert!(drive.as_mut().poll(&mut cx).is_pending());
        // Asserting on passes, not parks: a park that ignored the nudge and
        // simply re-armed would also increment `parks`, so `parks` alone cannot
        // tell the two apart. Only another pass proves the nudge was honoured.
        assert_eq!(
            driver.inner.borrow().passes,
            passes + 1,
            "a nudge must produce another pass, not merely another suspension"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    #[test]
    fn a_nudge_reaches_the_task_that_is_driving_now() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let first = CountingWake::new();
        let second = CountingWake::new();
        let w1 = std::task::Waker::from(std::sync::Arc::clone(&first));
        let w2 = std::task::Waker::from(std::sync::Arc::clone(&second));

        let drive = driver.drive();
        futures::pin_mut!(drive);

        // Park under the first waker, then re-poll under the second, as an
        // executor that moved the task between workers would.
        assert!(
            drive
                .as_mut()
                .poll(&mut Context::from_waker(&w1))
                .is_pending()
        );
        assert!(
            drive
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_pending()
        );

        let first_before = first.count();
        let second_before = second.count();
        handle.nudge_driver();

        assert_eq!(
            first.count(),
            first_before,
            "the waker from the previous park must not be used"
        );
        assert!(
            second.count() > second_before,
            "the nudge must reach whichever task is driving now"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_ready()
            {
                break;
            }
        }
    }

    #[test]
    fn repeated_nudges_before_a_pass_produce_one_pass() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        assert!(drive.as_mut().poll(&mut cx).is_pending());
        let passes = driver.inner.borrow().passes;

        handle.nudge_driver();
        handle.nudge_driver();
        handle.nudge_driver();

        // Three nudges, one outstanding pass: the flag collapses them, and only
        // the first found a waker to take.
        assert_eq!(
            waker.count(),
            1,
            "only the nudge that found the driver parked should wake it"
        );
        assert!(drive.as_mut().poll(&mut cx).is_pending());
        assert_eq!(
            driver.inner.borrow().passes,
            passes + 1,
            "three nudges must produce one additional pass, not three"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    #[test]
    fn shutdown_wakes_a_parked_driver_on_its_own() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        assert!(drive.as_mut().poll(&mut cx).is_pending());
        let before = waker.count();

        // No completion can arrive — nothing was ever issued — so if shutdown
        // does not wake the park itself, nothing ever will.
        handle.shutdown_now();
        assert!(
            waker.count() > before,
            "a shutdown request must wake a parked driver by itself"
        );

        let mut finished = false;
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                finished = true;
                break;
            }
        }
        assert!(finished, "the driver should have torn down");
        assert!(driver.inner.borrow().torn_down);
    }

    #[test]
    fn a_nudge_after_shutdown_does_not_obstruct_teardown() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        assert!(drive.as_mut().poll(&mut cx).is_pending());
        handle.shutdown_now();
        // Raise a nudge mid-teardown; it must be inert rather than obstructive.
        handle.nudge_driver();

        let mut finished = false;
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                finished = true;
                break;
            }
            handle.nudge_driver();
        }
        assert!(finished, "nudges during teardown must not prevent it");
        assert!(driver.inner.borrow().torn_down);
    }

    #[test]
    fn an_abandoned_park_leaves_no_waker_behind() {
        // The `drive` future can be dropped mid-park. The flag is durable, so
        // nothing is stranded, but the waker slot must not outlive the park.
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);

        {
            let drive = driver.drive();
            futures::pin_mut!(drive);
            assert!(drive.as_mut().poll(&mut cx).is_pending());
            assert!(
                driver.inner.borrow().driver_waker.is_some(),
                "a parked driver should have recorded its waker"
            );
        }

        assert!(
            driver.inner.borrow().driver_waker.is_none(),
            "an abandoned park must not leave its waker behind"
        );

        // A fresh driving task still sees work raised while nobody was driving.
        handle.nudge_driver();
        assert!(driver.inner.borrow().nudged);

        handle.shutdown_now();
        let drive = driver.drive();
        futures::pin_mut!(drive);
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    // ---- the armed completion wait ---------------------------------------

    #[test]
    fn a_completion_signalled_mid_pass_is_not_lost() {
        // Withhold reaping so a completion is signalled while the driver is
        // between its last look and its park. The armed wait's signal is
        // sticky, so the next park must resolve rather than suspend on work the
        // platform has already finished.
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let file = readme();

        driver.inner.borrow_mut().withhold_reaps = 1;
        let mut fut = Box::pin(handle.read(&file, vec![0_u8; 64], 20, 0));

        let waker = CountingWake::new();
        let w = std::task::Waker::from(std::sync::Arc::clone(&waker));
        let mut cx = Context::from_waker(&w);
        let drive = driver.drive();
        futures::pin_mut!(drive);

        let mut resolved = false;
        for _ in 0..500 {
            let _ = drive.as_mut().poll(&mut cx);
            if fut.as_mut().poll(&mut cx).is_ready() {
                resolved = true;
                break;
            }
        }
        assert!(
            resolved,
            "a completion signalled while the driver was mid-pass was lost"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive.as_mut().poll(&mut cx).is_ready() {
                break;
            }
        }
    }

    #[test]
    fn a_driver_polled_by_a_new_task_is_still_woken_by_a_completion() {
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        let handle = driver.handle();
        let file = readme();

        let first = CountingWake::new();
        let second = CountingWake::new();
        let w1 = std::task::Waker::from(std::sync::Arc::clone(&first));
        let w2 = std::task::Waker::from(std::sync::Arc::clone(&second));

        let drive = driver.drive();
        futures::pin_mut!(drive);

        // Park under the first waker with nothing outstanding, then re-poll
        // under the second, as an executor moving the task would.
        assert!(
            drive
                .as_mut()
                .poll(&mut Context::from_waker(&w1))
                .is_pending()
        );
        assert!(
            drive
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_pending()
        );

        let first_before = first.count();
        let mut fut = Box::pin(handle.read(&file, vec![0_u8; 64], 20, 0));

        let mut resolved = false;
        for _ in 0..500 {
            let _ = drive.as_mut().poll(&mut Context::from_waker(&w2));
            if fut.as_mut().poll(&mut Context::from_waker(&w2)).is_ready() {
                resolved = true;
                break;
            }
        }
        assert!(resolved, "the read should have completed");
        assert_eq!(
            first.count(),
            first_before,
            "the waker from the earlier park must not be used"
        );
        assert!(
            second.count() > 0,
            "the wake must reach whichever task is driving now"
        );

        handle.shutdown_now();
        for _ in 0..200 {
            if drive
                .as_mut()
                .poll(&mut Context::from_waker(&w2))
                .is_ready()
            {
                break;
            }
        }
    }

    #[test]
    fn a_driver_whose_wait_cannot_be_armed_reports_it() {
        // A driver that silently could not be woken would hang rather than
        // fail, so this must surface through the constructor.
        crate::sys::ArmedEvent::fail_next_arm();
        let ring = IoRing::builder().build().unwrap();
        let result = Driver::new(ring);
        assert!(
            result.is_err(),
            "a wait that cannot be armed must fail construction"
        );
        // The seam is consumed, so a driver built afterwards works normally.
        let ring = IoRing::builder().build().unwrap();
        let driver = Driver::new(ring).unwrap();
        drop(driver);
    }
}
