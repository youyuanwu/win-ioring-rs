mod event;
/// Crate-internal: the persistent-wait primitive the driver parks on.
///
/// Deliberately not part of the public surface. Its teardown takes the blocking
/// unregister unconditionally, which is sound only because it can never run on a
/// thread-pool callback thread — a guarantee that holds because the only thing
/// that owns one is `!Send`. See [`event::ArmedEvent`] for what making this
/// public would require.
pub(crate) use event::ArmedEvent;
/// Crate-internal, test-only: the state an [`ArmedEvent`] shares with its
/// thread-pool callback.
///
/// Re-exported so that a test elsewhere in the crate can hold a `Weak` to it and
/// tell a leaked registration from a reclaimed one.
#[cfg(test)]
pub(crate) use event::ArmedShared;
pub use event::AsyncEvent;
