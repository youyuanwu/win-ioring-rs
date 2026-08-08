//! Named pipe support.
//!
//! Two types: [`Client`], which connects to a pipe someone else is serving, and
//! `Server`, which creates instances and waits for clients to arrive. Both
//! produce a [`File`](crate::file::File), so once a connection exists every
//! read, write and flush goes through the ring exactly as it does for a file —
//! same futures, same owned-buffer discipline, same cancellation rules.
//!
//! # Accept does not go through the ring
//!
//! This is the shape of the whole module and it is worth stating before the API
//! rather than after it. IoRing, as this crate can drive it, has a **closed set
//! of operations**: no-op, read, write, flush, cancel, and the two registration
//! calls. There is no connect. The `windows` crate exposes one `BuildIoRing*`
//! function per operation and each hard-codes its own op code, so there is no
//! generic submission path a connect could be smuggled through either.
//!
//! The read and write halves of a pipe are therefore ordinary ring work, but
//! the server's accept cannot be. It runs as an overlapped `ConnectNamedPipe`
//! and completes through an event wait rather than through a completion queue
//! entry. That asymmetry is a property of the platform, not an artefact of this
//! crate's design, and it has consequences a caller can observe.
//!
//! # Dropping an accept does not cancel it
//!
//! This is the module's central safety argument, and it is the same one
//! `docs/buffer-ownership.md` makes for buffers: when the kernel is writing into
//! memory, that memory cannot belong to something the caller is free to drop.
//!
//! An overlapped `ConnectNamedPipe` hands the kernel a pointer to an
//! `OVERLAPPED` and returns immediately. The kernel writes the result into that
//! structure whenever the client eventually arrives — which may be long after
//! the caller has lost interest. If the `OVERLAPPED` lived inside the accept
//! future, dropping that future would free memory the kernel still holds a
//! pointer to, and the write would land in freed memory. That is a
//! use-after-free with no diagnostic: it corrupts whatever was allocated there
//! next, at a time unrelated to the code that caused it.
//!
//! So the `OVERLAPPED` lives in a heap allocation owned by the [`Server`], not
//! by the [`Accept`] future. Two consequences follow, and both are observable:
//!
//! - **Dropping an accept future does not cancel the accept.** The connect stays
//!   pending in the kernel and the server keeps the memory it is writing to. A
//!   dropped future abandons the caller's *interest*, not the operation, and a
//!   later [`Server::accept`] **resumes** that same operation rather than
//!   submitting a second one. A client that arrives while no future is waiting
//!   is therefore not lost.
//! - **The server, not the future, is what is torn down carefully.** Cancelling
//!   and collecting happens when the [`Server`] is dropped, which is the only
//!   point that can establish the kernel is finished with the allocation.
//!
//! # Reusing an instance takes an explicit accept
//!
//! Stated here because the natural assumption is wrong and the failure is
//! silent. A **freshly created** instance admits a client with no call from this
//! crate at all. An instance returned to service by [`Server::disconnect`] does
//! **not** — the platform refuses clients with `ERROR_PIPE_BUSY` until a further
//! [`Server::accept`] has submitted a connect. Measured, not inferred.
//!
//! So a server that disconnects a client and then waits for the next one without
//! accepting again serves exactly one client and then refuses every other, with
//! `Ok` from every call it makes. The idiom is `disconnect` followed immediately
//! by `accept`, and [`Server::accepts_clients`] reports which side of that gap
//! an instance is on.
//!
//! # What this module does not do
//!
//! - **Message mode is not supported.** Instances are byte mode. A partial read
//!   in message mode reports `ERROR_MORE_DATA`, and this crate's `BufResult`
//!   shape pairs one result with one buffer — it cannot express "here is part of
//!   a message" as either a success or a failure without losing which it was.
//! - **No wait-for-availability.** A [`Client`] that finds every instance busy
//!   reports [`Error::PipeBusy`](crate::Error::PipeBusy) immediately rather than
//!   blocking, which is what a Win32 caller would get from `WaitNamedPipe`.
//!   Retrying is the caller's to schedule, on the caller's own runtime.
//! - **No security attributes.** Instances are created with the default
//!   descriptor, which denies remote access but permits any local user. A server
//!   exposed to a less trusted local principal needs a descriptor this API does
//!   not yet accept.

mod client;
mod server;

pub use client::{Client, ClientOptions};
pub use server::{Accept, Server, ServerOptions};

/// The prefix every pipe path carries on the local machine.
const LOCAL_PREFIX: &str = r"\\.\pipe\";

/// Builds the full path for a pipe name on the local machine.
///
/// Accepts either a bare name or an already-qualified path, so a caller holding
/// one from elsewhere does not have to strip it. Matching is on the literal
/// `\\` prefix, so a path naming another host passes through untouched rather
/// than being rewritten: silently redirecting a remote pipe to the local
/// machine would connect the caller to a different pipe than the one they
/// named, which is a worse outcome than failing to open it.
pub(crate) fn qualify(name: &str) -> String {
    if name.starts_with(r"\\") {
        name.to_owned()
    } else {
        format!("{LOCAL_PREFIX}{name}")
    }
}

/// A pipe name unique to this test run, so tests may run in parallel.
///
/// The process id is what separates concurrent `cargo test` invocations; the
/// counter separates tests within one.
#[cfg(test)]
pub(crate) fn unique_name(tag: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "win-ioring-test-{tag}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_name_is_qualified_and_a_full_path_is_left_alone() {
        assert_eq!(qualify("demo"), r"\\.\pipe\demo");
        assert_eq!(qualify(r"\\.\pipe\demo"), r"\\.\pipe\demo");
        assert_eq!(qualify(r"\\host\pipe\demo"), r"\\host\pipe\demo");
    }
}
