//! The pipe surface's own I/O futures.
//!
//! # Why these exist at all
//!
//! Without them the per-API split would be decorative. [`Client`](super::Client)
//! and [`Server`](super::Server) had no I/O methods of their own: a caller
//! reached a pipe's reads and writes through the [`File`] the pipe holds, and so
//! got a file error from a pipe operation. A `pipe::Error` with nothing
//! returning it would have compiled, passed review, and changed nothing.
//!
//! # Why they are wrappers
//!
//! Same reason as the file surface's, and it is a constraint rather than a
//! preference. The driver classifies a completion inside the region
//! `docs/performance.md`'s published matrix times. Threading the error type
//! through the driver's futures would reach into that path; wrapping at the
//! boundary reaches nothing that is measured.
//!
//! The wrapper also has to hold a *rejection*, because [`Server`](super::Server)
//! can refuse an operation before it reaches the kernel — a server that is still
//! listening, or has an accept outstanding, has no connection to read from.
//! Those conditions have no `HRESULT`, so no driver future can carry them, which
//! is the same reason `file::SequentialRead` holds one.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::Error;
use crate::buf::{BufResult, IoBuf, IoBufMut};
use crate::file::File;
use crate::runtime::{Handle, OperationId, ReadFuture, WriteFuture};

/// A pipe read in progress, reporting failures as [`Error`].
pub struct PipeRead<B: IoBufMut> {
    inner: Option<ReadFuture<B>>,
    /// A refusal that never reached the kernel, with the caller's buffer.
    rejected: Option<(Error, B)>,
}

impl<B: IoBufMut> PipeRead<B> {
    /// Issues the read against `file` through `handle`.
    pub(super) fn issue(handle: &Handle, file: &File, buffer: B, len: u32, offset: u64) -> Self {
        Self {
            inner: Some(handle.read(file, buffer, len, offset)),
            rejected: None,
        }
    }

    /// Refuses the read before it reaches the kernel, keeping the buffer.
    pub(super) fn rejected(error: Error, buffer: B) -> Self {
        Self {
            inner: None,
            rejected: Some((error, buffer)),
        }
    }

    /// This operation's identifier, for cancellation.
    ///
    /// Absent if the operation was refused before reaching the kernel.
    pub fn operation_id(&self) -> Option<OperationId> {
        self.inner.as_ref().and_then(|f| f.operation_id())
    }
}

impl<B: IoBufMut> Future for PipeRead<B> {
    type Output = BufResult<u32, B, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some((error, buffer)) = self.rejected.take() {
            return Poll::Ready(BufResult::new(Err(error), buffer));
        }
        let inner = self
            .inner
            .as_mut()
            .expect("a pipe operation is either refused or in flight");
        let outcome = std::task::ready!(Pin::new(inner).poll(cx));
        // The boundary conversion. `Handle::read` is the same entry point the
        // file surface uses, so this converts from `runtime::Error`, not from
        // `file::Error` — the two surfaces are siblings, not a chain.
        Poll::Ready(BufResult::new(
            outcome.result.map_err(Error::from),
            outcome.buffer,
        ))
    }
}

/// A pipe write in progress, reporting failures as [`Error`].
pub struct PipeWrite<B: IoBuf> {
    inner: Option<WriteFuture<B>>,
    /// See [`PipeRead::rejected`].
    rejected: Option<(Error, B)>,
}

impl<B: IoBuf> PipeWrite<B> {
    /// Issues the write against `file` through `handle`.
    pub(super) fn issue(handle: &Handle, file: &File, buffer: B, len: u32, offset: u64) -> Self {
        Self {
            inner: Some(handle.write(file, buffer, len, offset)),
            rejected: None,
        }
    }

    /// Refuses the write before it reaches the kernel, keeping the buffer.
    pub(super) fn rejected(error: Error, buffer: B) -> Self {
        Self {
            inner: None,
            rejected: Some((error, buffer)),
        }
    }

    /// This operation's identifier, for cancellation.
    ///
    /// Absent if the operation was refused before reaching the kernel.
    pub fn operation_id(&self) -> Option<OperationId> {
        self.inner.as_ref().and_then(|f| f.operation_id())
    }
}

impl<B: IoBuf> Future for PipeWrite<B> {
    type Output = BufResult<u32, B, Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some((error, buffer)) = self.rejected.take() {
            return Poll::Ready(BufResult::new(Err(error), buffer));
        }
        let inner = self
            .inner
            .as_mut()
            .expect("a pipe operation is either refused or in flight");
        let outcome = std::task::ready!(Pin::new(inner).poll(cx));
        Poll::Ready(BufResult::new(
            outcome.result.map_err(Error::from),
            outcome.buffer,
        ))
    }
}
