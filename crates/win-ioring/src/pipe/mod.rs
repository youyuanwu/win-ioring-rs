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

pub use client::{Client, ClientOptions};

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
