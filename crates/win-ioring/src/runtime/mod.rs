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
    /// Keeps the target file's handle open for as long as the kernel may use it.
    _file: Rc<FileState>,
    /// Shared with the future awaiting this operation.
    slot: Rc<RefCell<ResultSlot>>,
}

/// Shared driver state, reached by handles and futures.
struct DriverInner {
    ring: IoRing,
    slab: OpSlab,
    /// Set when entries are queued but not yet accepted by the kernel.
    ///
    /// While set, a retry is owed and no completion can arrive to prompt it, so
    /// the driver must schedule its own wake.
    pending_submit: bool,
    shutting_down: bool,
    torn_down: bool,
    on_error: Option<ErrorObserver>,
}

impl DriverInner {
    fn report(&self, error: &Error) {
        if let Some(observer) = &self.on_error {
            observer(error);
        }
    }

    /// Hands queued entries to the kernel.
    ///
    /// On failure every entry stays in the submission queue, so the affected
    /// buffers stay retained and a retry is owed.
    fn submit_pending(&mut self) {
        if !self.pending_submit {
            return;
        }
        match self.ring.submit(0, 0) {
            Ok(_) => {
                self.pending_submit = false;
                self.slab.promote_built_to_submitted();
            }
            Err(e) => {
                self.report(&e);
                // Leave `pending_submit` set. The entries are still queued and
                // their buffers must stay retained until the kernel takes them.
            }
        }
    }

    /// Drains the completion queue.
    fn reap_completions(&mut self) {
        loop {
            let cqe = match self.ring.pop_completion() {
                Ok(Some(cqe)) => cqe,
                Ok(None) => return,
                Err(e) => {
                    self.report(&e);
                    return;
                }
            };

            let token = Token::from_user_data(cqe.UserData);

            if token.kind() == TokenKind::Cancel {
                // A cancellation's completion never releases the target's
                // buffer; it only retires the cancellation's own bookkeeping.
                self.slab.complete_cancel(token);
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
            let waker = {
                let mut slot = payload.slot.borrow_mut();
                slot.completed = Some((result, buffer));
                slot.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    /// Closes the ring, releasing what is safe to release and leaking the rest.
    fn teardown(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;
        self.shutting_down = true;

        // One last sweep: anything already finished can be delivered normally.
        self.reap_completions();

        if self.slab.outstanding() == 0 {
            let _ = self.ring.close();
            drop(self.slab.drain());
            return;
        }

        // Work is still outstanding and quiescence cannot be established.
        // Resolve every waiting future *before* abandoning the buffers: the
        // payloads hold the wakers, so leaking first would hang those futures
        // forever rather than reporting to them.
        let mut wakers = Vec::new();
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

        for waker in wakers {
            waker.wake();
        }
    }
}

/// Owns the ring and drives it to completion.
///
/// Spawn [`Driver::drive`] on your executor and issue operations through
/// [`Driver::handle`]. The driver, its handles, and its futures are all
/// single-threaded by design and cannot be sent between threads.
pub struct Driver {
    inner: Rc<RefCell<DriverInner>>,
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
                pending_submit: false,
                shutting_down: false,
                torn_down: false,
                on_error: observer,
            })),
            completion_event,
            wake,
        })
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
            let (shutting_down, retry_owed) = {
                let mut inner = self.inner.borrow_mut();
                inner.submit_pending();
                inner.reap_completions();
                (inner.shutting_down, inner.pending_submit)
            };

            if shutting_down {
                break;
            }

            if retry_owed {
                // No completion can arrive to prompt the retry, so yield and
                // come straight back rather than blocking on an event.
                futures::pending!();
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

        self.inner.borrow_mut().teardown();
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        // An abrupt drop gets the same treatment as a requested shutdown: no
        // memory the kernel might still reach is freed.
        self.inner.borrow_mut().teardown();
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
        mut buffer: B,
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
            _file: file.state(),
            slot: Rc::clone(&slot),
        });
        let token = match inner.slab.insert(payload) {
            Ok(token) => token,
            Err(_) => return Err((Error::QueueFull, buffer)),
        };

        let data_ptr = {
            let payload = inner
                .slab
                .payload_mut(token)
                .and_then(|p| p.downcast_mut::<OpPayload>())
                .expect("just inserted");
            let ptr = buffer.buf_mut_ptr();
            payload.buffer = Some(Box::new(buffer));
            ptr
        };

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
    /// operation's terminal result.
    pub fn cancel(&self, id: OperationId) {
        // Registering is the whole job for now: it makes a repeat request a
        // no-op and records that a cancellation is pending. Issuing the
        // platform request is Phase 3c's work.
        let mut inner = self.strong.borrow_mut();
        let _ = inner.slab.register_cancel(id.0);
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
        // when its own completion is dequeued, never here.
        if let Some(Lifecycle::Described) = inner.slab.detach(*token) {
            // Nothing was ever built, so no queue entry references the buffer
            // and it can be released now. Built or submitted operations must
            // keep theirs; cancelling those arrives in the next phase.
            drop(inner.slab.complete(*token));
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
}
