//! The server end of a named pipe.
//!
//! The `OVERLAPPED` ownership rule this module implements — and the reason a
//! dropped accept resumes rather than reissues — is argued in the
//! [`pipe` module documentation](crate::pipe), which is where a caller will
//! look for it. What follows is the part that only matters from inside.
//!
//! The allocation the kernel writes into is owned by [`Server`] and reached
//! through `AcceptSlot`. Every path that could free it while the kernel may
//! still hold the pointer either cancels and collects first, or leaks it
//! deliberately. Leaking is the correct outcome on those paths and is not a
//! defect to be tidied away later: a leak costs one allocation, and the
//! alternative is a write into memory that has been handed to something else.

use crate::error::Error;
use crate::file::File;
use crate::sys::ArmedEvent;
use windows::Win32::Foundation::{
    ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_FLAGS_AND_ATTRIBUTES,
    PIPE_ACCESS_DUPLEX, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::PCWSTR;

/// The default size, in bytes, suggested to the system for each direction.
const DEFAULT_BUFFER: u32 = 4096;

/// `STATUS_PENDING`, as it appears in `OVERLAPPED::Internal`.
///
/// The kernel overwrites this field with the operation's final status when it
/// completes, which is what lets the outstanding-accept observable be a fact
/// about the operation rather than a record of what was submitted.
const STATUS_PENDING_INTERNAL: usize = 0x0000_0103;

// Test seam: forces the next cancel-and-collect to fail.
//
// `CancelIoEx` has no reliably reproducible failure mode, so the server's
// leak-rather-than-free teardown branch cannot be exercised without injecting
// one. The same reason `ArmedEvent::fail_next_arm` exists, and a thread-local
// for the same reason: tests running in parallel must not consume each other's
// injection.
#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CANCEL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Options for creating a pipe instance.
///
/// ```no_run
/// use win_ioring::pipe::ServerOptions;
///
/// let server = ServerOptions::new().max_instances(4).create("demo")?;
/// # Ok::<(), win_ioring::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct ServerOptions {
    inbound: bool,
    outbound: bool,
    max_instances: u32,
    in_buffer: u32,
    out_buffer: u32,
    first_instance_only: bool,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ServerOptions {
    /// Default options: duplex, unlimited instances, 4 KiB buffers each way.
    pub fn new() -> Self {
        Self {
            inbound: true,
            outbound: true,
            max_instances: PIPE_UNLIMITED_INSTANCES,
            in_buffer: DEFAULT_BUFFER,
            out_buffer: DEFAULT_BUFFER,
            first_instance_only: false,
        }
    }

    /// Whether the server may read from the client. Defaults to `true`.
    pub fn access_inbound(mut self, allow: bool) -> Self {
        self.inbound = allow;
        self
    }

    /// Whether the server may write to the client. Defaults to `true`.
    pub fn access_outbound(mut self, allow: bool) -> Self {
        self.outbound = allow;
        self
    }

    /// Caps how many instances of this pipe may exist.
    ///
    /// This is a **limit on creation, not a pool**. Setting it to four does not
    /// create four instances; it permits four to be created. Each
    /// [`ServerOptions::create`] call makes exactly one, and one instance serves
    /// exactly one client at a time.
    ///
    /// Worth stating because the opposite reading is natural, and it leads to a
    /// server that serves one client and appears to hang for every other.
    pub fn max_instances(mut self, max: u32) -> Self {
        self.max_instances = max;
        self
    }

    /// Suggests the inbound buffer size to the system, in bytes.
    pub fn in_buffer_size(mut self, bytes: u32) -> Self {
        self.in_buffer = bytes;
        self
    }

    /// Suggests the outbound buffer size to the system, in bytes.
    pub fn out_buffer_size(mut self, bytes: u32) -> Self {
        self.out_buffer = bytes;
        self
    }

    /// Requires this to be the first instance of the name.
    ///
    /// Creation fails if another process already serves this pipe, which is how
    /// a server declines to be impersonated by one that got there first.
    pub fn first_instance_only(mut self, first: bool) -> Self {
        self.first_instance_only = first;
        self
    }

    /// Creates one pipe instance.
    ///
    /// The instance exists and is listening as soon as this returns, so a client
    /// may connect before [`Server::accept`] is ever called. That client is not
    /// lost; see [`Server::accept`].
    ///
    /// Instances are **byte mode**, always opened for overlapped I/O. Message
    /// mode is not supported; see the [module documentation](crate::pipe).
    ///
    /// # Errors
    ///
    /// Fails if the name is already served and no further instance may be
    /// created, or if [`ServerOptions::first_instance_only`] was set and it is
    /// not the first. An options set with neither direction allowed is refused
    /// by the platform with `ERROR_INVALID_PARAMETER`.
    pub fn create(&self, name: impl AsRef<str>) -> crate::Result<Server> {
        let path = super::qualify(name.as_ref());
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        let access = match (self.inbound, self.outbound) {
            (true, true) => PIPE_ACCESS_DUPLEX,
            (true, false) => PIPE_ACCESS_INBOUND,
            (false, true) => PIPE_ACCESS_OUTBOUND,
            // Neither direction names no pipe anybody can use, and the three
            // access constants have no zero to encode it with. Passed to the
            // platform as the zero it is rather than invented into some other
            // error: measured, `CreateNamedPipeW` refuses a `dwOpenMode` with no
            // access bits outright, reporting `ERROR_INVALID_PARAMETER`. So the
            // platform's own answer is both correct and specific, and
            // substituting one here would only make it less so.
            (false, false) => FILE_FLAGS_AND_ATTRIBUTES(0),
        };

        let mut open_mode = access | FILE_FLAG_OVERLAPPED;
        if self.first_instance_only {
            open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
        }

        // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the
        // call. No security attributes are supplied, so the instance gets the
        // default descriptor — a limitation the module documentation records
        // rather than hides. The returned handle is owned by the `File` built
        // below and closed exactly once, when the last reference to it drops.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                self.max_instances,
                self.out_buffer,
                self.in_buffer,
                0,
                None,
            )
        };

        if handle.is_invalid() {
            return Err(Error::from_hresult(
                windows::core::Error::from_thread().code(),
            ));
        }

        Ok(Server {
            // SAFETY: `handle` came from a successful `CreateNamedPipeW` and
            // nothing else holds it, so this `File` takes sole ownership of it.
            file: unsafe { File::from_raw_handle(handle) },
            accept: AcceptState::Fresh,
            synchronous_accepts: 0,
            connect_submissions: 0,
        })
    }
}

/// The heap allocation an outstanding connect writes into, and the event that
/// announces it.
///
/// Both belong to the [`Server`] and are released only once the kernel is known
/// to be finished with them.
struct AcceptSlot {
    /// The structure the kernel writes the connect's result into.
    ///
    /// `Box` rather than an inline field because the address handed to the
    /// kernel must stay valid and unmoved for as long as the operation is
    /// outstanding, and a `Server` is an ordinary movable value. Boxing makes
    /// the address stable without constraining the server's movability, and it
    /// makes the `mem::forget` case a leak rather than a dangling write.
    overlapped: Box<OVERLAPPED>,
    /// Signalled by the kernel when the connect completes.
    ///
    /// A plain `ArmedEvent`, deliberately not `ManuallyDrop`: releasing the
    /// thread-pool registration is idempotent, so a teardown path can release it
    /// early and then let the value drop normally. Wrapping it would suppress
    /// the drop glue for its shared reference count too, leaking that.
    event: ArmedEvent,
    /// The event's raw handle value, recorded so a test can ask the operating
    /// system whether it is still open once its owner is gone.
    ///
    /// Deliberately the handle *value* and not a flag recording that a close
    /// step ran: a flag answers "did the code execute", which is not the
    /// question being asked.
    #[cfg(test)]
    event_handle_value: isize,
}

impl AcceptSlot {
    /// Whether the kernel has finished with this operation.
    fn completed(&self) -> bool {
        self.overlapped.Internal != STATUS_PENDING_INTERNAL
    }
}

enum AcceptState {
    /// Created and never connected.
    ///
    /// A client that arrives now is connected by the kernel with no call from
    /// here — which is why [`ServerOptions::create`] can be followed directly by
    /// a client. Distinct from `Idle` below, and the distinction is measured
    /// rather than assumed.
    Fresh,
    /// Disconnected from a previous client, with nothing submitted.
    ///
    /// **A client that arrives now is refused as busy**, unlike `Fresh`. The
    /// platform does not return a disconnected instance to the listening state;
    /// it takes a further `ConnectNamedPipe` to do that. Measured: after
    /// `DisconnectNamedPipe` a client open reports `ERROR_PIPE_BUSY`, and the
    /// same open succeeds once a connect has been submitted.
    ///
    /// Folding this into `Fresh` would make [`Server::accepts_clients`] report
    /// the opposite of the truth for every reused instance.
    Idle,
    /// A connect has been submitted and not yet collected.
    ///
    /// Covers both "still pending in the kernel" and "satisfied by the kernel
    /// but not yet observed by any future". Those are distinguished by reading
    /// the kernel's own status word, never by a separate flag.
    Accepting(AcceptSlot),
    /// A client is connected and that has been observed.
    Connected,
}

/// One instance of a named pipe, and the accepts issued against it.
///
/// See the [`pipe` module documentation](crate::pipe) for why the accept's `OVERLAPPED` is
/// owned here rather than by the accept future.
///
/// # Serving more than one client
///
/// One instance serves one client at a time. To serve several, create the
/// *next* instance as soon as an accept resolves and before handing the current
/// one on, so the name is never left without something listening:
///
/// ```no_run
/// # async fn demo() -> win_ioring::Result<()> {
/// use win_ioring::pipe::ServerOptions;
///
/// let mut server = ServerOptions::new().create("demo")?;
/// loop {
///     server.accept().await?;
///     // Create the replacement *before* giving this one away. Between the
///     // accept resolving and the next instance existing, a client that
///     // arrives finds every instance taken and is refused as busy.
///     let next = ServerOptions::new().create("demo")?;
///     let connected = std::mem::replace(&mut server, next);
///     serve(connected);
/// }
/// # }
/// # fn serve(_: win_ioring::pipe::Server) {}
/// ```
pub struct Server {
    file: File,
    accept: AcceptState,
    synchronous_accepts: u64,
    connect_submissions: u64,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("connected", &self.is_connected())
            .field("accept_outstanding", &self.accept_outstanding())
            .field("synchronous_accepts", &self.synchronous_accepts)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// Creates one instance with default options.
    pub fn create(name: impl AsRef<str>) -> crate::Result<Self> {
        ServerOptions::new().create(name)
    }

    /// The raw handle of this instance.
    pub fn as_raw_handle(&self) -> HANDLE {
        self.file.as_raw_handle()
    }

    /// Whether a connect is outstanding **in the kernel** right now.
    ///
    /// Derived from the kernel's own status word rather than from a flag set
    /// when the connect was submitted. The difference is not cosmetic: a
    /// submission flag cannot distinguish "the kernel is still waiting for a
    /// client" from "a client arrived and nobody has collected it yet", and this
    /// reports **only the first**.
    ///
    /// It is therefore the wrong thing to consult before reading or writing —
    /// `false` here does not mean a client is connected. Use
    /// [`Server::is_connected`] for that.
    pub fn accept_outstanding(&self) -> bool {
        match &self.accept {
            AcceptState::Accepting(slot) => !slot.completed(),
            _ => false,
        }
    }

    /// How many accepts have completed **synchronously**, because a client was
    /// already waiting when the connect was submitted.
    ///
    /// A diagnostic. It is the only way to tell from outside which of the two
    /// accept paths a connection took, and they differ enough — one never
    /// signals its event at all — that a test claiming to exercise one of them
    /// has to read this to know it did.
    pub fn synchronous_accepts(&self) -> u64 {
        self.synchronous_accepts
    }

    /// Whether a client is connected and that connection has been observed.
    pub fn is_connected(&self) -> bool {
        matches!(&self.accept, AcceptState::Connected)
    }

    /// Whether a client connecting **right now** would reach this instance.
    ///
    /// This exists because the answer is not what it looks like, and getting it
    /// wrong produces a server that silently stops serving. A freshly created
    /// instance accepts a client with no call from here at all. An instance
    /// returned to service by [`Server::disconnect`] does **not**: the platform
    /// refuses clients with a busy error until a further [`Server::accept`] has
    /// submitted a connect. Creating and reusing therefore leave the instance in
    /// visibly different states, and only this observable separates them.
    pub fn accepts_clients(&self) -> bool {
        match &self.accept {
            AcceptState::Fresh => true,
            AcceptState::Accepting(slot) => !slot.completed(),
            AcceptState::Idle | AcceptState::Connected => false,
        }
    }

    /// The connected file, for reads, writes and flushes through the ring.
    ///
    /// Lent as `&File`, never as `&mut File`. The sequential API
    /// ([`File::read`], [`File::write`]) is cursor-based, and a pipe ignores the
    /// file offset entirely — so on a pipe those calls return `Ok` together with
    /// bytes that did not come from where the cursor says they came from. Not
    /// lending `&mut File` keeps this type from *offering* an operation whose
    /// result is meaningless here.
    ///
    /// # Errors
    ///
    /// Refused unless a client is connected **and observed**, because the other
    /// states would produce wrong results rather than obvious ones: a read on a
    /// listening instance fails with [`Error::PipeListening`], and a read on an
    /// instance whose accept completed but was never collected would succeed
    /// against a peer the caller has not been told exists.
    pub fn file(&self) -> crate::Result<&File> {
        match &self.accept {
            AcceptState::Connected => Ok(&self.file),
            AcceptState::Fresh | AcceptState::Idle => Err(Error::PipeListening),
            AcceptState::Accepting(_) => Err(Error::AcceptOutstanding),
        }
    }

    /// Waits for a client to connect.
    ///
    /// # A client that arrived first is not a failure
    ///
    /// If a client connects between creating the instance and calling this, the
    /// platform reports `ERROR_PIPE_CONNECTED` from the connect call and **never
    /// signals the completion event**. That is resolved as a success on the
    /// spot, and [`Server::synchronous_accepts`] counts it. The distinction
    /// matters more than it looks: treating that code as an error drops a real
    /// connection, and waiting for the event that will never come hangs forever.
    /// Both are easy to write, and neither is caught by a test in which the
    /// server always accepts first.
    ///
    /// # Dropping the returned future does not cancel the accept
    ///
    /// The connect stays pending in the kernel, which is still writing into
    /// memory this server owns. A client that arrives afterwards is connected,
    /// and a later call to this method **resumes** that same operation — it does
    /// not submit a second one. See the [`pipe` module documentation](crate::pipe).
    ///
    /// # One at a time
    ///
    /// The returned future borrows the server exclusively, so a second accept
    /// cannot be created while one is alive. Two futures sharing one
    /// `OVERLAPPED` have no sound completion story, and the borrow makes that
    /// unrepresentable rather than merely refused at runtime:
    ///
    /// ```compile_fail
    /// # fn demo() -> win_ioring::Result<()> {
    /// use win_ioring::pipe::ServerOptions;
    ///
    /// let mut server = ServerOptions::new().create("demo")?;
    /// let first = server.accept();
    /// let second = server.accept();
    /// // Using `first` after the second call is what keeps its borrow live
    /// // past it, so this fails on the conflicting borrow rather than because
    /// // the borrow had already ended.
    /// drop(first);
    /// drop(second);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The same code with the first future released first compiles, which is
    /// what establishes that the failure above is the borrow and not a typo:
    ///
    /// ```
    /// # fn demo() -> win_ioring::Result<()> {
    /// use win_ioring::pipe::ServerOptions;
    ///
    /// let mut server = ServerOptions::new().create("demo")?;
    /// let first = server.accept();
    /// drop(first);
    /// let second = server.accept();
    /// drop(second);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Calling this on a server that already has an observed client resolves
    /// immediately without submitting anything, which is what the platform does
    /// with `ConnectNamedPipe` on a connected instance. It does not count as a
    /// synchronous accept, because no connect was made.
    pub fn accept(&mut self) -> Accept<'_> {
        match self.begin_accept() {
            Ok(true) => Accept::ready(),
            Ok(false) => Accept::waiting(self),
            Err(e) => Accept::failed(e),
        }
    }

    /// Submits or resumes the connect.
    ///
    /// `Ok(true)` means a client is connected now and the future has nothing to
    /// wait for. `Ok(false)` means the caller must wait on the slot's event.
    fn begin_accept(&mut self) -> crate::Result<bool> {
        match &self.accept {
            // Already serving somebody; nothing to submit.
            AcceptState::Connected => return Ok(true),
            AcceptState::Accepting(slot) => {
                // Resume, do not reissue. A second `ConnectNamedPipe` against an
                // instance that already has one outstanding would break the
                // one-operation-per-event premise the teardown reasoning rests
                // on — and it would deliver the client just as happily, so
                // nothing but the submission count can tell the two apart.
                return if slot.completed() {
                    self.collect().map(|()| true)
                } else {
                    Ok(false)
                };
            }
            // Both submit a connect, and they must: `Idle` in particular is not
            // serving clients until one is submitted.
            AcceptState::Fresh | AcceptState::Idle => {}
        }

        let event = ArmedEvent::new()?;
        #[cfg(test)]
        let event_handle_value = event.handle().0 as isize;

        let mut overlapped = Box::new(OVERLAPPED::default());
        overlapped.hEvent = event.handle();
        // Written before submitting rather than after. Setting it afterwards
        // would race the kernel's own write to the same field on a connect that
        // completes immediately, and would briefly leave a submitted operation
        // reading as complete.
        overlapped.Internal = STATUS_PENDING_INTERNAL;
        let ptr: *mut OVERLAPPED = &mut *overlapped;

        self.connect_submissions += 1;

        // SAFETY: `ptr` addresses a heap allocation this server owns and keeps
        // for as long as the operation is outstanding. The `Box` is moved into
        // `AcceptState::Accepting` below, which moves the pointer to the box and
        // not the pointee, so the address the kernel holds stays valid; it is
        // released only once the operation has been collected, or cancelled and
        // then collected, in `collect` or in `Drop`. `hEvent` names an event
        // kept alive beside it in the same slot for exactly as long.
        let issued = unsafe { ConnectNamedPipe(self.file.as_raw_handle(), Some(ptr)) };

        let slot = AcceptSlot {
            overlapped,
            event,
            #[cfg(test)]
            event_handle_value,
        };

        match issued {
            // A synchronous success. Not documented as reachable on an
            // overlapped handle, but if it happens the client is connected and
            // the kernel has finished with the structure, so the slot is dropped
            // rather than retained.
            Ok(()) => {
                self.accept = AcceptState::Connected;
                self.synchronous_accepts += 1;
                Ok(true)
            }
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {
                self.accept = AcceptState::Accepting(slot);
                Ok(false)
            }
            // The client got here first. This is a success, and the event will
            // never signal, so it must be recognised here or not at all.
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                self.accept = AcceptState::Connected;
                self.synchronous_accepts += 1;
                Ok(true)
            }
            Err(e) => {
                // The state is deliberately left as it was: the submission
                // never started, so the instance is exactly where it was
                // before, and `Fresh` and `Idle` differ in a way a blanket
                // reset would destroy. Dropping `slot` here is correct for the
                // same reason -- the kernel never took the pointer.
                Err(Error::from_hresult(e.code()))
            }
        }
    }

    /// Collects a completed connect and moves to `Connected`.
    fn collect(&mut self) -> crate::Result<()> {
        let AcceptState::Accepting(slot) = &self.accept else {
            return Ok(());
        };

        let mut transferred = 0_u32;
        // SAFETY: the handle is open for this server's whole life, and
        // `overlapped` is the structure handed to this very operation, still
        // owned here. `bWait` is false: waiting is the event's job, and passing
        // true would block inside this call whenever the status is still
        // pending — which is exactly how a completion that resolves *during* the
        // call becomes a deadlock.
        let result = unsafe {
            GetOverlappedResult(
                self.file.as_raw_handle(),
                &*slot.overlapped,
                &mut transferred,
                false,
            )
        };

        match result {
            Ok(()) => {
                self.accept = AcceptState::Connected;
                Ok(())
            }
            // A connect the client satisfied out from under the wait is still a
            // connect.
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                self.accept = AcceptState::Connected;
                Ok(())
            }
            // Not finished after all. Left outstanding rather than reported, so
            // the next poll waits on the same operation.
            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => Ok(()),
            Err(e) => Err(Error::from_hresult(e.code())),
        }
    }

    /// Disconnects the current client, freeing the instance for reuse.
    ///
    /// The client's handle is not closed by this; the client sees its own end
    /// break. Data the client has not yet read is **discarded**, so flush first
    /// if that matters.
    ///
    /// # This does not return the instance to listening
    ///
    /// Stated first because the opposite is the natural assumption and it
    /// produces a server that stops serving without any error. A disconnected
    /// instance **refuses clients with a busy error** until a further
    /// [`Server::accept`] has submitted a connect — unlike a freshly created
    /// one, which accepts a client with no call at all. Measured, not inferred:
    /// a client open between the disconnect and the next accept reports
    /// `ERROR_PIPE_BUSY`, and the same open succeeds once the connect is in.
    ///
    /// So the reuse idiom is `disconnect` immediately followed by `accept`, and
    /// [`Server::accepts_clients`] reports which side of that gap the instance
    /// is on.
    ///
    /// # Errors
    ///
    /// Refused with [`Error::AcceptOutstanding`] while an accept is in progress —
    /// including the case where the kernel has already satisfied it and no
    /// future has observed that yet. Disconnecting an instance the kernel is
    /// still writing a connect result for would leave that result describing a
    /// connection that no longer exists.
    ///
    /// Refused with [`Error::PipeListening`] when there is no client to
    /// disconnect.
    pub fn disconnect(&mut self) -> crate::Result<()> {
        match &self.accept {
            AcceptState::Connected => {}
            AcceptState::Fresh | AcceptState::Idle => return Err(Error::PipeListening),
            AcceptState::Accepting(_) => return Err(Error::AcceptOutstanding),
        }

        // SAFETY: the handle is open, and the state check above establishes that
        // no connect is outstanding, so nothing is writing into an `OVERLAPPED`
        // for this handle.
        unsafe { DisconnectNamedPipe(self.file.as_raw_handle()) }
            .map_err(|e| Error::from_hresult(e.code()))?;

        // `Idle`, not `Fresh`: the platform will not admit a client here until a
        // connect is submitted, and the two states exist to keep that difference
        // visible.
        self.accept = AcceptState::Idle;
        Ok(())
    }

    // ---- crate-internal observables --------------------------------------
    //
    // These exist so that tests can separate outcomes that are identical from
    // outside: a leak from a correct reclaim, a resumed accept from a reissued
    // one, a released handle from a released allocation. They are `pub(crate)`
    // and read from inside the crate, as the driver's internals already are, so
    // no public surface is added for a test's benefit.

    /// How many accept allocations this server currently holds.
    ///
    /// One while an accept is in progress, zero otherwise.
    #[cfg(test)]
    pub(crate) fn live_accept_allocations(&self) -> usize {
        match &self.accept {
            AcceptState::Accepting(_) => 1,
            _ => 0,
        }
    }

    /// A weak handle to the accept event's shared state.
    ///
    /// The direct analogue of `ArmedEvent::watch`, and it answers a different
    /// question from the handle value below: this reports whether the shared
    /// allocation was reclaimed, that reports whether the kernel handle was
    /// closed. The teardown path deliberately unfuses those two steps, so one
    /// can succeed while the other leaks.
    #[cfg(test)]
    pub(crate) fn accept_event_watch(&self) -> Option<std::sync::Weak<crate::sys::ArmedShared>> {
        match &self.accept {
            AcceptState::Accepting(slot) => Some(slot.event.watch()),
            _ => None,
        }
    }

    /// The raw value of the accept event's handle, recorded at submission.
    ///
    /// Kept so that a test can interrogate the operating system about a handle
    /// whose owner has since been dropped.
    #[cfg(test)]
    pub(crate) fn accept_event_handle_value(&self) -> Option<isize> {
        match &self.accept {
            AcceptState::Accepting(slot) => Some(slot.event_handle_value),
            _ => None,
        }
    }

    /// How many `ConnectNamedPipe` calls this server has made.
    ///
    /// The discriminand between resuming an abandoned accept and reissuing one:
    /// both deliver the client, both leave every other observable identical, and
    /// only this moves.
    #[cfg(test)]
    pub(crate) fn connect_submissions(&self) -> u64 {
        self.connect_submissions
    }

    /// Test seam: makes the next cancel-and-collect on this thread fail.
    ///
    /// `CancelIoEx` has no reproducible failure mode, so the teardown's
    /// leak-rather-than-free branch — the most safety-critical path here — is
    /// otherwise unreachable and would ship unexecuted.
    #[cfg(test)]
    #[allow(dead_code, reason = "the seam is built here and first used in the teardown work")]
    pub(crate) fn fail_next_cancel() {
        FAIL_NEXT_CANCEL.with(|f| f.set(true));
    }
}

/// The future returned by [`Server::accept`].
///
/// Holds no `OVERLAPPED` of its own; see the [`pipe` module documentation](crate::pipe).
/// Dropping it abandons the caller's interest in the accept, not the accept.
/// It borrows the server exclusively, which is what makes two concurrent
/// accepts unrepresentable rather than merely refused.
pub struct Accept<'a> {
    state: AcceptFuture<'a>,
}

enum AcceptFuture<'a> {
    /// A client is connected already; the next poll resolves.
    Ready,
    /// Waiting on the slot's completion event.
    Waiting(&'a mut Server),
    /// Refused before anything reached the kernel.
    Failed(Error),
    /// Already resolved.
    Done,
}

impl<'a> Accept<'a> {
    fn ready() -> Self {
        Self {
            state: AcceptFuture::Ready,
        }
    }

    fn waiting(server: &'a mut Server) -> Self {
        Self {
            state: AcceptFuture::Waiting(server),
        }
    }

    /// A future that resolves to an error without ever touching the kernel.
    ///
    /// The same construction-time rejection `File::read` uses, so a refusal is
    /// expressible in a shape whose success case owns resources.
    fn failed(error: Error) -> Self {
        Self {
            state: AcceptFuture::Failed(error),
        }
    }
}

impl std::future::Future for Accept<'_> {
    type Output = crate::Result<()>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;

        let this = self.get_mut();

        match std::mem::replace(&mut this.state, AcceptFuture::Done) {
            AcceptFuture::Done => panic!("accept future polled after completion"),
            AcceptFuture::Ready => Poll::Ready(Ok(())),
            AcceptFuture::Failed(e) => Poll::Ready(Err(e)),
            AcceptFuture::Waiting(server) => {
                let signalled = match &server.accept {
                    AcceptState::Accepting(slot) => slot.event.poll_signalled(cx).is_ready(),
                    // Nothing left to wait for.
                    AcceptState::Connected => true,
                    // Unreachable while this future holds the server
                    // exclusively: nothing else can call `disconnect`, and
                    // `Drop` cannot run. Reported rather than assumed away.
                    AcceptState::Fresh | AcceptState::Idle => {
                        return Poll::Ready(Err(Error::PipeListening));
                    }
                };

                if !signalled {
                    this.state = AcceptFuture::Waiting(server);
                    return Poll::Pending;
                }

                Poll::Ready(server.collect())
            }
        }
    }
}

/// What [`Drop for Server`](Server) must do with an accept slot it is tearing
/// down.
///
/// Extracted from the drop body because the wrong choice here is a deadlock or
/// a use-after-free, and neither is observable from a test that drops a server
/// through the public API: the state that gets it wrong is currently
/// unreachable. Deciding it in a pure function makes the rule itself testable,
/// which is the only way this can be pinned at all.
#[derive(Debug, PartialEq, Eq)]
enum Teardown {
    /// The kernel has, or may have, the operation. Wait it out, then free.
    ///
    /// Waiting is bounded here because a cancel has been accepted, so the
    /// operation is on its way to a terminal state.
    CollectThenFree,
    /// The kernel demonstrably does not have this operation, and never wrote to
    /// it. Waiting would block forever on a status word nothing will ever
    /// update.
    ///
    /// The allocation is **leaked** rather than freed. Freeing would be correct
    /// on the reasoning that the kernel never took the pointer, but this branch
    /// exists precisely because that reasoning was once wrong, and the crate's
    /// standing rule is that a leak costs one allocation while the alternative
    /// corrupts unrelated memory later.
    LeakWithoutWaiting,
}

/// Chooses the teardown action from what the kernel just told us.
///
/// `cancel_found` is whether `CancelIoEx` located the operation; `completed` is
/// whether the kernel has written a terminal status into the `OVERLAPPED`.
///
/// The only case that must not wait is "the kernel could not find it **and** it
/// was never completed" — that pair means no I/O for this structure exists in
/// the kernel, so nothing will ever finish it. If either holds the other way the
/// wait terminates: a located operation reaches a terminal state after a cancel,
/// and a completed one returns immediately.
fn teardown_action(cancel_found: bool, completed: bool) -> Teardown {
    if !cancel_found && !completed {
        Teardown::LeakWithoutWaiting
    } else {
        Teardown::CollectThenFree
    }
}

impl Drop for Server {
    /// Cancels and collects any outstanding connect before the memory the kernel
    /// is writing into goes away.
    ///
    /// The order is forced and none of it is optional. `CancelIoEx` asks the
    /// kernel to stop, but asking is not the same as it having stopped, so the
    /// result is collected afterwards — that collect is what establishes the
    /// kernel is finished with the `OVERLAPPED`. Only then may the allocation be
    /// freed.
    ///
    /// The exception is the case where the kernel never had the operation at
    /// all, which must not wait — waiting there is on a status word nothing will
    /// ever write. That distinction is drawn by the private `teardown_action`
    /// rule rather than inline here, so that it can be tested directly; the
    /// state that needs it is not reachable through this type's public API.
    fn drop(&mut self) {
        let AcceptState::Accepting(slot) = &self.accept else {
            return;
        };

        // A completed slot needs no cancel; treat it as located, since the
        // reason not to cancel is that the kernel already finished with it.
        let mut cancel_found = true;
        if !slot.completed() {
            // SAFETY: the handle is open until this server's `File` reference is
            // released, which happens after this body runs. The pointer is to an
            // allocation this server still owns.
            let cancelled =
                unsafe { CancelIoEx(self.file.as_raw_handle(), Some(&*slot.overlapped)) };
            // `ERROR_NOT_FOUND` is not a failure to report, but it is not
            // nothing either: it is the one signal that distinguishes an
            // operation the kernel is finishing from one it never had.
            cancel_found = !matches!(&cancelled, Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult());
        }

        match teardown_action(cancel_found, slot.completed()) {
            Teardown::CollectThenFree => {
                let mut transferred = 0_u32;
                // SAFETY: the same structure, still owned here and not yet
                // freed. `bWait` is true here and only here, and only on this
                // branch: the operation has been cancelled or has already
                // completed, so this waits out a kernel that is finishing rather
                // than one that will never start.
                let _ = unsafe {
                    GetOverlappedResult(
                        self.file.as_raw_handle(),
                        &*slot.overlapped,
                        &mut transferred,
                        true,
                    )
                };
                // Only now is the allocation the kernel was writing into safe to
                // free. `Idle` rather than `Fresh`: a connect was submitted
                // against this instance, so the platform will not admit a client
                // without another one. Nothing can observe this on the drop
                // path, but a state that is wrong only where nobody looks is
                // still a trap for the next change.
                self.accept = AcceptState::Idle;
            }
            Teardown::LeakWithoutWaiting => {
                let stale = std::mem::replace(&mut self.accept, AcceptState::Idle);
                std::mem::forget(stale);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::{Client, unique_name as unique};
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::{Duration, Instant};

    /// Polls a future once with a waker that does nothing, reporting the result.
    ///
    /// Deliberately non-blocking. Several tests here need to establish that an
    /// accept did **not** resolve, and the alternative — awaiting it and seeing
    /// whether the test finishes — turns a wrong answer into a hang. A hang is
    /// the worst failure mode for these tests specifically, because the very
    /// defect they exist to catch (waiting for an event that will never signal)
    /// presents as one.
    fn poll_once<F: Future>(fut: &mut Pin<Box<F>>) -> Poll<F::Output> {
        let mut cx = Context::from_waker(futures::task::noop_waker_ref());
        fut.as_mut().poll(&mut cx)
    }

    /// Spins until `cond` holds, failing rather than hanging if it never does.
    ///
    /// Used only for facts the kernel establishes on its own schedule — that it
    /// has written a completion status, for instance. Never used to assert a
    /// duration: the deadline is the failure mode, not the measurement.
    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if cond() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("timed out waiting for {what}");
    }

    /// Opens the client end from a plain `std::fs::File`, for use off-thread.
    ///
    /// [`Client`] holds a [`File`], which is `!Send`, so a helper thread that
    /// has to outlive its own scope cannot carry one.
    fn open_peer(name: &str) -> std::fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(FILE_FLAG_OVERLAPPED.0)
            .open(crate::pipe::qualify(name))
            .expect("the peer should have connected")
    }

    /// SC-002: a client that arrived first is accepted **without waiting**.
    ///
    /// The single most bug-prone point in the feature. The platform reports
    /// `ERROR_PIPE_CONNECTED` here and never signals the event, so an
    /// implementation that treats the code as an error drops a live connection
    /// and one that waits for the event hangs forever.
    ///
    /// The first poll is asserted to resolve, which is what separates this from
    /// its twin below: an implementation that waited would return `Pending`
    /// here and this would fail immediately rather than hanging.
    #[test]
    fn a_client_that_connects_first_is_accepted_without_waiting() {
        let name = unique("client-first");
        let mut server = Server::create(&name).unwrap();

        let _client = Client::connect(&name).expect("the client should connect to a listening instance");
        assert_eq!(
            server.synchronous_accepts(),
            0,
            "nothing has been accepted yet"
        );

        {
            let mut fut = Box::pin(server.accept());
            let first = poll_once(&mut fut);
            assert!(
                matches!(first, Poll::Ready(Ok(()))),
                "an accept with a client already waiting must resolve on its \
                 first poll -- the completion event is never signalled in this \
                 case, so anything that waits waits forever. Got {first:?}",
            );
        }

        assert_eq!(
            server.synchronous_accepts(),
            1,
            "the synchronous path must be counted, or nothing can tell which \
             of the two accept paths a connection took"
        );
        assert_eq!(server.connect_submissions(), 1);
        assert!(server.is_connected());
    }

    /// SC-003 and the twin half of SC-002: an accept issued **before** any
    /// client waits in the kernel, and still delivers.
    ///
    /// The two halves are both load-bearing. The negative half — that the
    /// synchronous count does not move — is what stops this test silently taking
    /// the client-first path above. The positive half — that a client is
    /// actually delivered — is what stops it passing when no accept happened at
    /// all.
    #[tokio::test(flavor = "current_thread")]
    async fn a_server_that_accepts_first_waits_in_the_kernel_and_still_delivers() {
        let name = unique("server-first");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(
                poll_once(&mut fut).is_pending(),
                "with no client in existence there is nothing to resolve"
            );
        }

        assert!(
            server.accept_outstanding(),
            "the connect must be outstanding in the kernel before any client \
             exists -- if this is false the accept never reached the kernel and \
             the delivery below would prove nothing"
        );
        assert_eq!(
            server.synchronous_accepts(),
            0,
            "no client was waiting, so no accept can have completed synchronously"
        );

        let _client = Client::connect(&name).expect("the client should connect");
        server.accept().await.expect("the accept should resolve");

        assert!(
            server.is_connected(),
            "the positive half: the accept must actually deliver a client"
        );
        assert_eq!(
            server.synchronous_accepts(),
            0,
            "the accept-first path must never report a synchronous completion, \
             or this test and its client-first twin are not distinguishable"
        );
    }

    /// SC-003, the wake path: a parked accept is resolved by the event.
    ///
    /// The test above establishes that the connect reaches the kernel, but it
    /// collects the client through a fresh poll rather than through a wakeup.
    /// This one leaves the future parked and lets the thread-pool callback wake
    /// it, which is the path a real caller takes.
    #[tokio::test(flavor = "current_thread")]
    async fn a_parked_accept_is_woken_when_the_client_arrives() {
        let name = unique("wake");
        let mut server = Server::create(&name).unwrap();

        let mut fut = Box::pin(server.accept());
        assert!(
            poll_once(&mut fut).is_pending(),
            "the accept must park before the client is created, or this test \
             measures the synchronous path instead"
        );

        let peer_name = name.clone();
        let peer = std::thread::spawn(move || open_peer(&peer_name));

        tokio::time::timeout(Duration::from_secs(5), fut)
            .await
            .expect("the parked accept should have been woken by the client")
            .expect("the accept should have succeeded");

        let _peer = peer.join().expect("the peer thread should not have panicked");
        assert!(server.is_connected());
        assert_eq!(server.synchronous_accepts(), 0);
    }

    /// SC-005: an abandoned accept is **resumed**, not reissued.
    ///
    /// The submission count is the whole criterion. A reissuing implementation
    /// delivers the client just as happily, leaves the outstanding observable
    /// true just as happily and reuses the same allocation, so every other
    /// observable is identical between the two.
    #[tokio::test(flavor = "current_thread")]
    async fn an_abandoned_accept_is_resumed_not_reissued() {
        let name = unique("resume");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        assert_eq!(
            server.connect_submissions(),
            1,
            "the first accept submits exactly one connect"
        );

        {
            let mut second = Box::pin(server.accept());
            assert!(
                poll_once(&mut second).is_pending(),
                "still no client, so the resumed accept has nothing to resolve"
            );
        }
        assert_eq!(
            server.connect_submissions(),
            1,
            "a second accept over an outstanding one must resume it, not submit \
             another -- two connects against one event breaks the premise the \
             teardown reasoning rests on"
        );

        let _client = Client::connect(&name).expect("the client should connect");
        server.accept().await.expect("the resumed accept should deliver");

        assert!(server.is_connected());
        assert_eq!(
            server.connect_submissions(),
            1,
            "and still only one submission after the client was delivered"
        );
    }

    /// SC-016c: the outstanding observable reports **kernel** status, so an
    /// accept the kernel has already satisfied does not read as still pending.
    ///
    /// A flag set at submission passes every other test in this file and fails
    /// this one, which is the only reason the observable is derived rather than
    /// tracked.
    #[test]
    fn an_accept_the_kernel_satisfied_is_not_reported_as_outstanding() {
        let name = unique("unobserved");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        assert!(
            server.accept_outstanding(),
            "before the client, the connect really is pending in the kernel"
        );

        let _client = Client::connect(&name).expect("the client should connect");
        wait_until(
            || !server.accept_outstanding(),
            "the kernel to record the connect as complete",
        );

        assert!(
            !server.is_connected(),
            "nothing has observed the completion, so the server must not claim \
             a connection it has never collected"
        );
        assert_eq!(
            server.live_accept_allocations(),
            1,
            "the allocation is still held: the kernel wrote into it and nobody \
             has collected the result"
        );
    }

    /// FR-006a: the state where the kernel has connected a client nobody has
    /// seen refuses everything except another accept.
    #[tokio::test(flavor = "current_thread")]
    async fn an_unobserved_connection_refuses_use_until_a_further_accept_collects_it() {
        let name = unique("unobserved-use");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let _client = Client::connect(&name).expect("the client should connect");
        wait_until(
            || !server.accept_outstanding(),
            "the kernel to record the connect as complete",
        );

        assert!(
            matches!(server.file(), Err(Error::AcceptOutstanding)),
            "reads and writes must be refused against a peer the caller has \
             never been told about, got {:?}",
            server.file().err()
        );
        assert!(
            matches!(server.disconnect(), Err(Error::AcceptOutstanding)),
            "disconnecting would leave the kernel's connect result describing a \
             connection that no longer exists"
        );

        server
            .accept()
            .await
            .expect("a further accept is the way out of this state");
        assert!(server.is_connected());
        assert!(server.file().is_ok(), "and now the file is usable");
    }

    /// A listening instance is a distinct refusal from an accepting one.
    ///
    /// The twin to the tests above: without it, an implementation that returned
    /// [`Error::AcceptOutstanding`] unconditionally would satisfy every
    /// assertion they make.
    #[test]
    fn a_listening_instance_refuses_use_with_its_own_error() {
        let name = unique("listening");
        let mut server = Server::create(&name).unwrap();

        assert!(server.accepts_clients());
        assert!(!server.accept_outstanding());
        assert_eq!(server.connect_submissions(), 0);
        assert!(
            matches!(server.file(), Err(Error::PipeListening)),
            "a listening instance has no peer, which is not the same condition \
             as an accept being in progress, got {:?}",
            server.file().err()
        );
        assert!(matches!(server.disconnect(), Err(Error::PipeListening)));
    }

    /// A disconnected instance is **not** a listening one, and serves again
    /// only once a further accept has been submitted.
    ///
    /// This asymmetry was measured, not assumed, and it was found because an
    /// earlier version of this test connected the second client straight after
    /// the disconnect and was refused as busy. The natural reading —
    /// `DisconnectNamedPipe` returns the instance to the state it was created
    /// in — is wrong, and an implementation built on it produces a server that
    /// serves exactly one client and then silently refuses every other.
    ///
    /// The busy assertion in the middle is the load-bearing half. Without it
    /// this test passes against an implementation in which `disconnect` is a
    /// no-op, because the accept that follows would connect the client anyway.
    #[test]
    fn a_disconnected_instance_admits_clients_only_after_a_further_accept() {
        let name = unique("disconnect");
        let mut server = Server::create(&name).unwrap();

        let first = Client::connect(&name).expect("the first client should connect");
        {
            let mut fut = Box::pin(server.accept());
            assert!(matches!(poll_once(&mut fut), Poll::Ready(Ok(()))));
        }
        assert!(server.is_connected());

        drop(first);
        server
            .disconnect()
            .expect("disconnecting a connected instance");

        assert!(
            !server.accepts_clients(),
            "a disconnected instance does not admit clients until a connect is \
             submitted -- reporting otherwise is the defect this test exists for"
        );
        assert!(
            matches!(Client::connect(&name), Err(Error::PipeBusy)),
            "and the platform agrees: the instance is not listening yet"
        );

        // The connect is what reopens it.
        let mut fut = Box::pin(server.accept());
        assert!(
            poll_once(&mut fut).is_pending(),
            "no client is waiting, so this parks"
        );
        drop(fut);
        assert!(server.accepts_clients(), "and now it admits one");

        let _second = Client::connect(&name).expect("a second client should now reach this instance");
        server_resolves(&mut server);
        assert!(server.is_connected());
        assert_eq!(
            server.connect_submissions(),
            2,
            "one connect per client served"
        );
    }

    /// Drives a parked accept to completion by polling until it resolves.
    ///
    /// The signal is sticky, so a plain re-poll observes a completion raised
    /// while nothing was waiting; this is only a bounded retry, not a wait
    /// primitive.
    fn server_resolves(server: &mut Server) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut fut = Box::pin(server.accept());
            match poll_once(&mut fut) {
                Poll::Ready(r) => {
                    drop(fut);
                    r.expect("the accept should have succeeded");
                    return;
                }
                Poll::Pending => {
                    drop(fut);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        panic!("timed out waiting for the accept to resolve");
    }

    /// The accept observables report the live slot, and report nothing when
    /// there is no slot.
    ///
    /// This is not a test of the accept path; it is the twin that makes the
    /// teardown assertions built on these observables mean something. Those
    /// assert that a watch expires and a handle stops being waitable. Both hold
    /// vacuously against observables wired to the wrong slot, or to nothing —
    /// `None` and "already dead" are indistinguishable from "correctly cleaned
    /// up" after the fact. So the state to pin is the one *before* teardown:
    /// while the accept is outstanding the watch must be alive and the handle
    /// must be a real, waitable, unsignalled event.
    #[test]
    fn the_accept_observables_report_a_live_slot_while_one_exists() {
        use windows::Win32::Foundation::{HANDLE, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;

        let name = unique("observables");
        let mut server = Server::create(&name).unwrap();

        assert!(
            server.accept_event_watch().is_none(),
            "nothing is submitted yet, so there is no event to report"
        );
        assert!(server.accept_event_handle_value().is_none());
        assert_eq!(server.live_accept_allocations(), 0);

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }

        assert_eq!(server.live_accept_allocations(), 1);
        let watch = server
            .accept_event_watch()
            .expect("an outstanding accept has an event");
        assert!(
            watch.upgrade().is_some(),
            "and its shared state is alive while the accept is"
        );
        let handle = server
            .accept_event_handle_value()
            .expect("an outstanding accept has an event handle");

        // SAFETY: the handle is the accept event's, which the server still owns,
        // so it is open for the duration of this call. A zero timeout does not
        // block.
        let waited = unsafe { WaitForSingleObject(HANDLE(handle as *mut _), 0) };
        assert_eq!(
            waited, WAIT_TIMEOUT,
            "the recorded value must name a real, waitable event that has not \
             been signalled -- a stale or zero handle would fail this and would \
             make every assertion built on it vacuous"
        );

        drop(server);
        assert!(
            watch.upgrade().is_none(),
            "and the shared state goes with the server, which is the change the \
             assertion above establishes is a change"
        );
    }

    /// The teardown decision, over all four inputs.
    ///
    /// Exhaustive rather than sampled, because the space is four cases and one
    /// of them is a deadlock. Written after a mutation exposed that `Drop` waited
    /// unconditionally: an accept slot the kernel never took has a status word
    /// nothing will ever write, and `GetOverlappedResult` with `bWait` set waits
    /// on it forever. That state is unreachable through the public API today,
    /// which is exactly why nothing could catch it — the mutation reached it in
    /// one edit, and the next real change might too.
    #[test]
    fn the_teardown_decision_waits_unless_the_kernel_never_had_the_operation() {
        assert_eq!(
            teardown_action(false, false),
            Teardown::LeakWithoutWaiting,
            "not found and never completed is the only pair that means no I/O \
             exists for this structure -- waiting on it never returns"
        );

        // The other three all terminate, and each for its own reason. Asserted
        // separately so that a rule collapsing to a constant fails here.
        assert_eq!(
            teardown_action(true, false),
            Teardown::CollectThenFree,
            "a located operation reaches a terminal state after the cancel"
        );
        assert_eq!(
            teardown_action(false, true),
            Teardown::CollectThenFree,
            "not found because it had already finished; the wait returns at once"
        );
        assert_eq!(
            teardown_action(true, true),
            Teardown::CollectThenFree,
            "found and finished"
        );
    }

    /// Dropping a server with an accept outstanding completes, and the process
    /// survives to say so.
    ///
    /// The assertion *after* the drop is the point: a teardown that aborted or
    /// hung would never reach it. Strengthened by the teardown work, which owns
    /// the leak and ordering rules; this establishes only that the drop
    /// implemented here terminates.
    #[test]
    fn dropping_a_server_with_an_accept_outstanding_completes() {
        let name = unique("drop-pending");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        assert!(server.accept_outstanding());

        drop(server);

        // Reached only if the drop above returned. The name is free again,
        // which is a fact about the dropped server rather than about this
        // process still running.
        let replacement = Server::create(&name).expect("the name should be free again");
        assert!(replacement.accepts_clients());
    }

    /// An accept on a server that already has an observed client resolves
    /// without submitting anything.
    #[test]
    fn accepting_an_already_connected_instance_submits_nothing() {
        let name = unique("already");
        let mut server = Server::create(&name).unwrap();
        let _client = Client::connect(&name).expect("the client should connect");

        {
            let mut fut = Box::pin(server.accept());
            assert!(matches!(poll_once(&mut fut), Poll::Ready(Ok(()))));
        }
        assert_eq!(server.connect_submissions(), 1);
        assert_eq!(server.synchronous_accepts(), 1);

        {
            let mut again = Box::pin(server.accept());
            assert!(matches!(poll_once(&mut again), Poll::Ready(Ok(()))));
        }
        assert_eq!(
            server.connect_submissions(),
            1,
            "a redundant accept must not reach the kernel"
        );
        assert_eq!(
            server.synchronous_accepts(),
            1,
            "and must not be counted as an accept that completed synchronously, \
             since no connect was made"
        );
    }

    /// The access-direction options reach the platform.
    ///
    /// The twin is the failing half: an options set with neither direction is
    /// refused, which establishes that the mapping is read at all rather than
    /// being ignored in favour of a duplex default.
    #[test]
    fn access_direction_reaches_the_platform_and_neither_direction_is_refused() {
        let inbound = unique("inbound");
        let server = ServerOptions::new()
            .access_outbound(false)
            .create(&inbound)
            .expect("an inbound-only instance should be creatable");
        assert!(server.accepts_clients());

        let neither = unique("neither");
        let refused = ServerOptions::new()
            .access_inbound(false)
            .access_outbound(false)
            .create(&neither);
        assert!(
            refused.is_err(),
            "a pipe with no access direction names nothing the platform can \
             create, and it must not be quietly promoted to duplex"
        );
    }

    /// Two created instances serve two clients at once.
    ///
    /// The documented idiom's premise, and the answer to the reading of
    /// `max_instances` that treats it as a pool: the cap permits instances, it
    /// does not create them.
    #[test]
    fn two_created_instances_serve_two_clients() {
        let name = unique("two");
        let mut first = ServerOptions::new().max_instances(2).create(&name).unwrap();
        let mut second = ServerOptions::new().max_instances(2).create(&name).unwrap();

        let _a = Client::connect(&name).expect("the first client should connect");
        let _b = Client::connect(&name).expect("the second client should reach the second instance");

        for server in [&mut first, &mut second] {
            let mut fut = Box::pin(server.accept());
            assert!(matches!(poll_once(&mut fut), Poll::Ready(Ok(()))));
        }
        assert!(first.is_connected() && second.is_connected());
    }
}

