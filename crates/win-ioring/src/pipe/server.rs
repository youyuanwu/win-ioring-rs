//! The server end of a named pipe.
//!
//! The `OVERLAPPED` ownership rule this module implements — and the reason a
//! dropped accept resumes rather than reissues — is argued in the
//! [`pipe` module documentation](crate::pipe), which is where a caller will
//! look for it. What follows is the part that only matters from inside.
//!
//! The allocation the kernel writes into is owned by [`Server`] and reached
//! through `AcceptSlot`. Three kinds of path release it, and they are not
//! interchangeable:
//!
//! - **The kernel never took the pointer.** A synchronous `ConnectNamedPipe`
//!   failure other than `ERROR_IO_PENDING` means no IRP was queued, so the slot
//!   is simply dropped. This is sound, and it is also the reasoning that was
//!   once applied where it did not hold — see the next case.
//! - **The kernel may have taken it.** Cancel, collect, then free. The collect
//!   is what establishes the kernel is finished; it blocks and is not bounded.
//! - **The kernel demonstrably never took it, on a teardown path.** Leak it.
//!   Freeing would be defensible on the first case's reasoning, but this branch
//!   exists because that reasoning was wrong once already, and a leak costs one
//!   allocation while the alternative corrupts unrelated memory later.
//!
//! The distinction between the first and third is not a contradiction but it is
//! the sharpest edge in this file: the same argument is trusted in one place and
//! deliberately not trusted in the other, because one is a synchronous return
//! this code just observed and the other is an inference about kernel state.

use crate::error::Error;
use crate::file::File;
use crate::runtime::AbortOnUnwind;
use crate::sys::{ArmedEvent, Registration};
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

// Test seam: forces the next cancel to report a chosen outcome.
//
// `CancelIoEx` has no reliably reproducible failure mode, so neither the
// never-submitted branch nor the cancel-failed branch of the server's teardown
// can be exercised without injecting one. The same reason
// `ArmedEvent::fail_next_arm` exists, and a thread-local for the same reason:
// tests running in parallel must not consume each other's injection.
//
// It carries an outcome rather than a bool because `ERROR_NOT_FOUND` and a
// genuine failure lead to *different* leak reasons, and a seam that could only
// say "not the happy path" would leave one of them unreachable.
#[cfg(test)]
thread_local! {
    static FORCED_CANCEL: std::cell::Cell<Option<CancelOutcome>> =
        const { std::cell::Cell::new(None) };
}

// Test seam: forces the next teardown collect to observe an unfinished
// operation.
//
// `GetOverlappedResult(bWait = TRUE)` returning with the status word still
// pending is not something a test can arrange, and the branch that answers it is
// the one where being wrong frees memory the kernel is writing into. A branch
// whose only cost is invisible is exactly the shape that ships unexecuted.
#[cfg(test)]
thread_local! {
    static FORCE_TEARDOWN_UNFINISHED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Test seam: forces collects to observe an unfinished operation.
//
// The `ERROR_IO_INCOMPLETE` path -- event signalled, kernel status not yet
// terminal -- is narrow enough that no test constructs it naturally, which was
// measured: a mutation making that path report a connection survived the whole
// pipe suite. It is also the one path where the wrong answer is silent, so it
// gets a seam rather than a comment.
//
// A *latch*, not a one-shot, and that distinction was measured too. One
// `accept()` can collect twice -- `begin_accept` collects a slot the kernel has
// already satisfied, and `Accept::poll` collects again after the event reports
// ready -- so a one-shot seam is consumed by the first and the second returns
// the real, complete status. The accept then resolves and the test fails,
// intermittently, at a rate set by whether the thread-pool callback has run yet:
// 10 failures in 40 runs. A latch models the state the path actually describes,
// which is that the kernel has not finished *yet*, and every collect while that
// holds must say so.
#[cfg(test)]
thread_local! {
    static FORCE_COLLECT_INCOMPLETE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Test seam: panics partway through the teardown, to gate the unwind fence.
//
// The fence aborts the process, so the only way to observe it is from another
// process. A `#[test]` cannot assert "and this one aborted" about itself. The
// seam therefore exists so that a child process can be made to unwind out of a
// half-finished teardown, and the parent can assert the child died the way the
// fence says it should rather than the way an unfenced drop would.
#[cfg(test)]
thread_local! {
    static PANIC_IN_TEARDOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

//
// The same shape as `sys::event`'s `TEARDOWN_TRACE`, and for the same reason.
// The teardown *rule* is a pure function and can be tested directly, but nothing
// would establish that `Drop` consults it: replacing the call with a constant
// leaves every other gate green and restores the deadlock the rule exists to
// prevent. This is what makes the branch taken observable.
#[cfg(test)]
thread_local! {
    static LAST_TEARDOWN: std::cell::Cell<Option<Teardown>> = const { std::cell::Cell::new(None) };
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
    /// Creation fails with an access-denied error if the pipe already exists,
    /// which is how a server declines to be impersonated by one that got there
    /// first.
    ///
    /// It does **not** make the name exclusive. Measured: with this set on the
    /// first instance, a second instance created *without* it still succeeds.
    /// The flag protects the caller from being second; it does not stop anyone
    /// else from being third. A server that needs to be the only instance needs
    /// a security descriptor, which this API does not yet accept.
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
    /// thread-pool registration is idempotent, so the teardown path releases it
    /// early — before the blocking collect — and then lets the value drop
    /// normally, which reclaims the shared count and closes the handle in that
    /// order. Wrapping it would suppress the drop glue for its shared reference
    /// count too, leaking that.
    ///
    /// The early release is the load-bearing part and it is not an
    /// optimisation. The event is auto-reset, so one signal releases exactly one
    /// waiter; a blocking collect entered with the pool wait still armed is a
    /// second consumer of that single signal, and the configuration measured
    /// hanging. The handle is left *open* across the collect — closing it is
    /// what must wait for the collect, not the other way round.
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
    ///
    /// `Internal` is written by the kernel asynchronously, not by this thread,
    /// so the read is volatile. A plain load is one a compiler may hoist out of
    /// a loop that polls this — and polling this in a loop is exactly what the
    /// waiting paths do. Nothing currently observed depends on it, which is the
    /// argument for making it correct now rather than after something does.
    fn completed(&self) -> bool {
        // SAFETY: `overlapped` is a live, aligned, initialised `OVERLAPPED` this
        // slot owns; `Internal` is a `usize` field within it. Volatile only
        // constrains the compiler, which is the whole requirement here: the
        // kernel's write is ordered by the I/O completion itself.
        let internal = unsafe { std::ptr::read_volatile(&raw const self.overlapped.Internal) };
        internal != STATUS_PENDING_INTERNAL
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
/// ```
/// # fn main() -> win_ioring::Result<()> {
/// use win_ioring::pipe::{ClientOptions, ServerOptions};
///
/// let name = format!("win-ioring-doc-serve-{}", std::process::id());
/// let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
///
/// // Two clients arrive from another thread, so the server below is genuinely
/// // waiting for them rather than finding them already connected. A client
/// // that connects first would resolve `accept` synchronously and this example
/// // would demonstrate nothing about waiting.
/// let clients = std::thread::spawn({
///     let name = name.clone();
///     move || {
///         // `Client` is `!Send`, like everything else in this crate, so these
///         // stay on the thread that opened them and are released here.
///         let mut held = Vec::new();
///         for _ in 0..2 {
///             // Narrowing the gap is not closing it: between an accept
///             // resolving and the replacement instance existing, a client
///             // finds every instance taken. Real clients retry.
///             let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
///             loop {
///                 if let Ok(client) = ClientOptions::new().open(&name) {
///                     // Held, not dropped. A client that closes before the
///                     // server accepts leaves nothing to connect to, and the
///                     // accept reports `Error::PipeNoPeer` rather than a
///                     // connection.
///                     held.push(client);
///                     break;
///                 }
///                 assert!(std::time::Instant::now() < deadline, "no instance ever listened");
///                 std::thread::yield_now();
///             }
///         }
///         let _ = done_rx.recv();
///         drop(held);
///     }
/// });
///
/// let mut server = ServerOptions::new().max_instances(4).create(&name)?;
/// let served = tokio::runtime::Builder::new_current_thread()
///     .build()
///     .unwrap()
///     .block_on(async {
///         let mut served = 0;
///         for _ in 0..2 {
///             server.accept().await?;
///             // Create the replacement *before* giving this one away.
///             let next = ServerOptions::new().max_instances(4).create(&name)?;
///             let connected = std::mem::replace(&mut server, next);
///             served += 1;
///             drop(connected);
///         }
///         Ok::<_, win_ioring::Error>(served)
///     })?;
///
/// // Release the clients now the server has finished with them.
/// drop(done_tx);
/// clients.join().unwrap();
/// assert_eq!(served, 2);
/// # Ok(())
/// # }
/// ```
///
/// Note that the accept above resolves with no ring and no driver anywhere in
/// the example. That is not an omission: the connect is not a ring operation,
/// so nothing has to be pumping the ring for it to complete.
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
                    self.collect()
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
    ///
    /// `Ok(true)` means a client is now connected and observed. `Ok(false)`
    /// means the operation is still outstanding and the caller must keep
    /// waiting on it — it is not an error and it is not a connection.
    fn collect(&mut self) -> crate::Result<bool> {
        let AcceptState::Accepting(slot) = &self.accept else {
            // Nothing outstanding. Whatever the state is, it is not one this can
            // advance, and the caller's `Connected` check is what decides.
            return Ok(true);
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

        // Injected before the match so the injected case travels the same arm as
        // the real one. Diverting to a separate early return would test the
        // injection rather than the code.
        #[cfg(test)]
        let result = if FORCE_COLLECT_INCOMPLETE.with(|f| f.get()) {
            Err(windows::core::Error::from_hresult(
                ERROR_IO_INCOMPLETE.to_hresult(),
            ))
        } else {
            result
        };

        match result {
            Ok(()) => {
                self.accept = AcceptState::Connected;
                Ok(true)
            }
            // A connect the client satisfied out from under the wait is still a
            // connect.
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => {
                self.accept = AcceptState::Connected;
                Ok(true)
            }
            // Not finished after all: the event was signalled but the kernel has
            // not written a terminal status. Reported as *not collected* so the
            // caller re-parks on the same operation. Returning `Ok(())` here
            // would resolve the accept successfully having delivered no client —
            // the caller would see `Ok` from `accept().await` with
            // `is_connected()` false, which is the silent-wrong-answer class
            // this state machine exists to prevent.
            Err(e) if e.code() == ERROR_IO_INCOMPLETE.to_hresult() => Ok(false),
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

    /// Test seam: makes the next cancel on this thread report a chosen outcome,
    /// without issuing it.
    ///
    /// `CancelIoEx` has no reproducible failure mode, so the teardown's
    /// leak-rather-than-free branches — the most safety-critical paths here —
    /// are otherwise unreachable and would ship unexecuted. It takes the outcome
    /// rather than only forcing failure because `ERROR_NOT_FOUND` and a genuine
    /// failure lead to different leak reasons, and a seam that conflated them
    /// would leave one reason gated by nothing.
    #[cfg(test)]
    pub(crate) fn force_next_cancel(outcome: CancelOutcome) {
        FORCED_CANCEL.with(|f| f.set(Some(outcome)));
    }

    /// Test seam: makes the next *teardown* collect on this thread observe an
    /// operation the kernel has not finished with.
    ///
    /// Distinct from `force_next_collect_incomplete` below, which acts on the
    /// polling path. This one gates the branch where the blocking collect
    /// returns and the status word is still pending — the only place where
    /// trusting the platform's answer would free memory the kernel is writing
    /// into.
    #[cfg(test)]
    pub(crate) fn force_next_teardown_unfinished() {
        FORCE_TEARDOWN_UNFINISHED.with(|f| f.set(true));
    }

    /// Test seam: makes collects on this thread observe an operation the kernel
    /// has not finished writing a status for, until cleared.
    ///
    /// Gates the one path in this type where a wrong answer is silent rather
    /// than loud — an accept resolving `Ok` having delivered no client. A latch
    /// rather than a one-shot because one `accept()` can collect twice, so a
    /// one-shot leaves the second collect seeing the real status; that produced
    /// a test that failed 10 runs in 40.
    #[cfg(test)]
    pub(crate) fn force_collect_incomplete(on: bool) {
        FORCE_COLLECT_INCOMPLETE.with(|f| f.set(on));
    }

    /// Which teardown branch this thread's last `Drop for Server` took.    ///
    /// The teardown *rules* are pure functions and testable directly, but
    /// nothing would establish that `Drop` consults them: replacing a call with
    /// a constant leaves every other gate green and restores the deadlock the
    /// rules exist to prevent. Measured, not assumed — that mutation survived a
    /// nine-row harness.
    #[cfg(test)]
    pub(crate) fn last_teardown() -> Option<Teardown> {
        LAST_TEARDOWN.with(|t| t.get())
    }

    /// Test seam: makes the next teardown on this thread panic while the    /// `OVERLAPPED` is still live.
    ///
    /// Only useful in a child process — the fence turns the panic into an
    /// abort, so the calling process does not survive to assert anything. That
    /// is the point: an unfenced teardown would unwind instead, and the two are
    /// distinguishable only from outside.
    #[cfg(test)]
    pub(crate) fn panic_in_next_teardown() {
        PANIC_IN_TEARDOWN.with(|f| f.set(true));
    }

    /// The server's `File`, cloned, regardless of accept state.
    ///
    /// Distinct from the public [`Server::file`], which refuses in every state
    /// but `Connected` — a refusal that is the point of that method and would
    /// make the leak assertions unwritable. This exists so a test can hold a
    /// reference across a `mem::forget` and read the count afterwards.
    #[cfg(test)]
    pub(crate) fn file_for_test(&self) -> File {
        self.file.clone()
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

                match server.collect() {
                    Ok(true) => Poll::Ready(Ok(())),
                    // Signalled, but the kernel has not written a terminal
                    // status yet. Re-park on the same operation rather than
                    // resolve: this future's contract is that `Ok` means a
                    // client is connected, and there is no client here.
                    //
                    // The waker is re-registered because `poll_signalled`
                    // consumed the readiness that got us here; without this the
                    // task would sleep with nothing scheduled to wake it.
                    Ok(false) => {
                        if let AcceptState::Accepting(slot) = &server.accept
                            && slot.event.poll_signalled(cx).is_ready()
                        {
                            cx.waker().wake_by_ref();
                        }
                        this.state = AcceptFuture::Waiting(server);
                        Poll::Pending
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
        }
    }
}

/// What `CancelIoEx` reported about the operation.
///
/// Three outcomes rather than a boolean, because the middle one licenses a
/// *different* action from either neighbour. `NotFound` is not a failure — it is
/// the one signal that distinguishes an operation the kernel is finishing from
/// one it never had — while a genuine failure means the cancel's effect is
/// unknown, which FR-014 answers by leaking. Collapsing `NotFound` and `Failed`
/// into "did not cancel" would leak where it should collect; collapsing
/// `Failed` and `Located` into "cancel returned" would collect on a status word
/// nothing may ever write.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum CancelOutcome {
    /// The kernel found the operation and has asked it to stop. A terminal
    /// status is coming.
    Located,
    /// `ERROR_NOT_FOUND` — the kernel has no such operation for this handle.
    NotFound,
    /// Any other error. What the cancel did, if anything, is unknown.
    Failed,
}

/// What [`Drop for Server`](Server) must do with an accept slot it is tearing
/// down.
///
/// Extracted from the drop body because the wrong choice here is a deadlock or
/// a use-after-free, and neither is observable from a test that drops a server
/// through the public API: most of the states that get it wrong are reachable
/// only through an injection seam. Deciding it in pure functions makes the rules
/// themselves testable, which is the only way this can be pinned at all.
///
/// The leak variants are distinct rather than one `Leak`, because they are
/// FR-014's three separate triggers plus the never-submitted case, and a test
/// that asserts only "it leaked" cannot tell which one fired. That distinction
/// is the whole content of the requirement: an implementation that leaked for
/// the wrong reason would satisfy a single-variant criterion.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Teardown {
    /// The kernel has, or may have, the operation. Collect it, then free.
    ///
    /// The collect on this branch blocks, and **is not bounded** — see the
    /// `Drop` body for why. What this branch asserts is only that a terminal
    /// status will eventually be written, not that it will be written soon.
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
    LeakNeverSubmitted,
    /// `CancelIoEx` failed for a reason other than `ERROR_NOT_FOUND`, so what it
    /// did is unknown and the operation may still be live.
    LeakCancelFailed,
    /// The blocking `UnregisterWaitEx` failed, so the thread pool may still be a
    /// consumer of the event the collect is about to wait on.
    ///
    /// This is the trigger FR-014's rationale singles out. Proceeding to the
    /// collect here would be worse than not releasing the registration at all:
    /// releasing it early exists precisely to remove the competing consumer, and
    /// a failed release means it was not removed.
    LeakUnregisterFailed,
    /// The blocking collect returned and the kernel's status word is *still*
    /// `STATUS_PENDING`, so the operation is not finished after all.
    ///
    /// Freeing here would hand back memory the kernel is still writing into,
    /// which is the exact hazard the collect exists to rule out. That the collect
    /// returned without doing so means its answer cannot be trusted.
    LeakCollectUnfinished,
}

impl Teardown {
    /// Whether this outcome frees rather than leaks.
    fn frees(self) -> bool {
        matches!(self, Teardown::CollectThenFree)
    }
}

/// Chooses the teardown action from what the cancel just reported.
///
/// The first of three decision points, and the only one with a non-obvious rule.
/// A `Failed` cancel leaks outright: FR-014's first trigger. Otherwise the only
/// case that must not collect is "the kernel could not find it **and** it was
/// never completed" — that pair means no I/O for this structure exists in the
/// kernel, so no terminal status will ever be written and the collect would
/// block forever. If either holds the other way a terminal status is coming: a
/// located operation gets one after the cancel, and a completed one already has
/// one. Neither says *when*; see the `Drop` body.
fn teardown_action(cancel: CancelOutcome, completed: bool) -> Teardown {
    match (cancel, completed) {
        (CancelOutcome::Failed, _) => Teardown::LeakCancelFailed,
        (CancelOutcome::NotFound, false) => Teardown::LeakNeverSubmitted,
        _ => Teardown::CollectThenFree,
    }
}

/// Whether the teardown may proceed to the blocking collect after releasing the
/// thread-pool registration.
///
/// The second decision point. `Live` is unreachable here — `release_registration`
/// never returns it — but is handled rather than asserted away, because the one
/// thing that must not happen on this path is proceeding into a blocking wait on
/// an event a thread-pool callback may also be waiting on. Treating an
/// unexpected answer as permission is the failure this whole ordering exists to
/// prevent.
fn release_permits_collect(release: Registration) -> Option<Teardown> {
    match release {
        Registration::Released => None,
        Registration::Failed | Registration::Live => Some(Teardown::LeakUnregisterFailed),
    }
}

/// Whether the blocking collect actually finished the operation.
///
/// The third decision point. `GetOverlappedResult(bWait = TRUE)` is *supposed* to
/// return only once a terminal status has been written; this reads the status
/// word afterwards rather than trusting that, because the cost of being wrong is
/// freeing memory the kernel is writing into and the cost of the check is one
/// volatile load on a path that has just blocked.
fn collect_finished_it(still_pending: bool) -> Teardown {
    if still_pending {
        Teardown::LeakCollectUnfinished
    } else {
        Teardown::CollectThenFree
    }
}

/// Asks the kernel to cancel an outstanding operation, and classifies the answer.
///
/// Separate from `Drop` so that the test injection can replace *the call*, not
/// falsify its result. Falsifying the result would not model a failing cancel:
/// the real call still succeeds, the kernel still terminates the operation, and
/// the slot then reads as completed — which sends the teardown down the
/// collecting branch anyway, leaving the leak branches unreachable. Measured, by
/// writing it the other way first and watching the leak branch stay unreachable.
fn issue_cancel(handle: HANDLE, overlapped: &OVERLAPPED) -> CancelOutcome {
    #[cfg(test)]
    if let Some(injected) = FORCED_CANCEL.with(|f| f.replace(None)) {
        return injected;
    }

    // SAFETY: the caller holds the handle open for the duration of this call,
    // and the pointer is to an allocation the caller still owns.
    match unsafe { CancelIoEx(handle, Some(overlapped)) } {
        Ok(()) => CancelOutcome::Located,
        // `ERROR_NOT_FOUND` is not a failure to report, but it is not nothing
        // either: it is the one signal that distinguishes an operation the
        // kernel is finishing from one it never had.
        Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => CancelOutcome::NotFound,
        Err(_) => CancelOutcome::Failed,
    }
}

impl Drop for Server {
    /// Cancels, unregisters, and collects any outstanding connect before the
    /// memory the kernel is writing into goes away.
    ///
    /// The order is forced and none of it is optional.
    ///
    /// 1. `CancelIoEx` asks the kernel to stop. Asking is not the same as it
    ///    having stopped.
    /// 2. The thread-pool registration is released, blocking until no callback
    ///    for it is running or can start. This is before the collect, not after,
    ///    because the event is auto-reset and so releases exactly one waiter: a
    ///    collect entered with the wait still armed is a *second consumer* of a
    ///    single signal, and measurement found that configuration hanging 8
    ///    times in 200 while the same test with no armed wait hung 0 in 200. The
    ///    event handle is deliberately left open across the collect — closing it
    ///    is what must wait for the collect, not the reverse.
    /// 3. The result is collected. That collect is what establishes the kernel
    ///    is finished with the `OVERLAPPED`. Only then may any of it be freed.
    ///
    /// Every step can decline to continue, and declining always means leaking
    /// rather than freeing: a cancel that failed, a release that failed, a
    /// collect that returned with the status word still pending, or a kernel
    /// that never had the operation at all. Those four are kept as separate
    /// outcomes so a test can assert *which* one fired. The crate's standing
    /// rule applies to all of them — a leak costs one allocation, and the
    /// alternative corrupts unrelated memory later.
    ///
    /// The whole body is fenced against unwinding, as `Drop for Driver` is
    /// (`runtime/mod.rs:1469`): a panic escaping halfway would drop the
    /// `OVERLAPPED` with the kernel still holding its address.
    fn drop(&mut self) {
        let fence = AbortOnUnwind;

        let AcceptState::Accepting(slot) = &mut self.accept else {
            std::mem::forget(fence);
            return;
        };

        // The seam is read here, inside the fence and with the `OVERLAPPED`
        // still live -- which is the state the fence exists for. Reading it
        // before the fence, or after the collect, would gate a panic the fence
        // was never meant to catch.
        #[cfg(test)]
        if PANIC_IN_TEARDOWN.with(|f| f.replace(false)) {
            panic!("injected: a panic escaping a half-finished pipe teardown");
        }

        // A completed slot needs no cancel; treat it as located, since the
        // reason not to cancel is that the kernel already finished with it.
        let cancel = if slot.completed() {
            CancelOutcome::Located
        } else {
            issue_cancel(self.file.as_raw_handle(), &slot.overlapped)
        };

        let mut action = teardown_action(cancel, slot.completed());

        if action.frees() {
            // Step two. Releasing before the collect is what stops the collect
            // being a second consumer of a single auto-reset signal; a failed
            // release means it was not removed, so the collect must not run.
            if let Some(leak) = release_permits_collect(slot.event.release_registration()) {
                action = leak;
            }
        }

        if action.frees() {
            let mut transferred = 0_u32;
            // SAFETY: the same structure, still owned here and not yet freed.
            // `bWait` is true here and only here, and only on this branch: the
            // operation has either been located by the cancel or has already
            // completed, so a terminal status will be written rather than never
            // arriving. The registration has been released, so nothing else is
            // waiting on this event.
            //
            // This wait is **not bounded**, and neither is this drop. The
            // measured costs are microseconds, but `CancelIoEx` is a request and
            // not a revocation — the same property ring cancellation has — and
            // `docs/buffer-ownership.md:90-91` gives a second reason independent
            // of promptness: an operation against a dead endpoint may not
            // complete for a long time. What is promised here is the crate's
            // standing one, that shutdown never abandons memory the kernel may
            // still write to. How long that takes is the platform's to decide.
            let _ = unsafe {
                GetOverlappedResult(
                    self.file.as_raw_handle(),
                    &*slot.overlapped,
                    &mut transferred,
                    true,
                )
            };

            #[cfg(test)]
            let still_pending =
                FORCE_TEARDOWN_UNFINISHED.with(|f| f.replace(false)) || !slot.completed();
            #[cfg(not(test))]
            let still_pending = !slot.completed();

            action = collect_finished_it(still_pending);
        }

        #[cfg(test)]
        LAST_TEARDOWN.with(|t| t.set(Some(action)));

        if action.frees() {
            // Only now is the allocation the kernel was writing into safe to
            // free, and the event handle safe to close — which is what dropping
            // the slot does, the registration already being released. `Idle`
            // rather than `Fresh`: a connect was submitted against this
            // instance, so the platform will not admit a client without another
            // one. Nothing can observe this on the drop path, but a state that is
            // wrong only where nobody looks is still a trap for the next change.
            self.accept = AcceptState::Idle;
        } else {
            let stale = std::mem::replace(&mut self.accept, AcceptState::Idle);
            std::mem::forget(stale);
            // The `File` reference goes with it. FR-014 leaks the server's own
            // strong reference too, which pins the `Rc` count above zero
            // permanently, so no surviving clone can close the handle either —
            // and the kernel may still be writing through it.
            std::mem::forget(self.file.clone());
        }

        std::mem::forget(fence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::{Client, ClientOptions, unique_name as unique};
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

    /// A future the test opens by hand, standing in for a handler that awaits.
    ///
    /// SC-010 needs the server *parked inside the handoff* for an interval the
    /// test controls. A sleep would make the interval a timing assumption, and
    /// this criterion has twice shipped in a form whose hazard a race could not
    /// reach. A gate the test opens makes the duration a fact about the test.
    ///
    /// `poll_count` is what lets the test tell "parked in the handler" from
    /// "still waiting on the accept" without inspecting the server's internals.
    #[derive(Clone)]
    struct Gate(std::rc::Rc<(std::cell::Cell<bool>, std::cell::Cell<u32>)>);

    impl Gate {
        fn shut() -> Self {
            Gate(std::rc::Rc::new((
                std::cell::Cell::new(false),
                std::cell::Cell::new(0),
            )))
        }
        fn open(&self) {
            self.0.0.set(true);
        }
        fn polls(&self) -> u32 {
            self.0.1.get()
        }
    }

    impl Future for Gate {
        type Output = ();
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.0.1.set(self.0.1.get() + 1);
            if self.0.0.get() {
                Poll::Ready(())
            } else {
                // The test re-polls in a loop, so waking immediately is what
                // keeps `poll_once`'s no-op waker from stalling the task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// SC-010: the documented idiom leaves no unserved window across a handoff.
    ///
    /// **Executor structure**, which SC-010 requires stating rather than
    /// assuming: a single-threaded loop that this test steps by hand with
    /// [`poll_once`]. There is no scheduler involved, so "the window's duration
    /// is the test's to set" is a fact about this code and not a claim about
    /// anyone's runtime. Clients connect from the test body, which is the same
    /// thread — `ClientOptions::open` is synchronous, so no helper thread is
    /// needed and none of the ordering below is raced.
    ///
    /// **Correspondence with the published example.** The `served` loop below is
    /// `Server`'s rustdoc example with exactly one insertion: the awaited gate
    /// standing in for the handler's suspension point. Read the two side by
    /// side. A change to the documented idiom that is not mirrored here breaks
    /// this criterion; it is not a documentation-only edit.
    ///
    /// What is actually gated is the assertion that **client 2 connects while
    /// the server is parked**. That is what "no unserved window" means, and it
    /// is a thing "both clients were served" cannot see. With the replacement
    /// instance created after the handler returns instead of before, the name
    /// has no listening instance for the whole parked interval and that
    /// connect fails.
    #[test]
    fn two_clients_are_served_through_the_documented_idiom_with_no_unserved_window() {
        let name = unique("sc010-handoff");
        let gate = Gate::shut();
        let served = std::rc::Rc::new(std::cell::Cell::new(0_u32));

        // `nMaxInstances > 1`: at the parked moment two instances exist, and a
        // limit of one would refuse the second create rather than the client,
        // moving the failure somewhere this test is not looking.
        let mut server = ServerOptions::new()
            .max_instances(4)
            .create(&name)
            .expect("first instance");

        let mut task = Box::pin({
            let (gate, served, name) = (gate.clone(), served.clone(), name.clone());
            async move {
                for _ in 0..2 {
                    server.accept().await.expect("accept");
                    let next = ServerOptions::new()
                        .max_instances(4)
                        .create(&name)
                        .expect("replacement instance");
                    let connected = std::mem::replace(&mut server, next);
                    gate.clone().await;
                    drop(connected);
                    served.set(served.get() + 1);
                }
            }
        });

        // Park the accept before client 1 exists, so this test exercises the
        // waiting path rather than the synchronous one. A client that connects
        // first would make the accept resolve without ever waiting.
        //
        // The pending result alone does not establish that: the task parks at
        // the gate under either ordering, so `is_pending` is true either way.
        // What discriminates is that the task has not *reached* the gate, which
        // it would have done in the same poll had the accept resolved
        // synchronously. Phase 7's mutation harness caught the weaker form
        // surviving.
        assert!(
            poll_once(&mut task).is_pending(),
            "the first accept should be outstanding before any client arrives"
        );
        assert_eq!(
            gate.polls(),
            0,
            "the accept must still be outstanding: reaching the handoff on the \
             first poll means it resolved synchronously and no waiting was tested"
        );

        let _c1 = ClientOptions::new().open(&name).expect("client 1 connects");

        wait_until(
            || {
                let _ = poll_once(&mut task);
                gate.polls() >= 1
            },
            "the server to park inside the handoff",
        );

        // The gated assertion. The server is parked mid-handoff and has not
        // been polled past the gate; a second instance must already be
        // listening.
        let _c2 = ClientOptions::new()
            .open(&name)
            .expect("client 2 must connect while the handoff is parked");

        gate.open();
        wait_until(
            || poll_once(&mut task).is_ready(),
            "the server loop to serve both clients",
        );
        assert_eq!(served.get(), 2, "both clients served");
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

    /// The three teardown decisions, each over all of its inputs.
    ///
    /// Exhaustive rather than sampled: the spaces are six, three and two cases,
    /// and the wrong answer in each is either a deadlock or a use-after-free.
    /// Written after a mutation exposed that `Drop` waited unconditionally: an
    /// accept slot the kernel never took has a status word nothing will ever
    /// write, and `GetOverlappedResult` with `bWait` set waits on it forever.
    /// That state is unreachable through the public API today, which is exactly
    /// why nothing could catch it — the mutation reached it in one edit, and the
    /// next real change might too.
    #[test]
    fn the_teardown_decisions_are_exhaustive_and_each_case_is_distinct() {
        // -- after the cancel, six cases --
        assert_eq!(
            teardown_action(CancelOutcome::NotFound, false),
            Teardown::LeakNeverSubmitted,
            "not found and never completed is the only pair that means no I/O \
             exists for this structure -- waiting on it never returns"
        );
        assert_eq!(
            teardown_action(CancelOutcome::Failed, false),
            Teardown::LeakCancelFailed,
            "a failed cancel is not a not-found cancel: what it did is unknown"
        );
        assert_eq!(
            teardown_action(CancelOutcome::Failed, true),
            Teardown::LeakCancelFailed,
            "and it stays unknown even for a slot that reads as completed, \
             because the failure is about the cancel, not the status word"
        );
        assert_eq!(
            teardown_action(CancelOutcome::Located, false),
            Teardown::CollectThenFree,
            "a located operation reaches a terminal state after the cancel"
        );
        assert_eq!(
            teardown_action(CancelOutcome::NotFound, true),
            Teardown::CollectThenFree,
            "not found because it had already finished; the wait returns at once"
        );
        assert_eq!(
            teardown_action(CancelOutcome::Located, true),
            Teardown::CollectThenFree,
            "found and finished"
        );

        // -- after the release, three cases --
        assert_eq!(
            release_permits_collect(Registration::Released),
            None,
            "a released registration is the only state that permits the collect"
        );
        assert_eq!(
            release_permits_collect(Registration::Failed),
            Some(Teardown::LeakUnregisterFailed),
            "a failed release leaves the pool a possible consumer of the same \
             single auto-reset signal the collect is about to wait for"
        );
        assert_eq!(
            release_permits_collect(Registration::Live),
            Some(Teardown::LeakUnregisterFailed),
            "and an answer that cannot occur must not be read as permission -- \
             treating the unexpected as the happy path is the failure this whole \
             ordering exists to prevent"
        );

        // -- after the collect, two cases --
        assert_eq!(
            collect_finished_it(true),
            Teardown::LeakCollectUnfinished,
            "a collect that returns with the status still pending has not \
             established that the kernel is finished"
        );
        assert_eq!(
            collect_finished_it(false),
            Teardown::CollectThenFree,
            "and one that returns with a terminal status has"
        );

        // Only one variant frees. Asserted, because `frees()` is what every
        // branch in `Drop` consults, and a version returning true for a leak
        // variant would free memory the kernel may still be writing into while
        // every assertion above still passed.
        assert!(Teardown::CollectThenFree.frees());
        for leak in [
            Teardown::LeakNeverSubmitted,
            Teardown::LeakCancelFailed,
            Teardown::LeakUnregisterFailed,
            Teardown::LeakCollectUnfinished,
        ] {
            assert!(!leak.frees(), "{leak:?} must not free");
        }
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
        let watch = server.accept_event_watch().expect("an event exists");
        assert!(watch.upgrade().is_some());

        drop(server);

        // Reached only if the drop above returned, which is the survival half.
        // The discriminating half is below: the accept's shared state is gone,
        // which is a fact about what the drop *did*. Re-creating the name would
        // not be — `max_instances` defaults to unlimited, so a second instance
        // is creatable whether or not the first was dropped.
        assert!(
            watch.upgrade().is_none(),
            "the drop must release the accept's event, not merely return"
        );
        assert_eq!(
            LAST_TEARDOWN.with(|t| t.get()),
            Some(Teardown::CollectThenFree),
            "and it must reach that decision through the teardown rule, on the \
             branch that collects -- the kernel had this operation"
        );
    }

    /// A signalled-but-unfinished connect must not resolve the accept.
    ///
    /// The narrowest path in this type and the only one where the wrong answer
    /// is silent: `Ok` from `accept().await` with no client behind it. A
    /// mutation reporting this state as a connection **survived the entire pipe
    /// suite** before this test existed, which is why it has a seam rather than
    /// a comment saying the path is hard to reach.
    ///
    /// The second poll is the other half. Without it, an implementation that
    /// parked forever on this state would pass — and that is a hang, which is
    /// the failure mode this file works hardest to avoid.
    #[test]
    fn a_collect_that_finds_the_operation_unfinished_does_not_resolve_the_accept() {
        let name = unique("incomplete");
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

        Server::force_collect_incomplete(true);
        {
            let mut fut = Box::pin(server.accept());
            let polled = poll_once(&mut fut);
            assert!(
                polled.is_pending(),
                "a collect that finds no terminal status has not collected a \
                 client, and must not report one. Got {polled:?}"
            );
        }
        assert!(
            !server.is_connected(),
            "and the server must not claim a peer it has not collected"
        );

        // Cleared, so the next collect sees the real status. The latch is
        // released explicitly rather than consumed, because one `accept()` can
        // collect twice and a self-clearing seam leaves the second collect
        // seeing the truth -- which is how the assertion above became a 25%
        // flake before this was measured.
        Server::force_collect_incomplete(false);
        {
            let mut fut = Box::pin(server.accept());
            assert!(
                matches!(poll_once(&mut fut), Poll::Ready(Ok(()))),
                "the next poll must collect the connection that was there all \
                 along -- parking forever here would be a hang, not a refusal"
            );
        }
        assert!(server.is_connected());
        assert_eq!(
            server.connect_submissions(),
            1,
            "and none of this reissued the connect"
        );
    }

    /// The teardown rules are not merely correct, they are the ones `Drop` uses.
    ///
    /// Written because the exhaustive tests above gate **pure functions**, and
    /// nothing gated the calls. Replacing a call with a constant left every
    /// other gate in this file green while restoring the deadlock the rules
    /// exist to prevent, which is the "covered token, unplanned distinction"
    /// failure exactly.
    ///
    /// Each of FR-014's leak triggers gets its own arm, and each asserts *which*
    /// leak fired rather than merely that one did. A test that only asked
    /// "did it leak" would pass against an implementation that leaked for the
    /// wrong reason — and the reasons are the requirement.
    #[test]
    fn every_leak_trigger_is_reachable_and_drop_distinguishes_them() {
        // Trigger: the kernel never had the operation.
        let name = unique("leak-never");
        let mut server = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = server.accept_event_watch().expect("an event exists");
        Server::force_next_cancel(CancelOutcome::NotFound);
        drop(server);
        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::LeakNeverSubmitted),
            "a cancel that could not find an uncompleted operation must take the \
             never-submitted branch -- if this reports the collecting branch the \
             injection is inert and every test built on it proves nothing"
        );
        assert!(
            watch.upgrade().is_some(),
            "and leaking means leaking: the event's shared state is deliberately \
             not reclaimed, because the kernel may still hold the OVERLAPPED"
        );

        // Trigger: the cancel failed outright. A *different* branch from the
        // one above, and the distinction is the point -- ERROR_NOT_FOUND is
        // information, any other error is the absence of it.
        let name = unique("leak-cancel");
        let mut server = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = server.accept_event_watch().expect("an event exists");
        Server::force_next_cancel(CancelOutcome::Failed);
        drop(server);
        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::LeakCancelFailed),
            "a cancel that failed for any other reason leaves the operation's \
             fate unknown, which is not the same as knowing it never existed"
        );
        assert!(watch.upgrade().is_some());

        // Trigger: the blocking unregister failed. FR-014's rationale singles
        // this one out, because a failed release means the pool may still be a
        // consumer -- which is the entire premise of releasing before the
        // collect.
        let name = unique("leak-unreg");
        let mut server = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = server.accept_event_watch().expect("an event exists");
        ArmedEvent::fail_next_unregister();
        drop(server);
        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::LeakUnregisterFailed),
            "a failed release must stop the teardown before the collect: the \
             collect would then be a second consumer of a single auto-reset \
             signal, which is the configuration measured hanging"
        );
        assert!(watch.upgrade().is_some());

        // Trigger: the collect returned with the status still pending. The one
        // branch where trusting the platform's answer frees memory the kernel
        // is writing into.
        let name = unique("leak-collect");
        let mut server = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = server.accept_event_watch().expect("an event exists");
        Server::force_next_teardown_unfinished();
        drop(server);
        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::LeakCollectUnfinished),
            "a collect that returns without a terminal status has not \
             established what it was called to establish"
        );
        assert!(watch.upgrade().is_some());

        // Every injection is one-shot, so none can leak into another test on
        // this thread. Asserted rather than assumed: a seam that stayed armed
        // would silently divert the next drop, and three of the four above were
        // consumed by different mechanisms.
        let name = unique("leak-after");
        let mut after = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(after.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = after.accept_event_watch().expect("an event exists");
        drop(after);
        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::CollectThenFree),
            "with nothing injected the teardown must collect and free"
        );
        assert!(
            watch.upgrade().is_none(),
            "and freeing means freeing -- without this the leak assertions above \
             would pass against an implementation that leaks unconditionally"
        );
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
    /// The positive half must observe the *direction*, not merely that creation
    /// succeeded: `accepts_clients()` on a fresh server is a struct-field
    /// comparison and is true for any options at all. So the check is that an
    /// inbound-only instance refuses a client opening for read and write, which
    /// is what `Client::connect` does — a mapping that quietly promoted the
    /// request to duplex would let that client in.
    ///
    /// The twin is the failing half: an options set with neither direction is
    /// refused by the platform.
    #[test]
    fn access_direction_reaches_the_platform_and_neither_direction_is_refused() {
        let inbound = unique("inbound");
        let server = ServerOptions::new()
            .access_outbound(false)
            .create(&inbound)
            .expect("an inbound-only instance should be creatable");
        assert!(server.accepts_clients());
        let refused_client = Client::connect(&inbound);
        assert!(
            refused_client.is_err(),
            "a duplex client must not be admitted by an inbound-only instance -- \
             if it is, the direction never reached the platform and creation \
             succeeding proves nothing. Got {refused_client:?}"
        );

        // The control: the same client against a duplex instance connects, so
        // the refusal above is the access direction and not something about the
        // client or the name.
        let duplex = unique("inbound-control");
        let _control = ServerOptions::new()
            .create(&duplex)
            .expect("a duplex instance should be creatable");
        Client::connect(&duplex).expect("the same client must reach a duplex instance");

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

    /// `first_instance_only` refuses to be a second instance, and does not
    /// prevent one.
    ///
    /// Gated because its rustdoc makes a security claim, and an option carrying
    /// one while being ignored is worse than not offering it. Both halves are
    /// measured rather than assumed: the flag on a create whose name already
    /// exists fails with access-denied, and a create *without* the flag against
    /// a name whose first instance had it **succeeds**. The second half is the
    /// limitation the rustdoc must state — the flag protects the caller from
    /// being second, it does not make the name exclusive.
    #[test]
    fn first_instance_only_refuses_to_be_second_but_does_not_prevent_a_second() {
        let name = unique("first-only");
        let _first = ServerOptions::new()
            .first_instance_only(true)
            .create(&name)
            .expect("the first instance should be creatable");

        let again = ServerOptions::new().first_instance_only(true).create(&name);
        assert!(
            again.is_err(),
            "a create demanding to be first must fail once the name exists, or \
             the flag never reached the platform. Got {again:?}"
        );

        ServerOptions::new().create(&name).expect(
            "and a create *without* the flag still succeeds -- the flag does not \
             make the name exclusive, and the rustdoc says so because this does",
        );
    }

    /// The instance cap is enforced by the platform, not merely stored.
    ///
    /// `two_created_instances_serve_two_clients` passes identically whether or
    /// not `max_instances` reaches the platform, because it never exceeds the
    /// cap. This one exceeds it.
    #[test]
    fn the_instance_cap_refuses_one_instance_too_many() {
        let name = unique("cap");
        let _a = ServerOptions::new()
            .max_instances(1)
            .create(&name)
            .expect("the first instance is within the cap");
        let over = ServerOptions::new().max_instances(1).create(&name);
        assert!(
            over.is_err(),
            "a second instance under a cap of one must be refused, or the cap \
             is a field this crate stores and the platform never sees. Got {over:?}"
        );

        // The control: raising the cap admits the instance that was just
        // refused, so the refusal is the cap's doing.
        let roomy = unique("cap-control");
        let _c = ServerOptions::new().max_instances(2).create(&roomy).unwrap();
        ServerOptions::new()
            .max_instances(2)
            .create(&roomy)
            .expect("a second instance under a cap of two must be admitted");
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

    // ---- teardown -----------------------------------------------------------

    /// Whether the *server's own* event is still open at a recorded value.
    ///
    /// `GetHandleInformation` alone answers "is some handle open at this value",
    /// not "is ours". Windows reuses handle values aggressively and this suite
    /// runs in parallel, so a correct close is regularly reported as still-open
    /// by a handle another thread opened at the same value in the interval.
    /// Measured, not anticipated: the `GetHandleInformation`-only form of this
    /// helper failed 2 runs in 40.
    ///
    /// The spec's answer was to query immediately and treat a still-open report
    /// as a signal to re-run. That is not enough here, because the interfering
    /// handle comes from *another thread*, which no discipline on this one can
    /// prevent, and "re-run it" is how a real leak gets explained away.
    ///
    /// So identity is compared rather than mere presence. A duplicate of the
    /// event is taken before the teardown; afterwards, the value is ours only if
    /// something is open there *and* it names the same kernel object. Holding the
    /// duplicate keeps the object alive, which is fine — the claim under test is
    /// that the server closed *its* handle, not that the object was destroyed.
    /// An unrelated handle landing on the value now reports correctly closed,
    /// and a genuinely leaked handle still reports open.
    struct EventIdentity {
        recorded: isize,
        duplicate: HANDLE,
    }

    impl EventIdentity {
        fn of(recorded: isize) -> Self {
            use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
            use windows::Win32::System::Threading::GetCurrentProcess;
            let mut duplicate = HANDLE::default();
            // SAFETY: `recorded` names an event the server still owns, so it is
            // open for the duration of this call. The out-pointer is to a local.
            unsafe {
                let me = GetCurrentProcess();
                DuplicateHandle(
                    me,
                    HANDLE(recorded as *mut _),
                    me,
                    &mut duplicate,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            }
            .expect("the accept event must be duplicable while the server holds it");
            Self {
                recorded,
                duplicate,
            }
        }

        /// Whether the recorded value still names this event.
        fn still_open(&self) -> bool {
            use windows::Win32::Foundation::{CompareObjectHandles, GetHandleInformation};
            let mut flags = 0_u32;
            // SAFETY: both are queries, defined for any value. `GetHandleInformation`
            // returns an error for a value that is not an open handle, which is
            // the first half of the question; `CompareObjectHandles` answers the
            // second half only when the first has already said something is
            // there.
            unsafe {
                if GetHandleInformation(HANDLE(self.recorded as *mut _), &mut flags).is_err() {
                    return false;
                }
                CompareObjectHandles(HANDLE(self.recorded as *mut _), self.duplicate).as_bool()
            }
        }
    }

    impl Drop for EventIdentity {
        fn drop(&mut self) {
            // SAFETY: the duplicate is this value's own, created by
            // `DuplicateHandle` above and not closed elsewhere.
            unsafe { let _ = windows::Win32::Foundation::CloseHandle(self.duplicate); }
        }
    }

    /// Dropping an accept future releases nothing; dropping the server releases
    /// everything.
    ///
    /// Both halves matter and they are opposite claims. The first is FR-006's
    /// premise — the future does not own the operation, so abandoning it must
    /// not cancel or free anything. The second is FR-012's — the server does own
    /// it, so its drop must reclaim the allocation, the event's shared state,
    /// the event *handle*, and its own `File` reference.
    ///
    /// The handle observable is the non-obvious one. `watch()` reports the
    /// shared allocation's reference count, which is an adequate proxy for the
    /// handle only while `Drop for ArmedEvent` fuses reclaim and close — and
    /// this teardown deliberately unfuses them. Once separated, an
    /// implementation that reclaims the count and never closes the handle passes
    /// every count-based assertion and leaks one OS event handle per served
    /// client.
    #[test]
    fn dropping_the_future_releases_nothing_and_dropping_the_server_releases_all() {
        let name = unique("release-all");
        let mut server = Server::create(&name).unwrap();
        let file = server.file_for_test();
        assert_eq!(
            file.reference_count(),
            2,
            "the server's own reference plus this clone"
        );

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }

        // Dropping the future released nothing.
        assert_eq!(
            server.live_accept_allocations(),
            1,
            "abandoning the future must not free the OVERLAPPED the kernel holds"
        );
        let watch = server.accept_event_watch().expect("an event exists");
        let handle = server.accept_event_handle_value().expect("a handle exists");
        let event = EventIdentity::of(handle);
        assert!(
            event.still_open(),
            "nor close the event the kernel may still signal -- if this fails, \
             every assertion below about the close is being read after the fact"
        );

        drop(server);

        assert!(
            watch.upgrade().is_none(),
            "the server's drop reclaims the event's shared state"
        );
        assert!(
            !event.still_open(),
            "and closes the event handle, which is a separate step from the \
             reclaim above and can fail on its own"
        );
        // The `File` reference is the third thing released, and the one nothing
        // else here would notice. A server that leaked it on the success path
        // would pin the pipe handle open forever while every assertion above
        // still passed.
        assert_eq!(
            file.reference_count(),
            1,
            "and releases its own reference to the pipe handle"
        );
    }

    /// An abandoned accept is still outstanding, and a later accept resumes it.
    ///
    /// The test above proves the future's drop frees nothing. That is only half
    /// of FR-006, and it is the half that a mutation can satisfy while breaking
    /// the requirement: a `Drop` that cancelled the operation and waited for the
    /// abort to land would free no memory, close no handle, and leave every
    /// count-based assertion above intact — while destroying the connection the
    /// caller was waiting for. Mutation row Q4 does exactly that, and it
    /// survived the release test.
    ///
    /// So the claim needs its behavioural half stated separately: the operation
    /// the kernel holds must still be *live* after the future is gone, and a
    /// second `accept()` must adopt it rather than start over. The client here
    /// arrives only after the abandonment, so the connect it satisfies can only
    /// be the one begun before it.
    #[test]
    fn an_abandoned_accept_is_still_outstanding_and_a_later_accept_resumes_it() {
        let name = unique("abandon-resume");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        assert!(
            server.accept_outstanding(),
            "PREMISE: the abandoned operation is still with the kernel, so the \
             connect below has something to satisfy"
        );

        // The second future adopts the outstanding operation rather than
        // beginning a new one. If it began a new one, the allocation count would
        // rise and the first would have been leaked.
        let mut fut = Box::pin(server.accept());
        assert!(poll_once(&mut fut).is_pending());

        let _client = Client::connect(&name).expect("the client should connect");
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            match poll_once(&mut fut) {
                Poll::Ready(r) => break r,
                Poll::Pending if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                Poll::Pending => panic!(
                    "the accept begun before the future was dropped never \
                     completed, so abandoning the future lost the connection"
                ),
            }
        };
        outcome.expect("the resumed accept should succeed");
        drop(fut);
        assert_eq!(
            server.live_accept_allocations(),
            0,
            "and exactly one operation existed throughout"
        );
    }

    /// The same, for an accept the kernel has already satisfied.    ///
    /// The other case SC-006 names, and it takes a different branch: the slot
    /// reads as completed, so the teardown does not cancel at all. An
    /// implementation correct only for the pending case would pass the test
    /// above and fail here.
    #[test]
    fn dropping_a_server_whose_accept_was_already_satisfied_releases_all() {
        let name = unique("release-satisfied");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let _client = Client::connect(&name).expect("the client should connect");
        wait_until(
            || !server.accept_outstanding(),
            "the kernel to satisfy the connect",
        );

        let watch = server.accept_event_watch().expect("an event exists");
        let handle = server.accept_event_handle_value().expect("a handle exists");
        let event = EventIdentity::of(handle);
        assert!(event.still_open());

        drop(server);

        assert_eq!(
            Server::last_teardown(),
            Some(Teardown::CollectThenFree),
            "a satisfied accept has a terminal status waiting, so this collects"
        );
        assert!(watch.upgrade().is_none());
        assert!(!event.still_open());
    }

    /// The leak path leaves the handle open, which is what makes the assertion
    /// above a measurement rather than a coincidence.
    ///
    /// SC-006c requires the handle observable to be read in *both* directions.
    /// Without this arm, an implementation that never closed the handle at all
    /// would be caught, but one whose observable always reported "closed" would
    /// not.
    #[test]
    fn the_leak_path_leaves_the_event_handle_open() {
        let name = unique("leak-handle");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let handle = server.accept_event_handle_value().expect("a handle exists");
        let event = EventIdentity::of(handle);
        let file = server.file_for_test();
        assert_eq!(file.reference_count(), 2);

        Server::force_next_cancel(CancelOutcome::Failed);
        drop(server);

        assert_eq!(Server::last_teardown(), Some(Teardown::LeakCancelFailed));
        assert!(
            event.still_open(),
            "a leak that closed the event handle would still be undefined \
             behaviour: the kernel may signal it. Leaking means leaking all of it"
        );
        assert_eq!(
            file.reference_count(),
            2,
            "and the server's own `File` reference goes with it -- the kernel may \
             still write through that handle, so no surviving clone may close it"
        );
    }

    /// `mem::forget` of a live server leaks everything the kernel may touch.
    ///
    /// Not a defect but the required outcome: a forgotten server never runs
    /// `Drop`, so nothing establishes the kernel is finished, so nothing may be
    /// released. The event handle is the non-trivial half — an implementation
    /// that leaked the allocation but let the event drop would satisfy a naive
    /// reading of this and still be unsound.
    #[test]
    fn forgetting_a_server_with_an_accept_live_leaks_everything() {
        let name = unique("forget");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        let watch = server.accept_event_watch().expect("an event exists");
        let handle = server.accept_event_handle_value().expect("a handle exists");
        let event = EventIdentity::of(handle);
        let file = server.file_for_test();
        assert_eq!(file.reference_count(), 2);

        std::mem::forget(server);

        assert!(
            watch.upgrade().is_some(),
            "the event's shared state must survive a forgotten server"
        );
        assert!(
            event.still_open(),
            "and so must the handle the kernel may signal"
        );
        assert_eq!(
            file.reference_count(),
            2,
            "and the forgotten server's own reference must not fall, which is \
             what keeps the pipe handle open while the kernel may still write \
             through it"
        );
    }

    /// `disconnect` is refused while an accept is outstanding, and the instance
    /// still works afterwards.
    ///
    /// The second half is what stops this being a test of an error path that
    /// broke the server. A refusal that left the instance unusable would satisfy
    /// the first assertion alone.
    #[test]
    fn disconnect_is_refused_during_an_accept_and_the_instance_survives_it() {
        let name = unique("disc-refuse");
        let mut server = Server::create(&name).unwrap();

        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        assert!(
            matches!(server.disconnect(), Err(Error::AcceptOutstanding)),
            "disconnecting an instance the kernel is writing a connect result \
             for would leave that result describing a connection that is gone"
        );

        let _client = Client::connect(&name).expect("the instance should still be listening");
        {
            let mut fut = Box::pin(server.accept());
            assert!(matches!(poll_once(&mut fut), Poll::Ready(Ok(()))));
        }
        assert!(server.is_connected());
        assert!(
            server.disconnect().is_ok(),
            "and now that there is a client, disconnecting it is permitted"
        );
    }

    /// The unwind fence is present, established from outside the process.
    ///
    /// A `#[test]` cannot assert that it aborted. So the assertion is made by a
    /// parent: a child process is told to panic partway through a teardown, and
    /// the parent reads how it died. A fenced teardown aborts; an unfenced one
    /// unwinds and the harness reports an ordinary test failure, exit code 101.
    ///
    /// Both codes are asserted rather than only the abort, because "not 101" is
    /// satisfied by a child that failed to start.
    #[test]
    fn a_panic_in_the_teardown_aborts_rather_than_unwinding() {
        if std::env::var_os("WIN_IORING_PIPE_FENCE_CHILD").is_some() {
            // In the child. Everything below runs in the parent.
            return;
        }

        let exe = std::env::current_exe().expect("the test binary's own path");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "pipe::server::tests::the_fence_child_panics_in_a_teardown",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("WIN_IORING_PIPE_FENCE_CHILD", "1")
            .output()
            .expect("the child test binary should run");

        let code = output.status.code();
        assert_ne!(
            code,
            Some(101),
            "101 is the harness's ordinary failure code, which is what an \
             unwinding panic in a drop produces. Reaching it means the teardown \
             was not fenced.\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_ne!(
            code,
            Some(0),
            "and a child that succeeded did not reach the panic at all"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("panic escaped teardown"),
            "the abort must be the fence's, named in its own message, not some \
             other death. stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// The child half of the test above. Aborts by design; runs only when the
    /// parent asks for it by name and by environment.
    #[test]
    fn the_fence_child_panics_in_a_teardown() {
        if std::env::var_os("WIN_IORING_PIPE_FENCE_CHILD").is_none() {
            return;
        }
        let name = unique("fence-child");
        let mut server = Server::create(&name).unwrap();
        {
            let mut fut = Box::pin(server.accept());
            assert!(poll_once(&mut fut).is_pending());
        }
        Server::panic_in_next_teardown();
        drop(server);
        unreachable!("the fence should have ended this process");
    }

    /// A drop that races the client's arrival completes.
    ///
    /// This is the configuration measurement found hanging, and it is neither of
    /// the two obvious ones. The client is spawned on a helper thread and the
    /// server is dropped **without joining it**, so the connect can resolve
    /// *during* the collect rather than before or after it. With both the cancel
    /// and the early unregister removed, that window hung 8 times in 200 on this
    /// host; with either one present it hung 0 in 500.
    ///
    /// **The iteration count is the gate, not decoration.** The hang rate is
    /// host-sensitive by roughly tenfold — 8/200 here, 25/200 and 7/200 on other
    /// hosts — so a single pass would detect the mutation about 4% of the time,
    /// which is worse than no gate at all: it would report success occasionally
    /// and be believed. At the low end of the measured range, 400 passes catch it
    /// with about 86% probability, and the mutation harness runs it once.
    ///
    /// The assertion is completion, not a wall-clock bound: the harness's own
    /// timeout is the failure mode. A timing assertion would inherit the
    /// load-sensitive flakes this tree already has, and no `assert!` can catch a
    /// hang anyway.
    #[test]
    fn a_drop_racing_the_clients_arrival_completes() {
        for _ in 0..400 {
            let name = unique("race");
            let mut server = Server::create(&name).unwrap();
            {
                let mut fut = Box::pin(server.accept());
                assert!(poll_once(&mut fut).is_pending());
            }

            let peer = name.clone();
            let joiner = std::thread::spawn(move || {
                let _ = Client::connect(&peer);
            });

            // Deliberately not joined first. Joining would resolve the connect
            // before the teardown starts and this would test the already-
            // satisfied path instead -- which is covered elsewhere and does not
            // enter the window.
            drop(server);
            let _ = joiner.join();
        }
        // Reaching here is the assertion. A hang is the failure mode, and it is
        // one no `assert!` can catch.
    }
}

