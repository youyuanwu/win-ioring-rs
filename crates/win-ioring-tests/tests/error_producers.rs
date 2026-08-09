//! FR-18's guard: every public error type is *returned by a public method*.
//!
//! # Why this file exists
//!
//! This work's own record contains four instances of one failure, and this is
//! the guard against the fourth. In each case the deliverable that went missing
//! was the one so obviously the point of the work that nobody wrote a criterion
//! for it:
//!
//! - The named-pipes feature's SC-001 — the headline criterion, assigned, cited,
//!   reported covered, and never implemented.
//! - This work's own FR-18 — six error types were specified, and the pipe surface
//!   had no I/O methods for a `pipe::Error` to be returned *from*. The whole
//!   design would have compiled, passed review, and done nothing.
//! - The migration itself, absent from the implementation plan that the change
//!   was made of.
//! - Twenty-six `Display` messages silently reworded inside a diff that read as
//!   "fifty new variants".
//!
//! The rule the first three share, and the one this file enforces, is: **not
//! "the type exists" but "a public method returns it."** A type with no producer
//! is indistinguishable from a type nobody reaches, and no amount of testing the
//! type's own behaviour tells the two apart.
//!
//! # Why it is a compile-time binding and not a scan
//!
//! Each check below calls a real public method and binds its error to an
//! explicitly annotated local. Deleting the method, or rewiring it to report a
//! different type, is a compile error here.
//!
//! It is deliberately not a test that reads the source looking for `-> ...Error`
//! in a signature. This work retired one text-scanning guard after nine defeats
//! across three review rounds: a guard that must anticipate every syntactic form
//! of a construct is playing an unbounded game. A guard that names the construct
//! in the language is playing a bounded one.
//!
//! These are `#[allow(unused)]` bindings inside functions that are never called.
//! That is intentional — the check is that this file *compiles*, and running the
//! I/O would add flakiness without adding coverage. The one exception is the
//! test at the bottom, which exists so the file is not silently excluded from
//! the build.

#![allow(dead_code, unused_variables)]

use win_ioring::runtime::Handle;

/// `buf::Error` is the one type with no *direct* public producer, by design.
///
/// It is a component rather than a surface: pure arithmetic over alignment and
/// capacity, with no `Other` and no `HRESULT` provenance, and the spec treats it
/// as an exception throughout (FR-3, SC-17). What it has instead is a *nested*
/// producer -- a surface error carries it -- and that is what is pinned here.
///
/// Stated rather than glossed. A guard that quietly counted this as equivalent
/// to the other five would be overstating its own coverage, which is the failure
/// this repository has already been bitten by twice.
fn buf_error_is_reachable_inside_a_surface_error(e: win_ioring::file::Error) {
    let inner: win_ioring::buf::Error = match e {
        win_ioring::file::Error::Buf(b) => b,
        _ => return,
    };
}

/// `io_ring::Error` — reached through building a ring.
fn io_ring_error_has_a_producer() {
    // Deliberately not `builder().build()`: that returns `BuildError`, a
    // different type. This guard caught that on its first compile, which is the
    // point -- "the type exists" and "a public method returns it" are different
    // claims, and only the compiler can tell them apart.
    let ring = match win_ioring::io_ring::IoRing::builder().build() {
        Ok(r) => r,
        Err(_) => return,
    };
    let e: win_ioring::io_ring::Error = match ring.info() {
        Ok(_) => return,
        Err(e) => e,
    };
}

/// `runtime::Error` — reached through the driver's own surface.
async fn runtime_error_has_a_producer(handle: &Handle, file: &win_ioring::file::File) {
    let e: win_ioring::runtime::Error = match handle.flush(file).await {
        Ok(()) => return,
        Err(e) => e,
    };
}

/// `file::Error` — reached through the file surface's positional read.
///
/// The binding is on the *error channel of the future's output*, which is what
/// FR-18 is actually about: it is not enough that `file::Error` is nameable, a
/// file operation has to be able to produce one.
async fn file_error_has_a_producer(handle: &Handle, file: &win_ioring::file::File) {
    let (result, _buffer) = file.read_at(handle, vec![0_u8; 8], 8, 0).await.into_parts();
    let e: win_ioring::file::Error = match result {
        Ok(_) => return,
        Err(e) => e,
    };
}

/// `file::Error` again, through the flush wrapper added with it.
async fn file_flush_has_a_producer(handle: &Handle, file: &win_ioring::file::File) {
    let e: win_ioring::file::Error = match file.flush(handle).await {
        Ok(()) => return,
        Err(e) => e,
    };
}

/// `pipe::Error` — through the client's own write, not through the `File` it
/// holds.
///
/// This is the check that would have failed before Phase 5. `Client` had no
/// write of its own; a caller reached a pipe's writes through its `File` and got
/// a `file::Error` back from a pipe operation.
async fn pipe_error_has_a_producer_on_the_client(
    handle: &Handle,
    client: &win_ioring::pipe::Client,
) {
    let (result, _buffer) = client
        .write_at(handle, vec![0_u8; 8], 8, 0)
        .await
        .into_parts();
    let e: win_ioring::pipe::Error = match result {
        Ok(_) => return,
        Err(e) => e,
    };
}

/// `pipe::Error` — through the server's own read, and through `accept`.
async fn pipe_error_has_a_producer_on_the_server(
    handle: &Handle,
    server: &mut win_ioring::pipe::Server,
) {
    let (result, _buffer) = server
        .read_at(handle, vec![0_u8; 8], 8, 0)
        .await
        .into_parts();
    let e: win_ioring::pipe::Error = match result {
        Ok(_) => return,
        Err(e) => e,
    };
    let e: win_ioring::pipe::Error = match server.accept().await {
        Ok(()) => return,
        Err(e) => e,
    };
}

/// Keeps this file in the test run.
///
/// Without a `#[test]` the harness reports zero tests and the file's failure to
/// compile would still be caught — but a reader scanning output for evidence
/// that the guard ran would find none. This work has twice been bitten by a
/// gate that reported `ok` while checking nothing; an empty binary is the same
/// hazard wearing different clothes.
#[test]
fn every_public_error_type_is_returned_by_a_public_method() {
    // The assertions are the type annotations above, checked by the compiler.
    // If this file compiles, all six types have a public producer.
}
