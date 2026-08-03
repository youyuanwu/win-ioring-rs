mod event;
/// Crate-internal: the persistent-wait primitive the driver parks on.
///
/// Deliberately not part of the public surface. Its teardown takes the blocking
/// unregister unconditionally, which is sound only because it can never run on a
/// thread-pool callback thread — a guarantee that holds because the only thing
/// that owns one is `!Send`. See [`event::ArmedEvent`] for what making this
/// public would require.
pub(crate) use event::ArmedEvent;
pub use event::AsyncEvent;
