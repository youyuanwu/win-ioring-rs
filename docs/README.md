# win-ioring documentation

Reference material for people working *on* this crate. For usage, start with the
[README](../README.md) and the rustdoc (`cargo doc --open`).

| Document | What it covers |
|---|---|
| [design.md](design.md) | Architecture, and the reasoning behind each significant decision |
| [buffer-ownership.md](buffer-ownership.md) | Why the buffer traits take ownership, what tokio-uring and compio do, and whether a borrowed slice is possible at all |
| [platform-notes.md](platform-notes.md) | How Windows IoRing actually behaves, established by probing rather than by reading |
| [testing.md](testing.md) | The verification approach, and which properties are proved by test versus by construction |
| [performance.md](performance.md) | How this crate compares to `tokio::fs` on identical work, what the measurement does and does not tell you |
| [pending-work.md](pending-work.md) | Known limitations, deferred work, and open observations |

## Reading order

If you are picking this crate up for the first time, `platform-notes.md` is the
one to read before you change anything. Most of the design falls out of a small
number of platform behaviours that are unintuitive, undocumented, or both — and
several of them contradict what a reasonable person would assume.
