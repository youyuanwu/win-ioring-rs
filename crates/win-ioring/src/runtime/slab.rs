//! Driver-owned storage for in-flight operations.
//!
//! An operation's state — including the caller's buffer — must outlive the
//! future awaiting it, because the kernel keeps a pointer into that buffer until
//! the operation completes, and a future can be dropped at any time. The slab is
//! where that state lives.
//!
//! # Why payloads are boxed
//!
//! The slab's own storage grows, which moves its elements. Each payload is
//! therefore held behind its own [`Box`], so the address handed to the kernel
//! stays put no matter how many further operations are inserted. Nothing in this
//! module ever moves a payload out of its box while the slot is occupied.
//!
//! # Tokens
//!
//! Every slot is addressed by a [`Token`], a `usize` suitable for use as the
//! platform's user data. A token encodes three things:
//!
//! - a **kind**, distinguishing an operation's completion from the completion of
//!   a cancellation request, since the platform reports both and they must not
//!   be confused;
//! - a **slot index**;
//! - a **generation**, incremented each time a slot is reused, so that a
//!   completion arriving for a long-finished operation cannot be mistaken for
//!   the operation now occupying that slot.
//!
//! # Slot state
//!
//! State is two orthogonal dimensions rather than one flat enum, because
//! "the future was dropped" and "how far the operation has progressed" vary
//! independently:
//!
//! - [`Lifecycle`] tracks progress: described, built into the submission queue,
//!   or submitted to the kernel.
//! - [`Observer`] tracks whether a future is still waiting.
//!
//! Dropping a future moves the observer dimension; a submission retry moves the
//! lifecycle dimension. Keeping them separate is what lets the driver tell a
//! detached-but-unsubmitted operation from a detached-and-submitted one, which
//! need different handling: only the latter can be cancelled.
//!
//! `SlotState::Tombstone` is a third, terminal state, entered when an
//! operation's own completion arrives while cancellation requests against it are
//! still outstanding.

use std::any::Any;

/// How far an operation has progressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// The payload is placed but nothing has been built into the submission
    /// queue. Dropping here is free: no queue entry references the buffer.
    Described,
    /// A submission queue entry exists. The entry cannot be withdrawn, so the
    /// payload must be retained even if the future goes away.
    Built,
    /// The kernel has accepted the operation.
    Submitted,
}

/// Whether a future is still waiting on an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observer {
    /// A future is waiting for this operation's result.
    Live,
    /// The future was dropped. The operation still runs to completion, but
    /// nobody wants the answer.
    Detached,
}

/// What kind of completion a token identifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// The completion of an operation.
    Operation,
    /// The completion of a cancellation request.
    Cancel,
}

/// Number of bits a token spends on the kind discriminant.
const KIND_BITS: u32 = 1;
/// Number of bits a token spends on the slot index.
const INDEX_BITS: u32 = 16;
/// Number of bits left over for the generation counter.
///
/// This is 15 on a 32-bit target and 47 on a 64-bit one. The layout is
/// deliberately identical on both so that reasoning about it does not depend on
/// the pointer width.
const GENERATION_BITS: u32 = usize::BITS - KIND_BITS - INDEX_BITS;

/// The largest number of slots the token layout can address.
pub const MAX_SLOTS: usize = 1 << INDEX_BITS;

const KIND_MASK: usize = (1 << KIND_BITS) - 1;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;
const GENERATION_MASK: usize = (1usize << GENERATION_BITS) - 1;

/// Identifies a slot, and what sort of completion to expect for it.
///
/// Tokens are handed to the platform as user data and come back on completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Token(usize);

impl Token {
    fn new(kind: TokenKind, index: usize, generation: usize) -> Self {
        debug_assert!(index < MAX_SLOTS);
        let kind_bit = match kind {
            TokenKind::Operation => 0,
            TokenKind::Cancel => 1,
        };
        Token(
            kind_bit
                | ((index & INDEX_MASK) << KIND_BITS)
                | ((generation & GENERATION_MASK) << (KIND_BITS + INDEX_BITS)),
        )
    }

    /// Returns what kind of completion this token identifies.
    pub fn kind(self) -> TokenKind {
        if self.0 & KIND_MASK == 0 {
            TokenKind::Operation
        } else {
            TokenKind::Cancel
        }
    }

    fn index(self) -> usize {
        (self.0 >> KIND_BITS) & INDEX_MASK
    }

    fn generation(self) -> usize {
        (self.0 >> (KIND_BITS + INDEX_BITS)) & GENERATION_MASK
    }

    /// Returns the same slot and generation, but identifying a cancellation
    /// request's own completion rather than the operation's.
    pub fn to_cancel(self) -> Token {
        Token::new(TokenKind::Cancel, self.index(), self.generation())
    }

    /// Returns the same slot and generation, identifying the operation.
    pub fn to_operation(self) -> Token {
        Token::new(TokenKind::Operation, self.index(), self.generation())
    }

    /// Returns the raw value to hand to the platform as user data.
    pub fn as_user_data(self) -> usize {
        self.0
    }

    /// Reconstructs a token from platform user data.
    pub fn from_user_data(value: usize) -> Self {
        Token(value)
    }
}

/// Whether a cancellation has ever been requested against an operation.
///
/// A counter cannot express this: it distinguishes "currently pending" from
/// "not pending", but not "never requested" from "already done". FR-040 makes
/// cancelling an already-cancelled operation a no-op, which needs the latter
/// distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelState {
    /// No cancellation has been requested.
    NeverRequested,
    /// A cancellation was requested and its own completion has not arrived.
    Pending,
    /// A cancellation was requested and has since completed.
    Completed,
}

/// The state of a slot.
enum SlotState {
    /// Free, awaiting reuse. Holds the next free index, forming a free list.
    Vacant { next_free: Option<usize> },
    /// Holds a live operation.
    Occupied {
        lifecycle: Lifecycle,
        observer: Observer,
        /// Boxed so its address is independent of the slab's own storage.
        payload: Box<dyn Any>,
        cancel: CancelState,
    },
    /// The operation completed while a cancellation against it was still
    /// outstanding. The payload is gone; the slot index is withheld from reuse
    /// until that cancellation reports.
    Tombstone,
    /// The slot has exhausted its generation counter and is permanently
    /// withdrawn from use, so that no stale token can ever alias a future
    /// occupant.
    Retired,
}

struct Slot {
    generation: usize,
    state: SlotState,
}

/// The outcome of looking a token up.
#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    /// The token refers to a live slot.
    Live,
    /// The token refers to a tombstoned slot: the operation is over but
    /// cancellations against it are still outstanding.
    Tombstoned,
    /// The token does not refer to anything the slab is tracking. This covers a
    /// stale token whose slot has since been reused, an index out of range, and
    /// a token for a vacant slot.
    Unknown,
}

/// Driver-owned storage for in-flight operations.
pub struct OpSlab {
    slots: Vec<Slot>,
    free_head: Option<usize>,
    occupied: usize,
}

impl OpSlab {
    /// Creates an empty slab.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_head: None,
            occupied: 0,
        }
    }

    /// Returns the number of slots holding a live operation.
    ///
    /// Tombstones are not counted: their operation has already completed.
    pub fn occupied(&self) -> usize {
        self.occupied
    }

    /// Returns the number of slots that still expect a completion of any kind,
    /// including tombstones awaiting cancellation completions.
    ///
    /// Shutdown uses this to decide whether the kernel is quiescent.
    pub fn outstanding(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| match &s.state {
                SlotState::Vacant { .. } | SlotState::Retired => false,
                SlotState::Occupied { .. } | SlotState::Tombstone => true,
            })
            .count()
    }

    /// Places a payload in the slab and returns its operation token.
    ///
    /// The slot starts in [`Lifecycle::Described`] with a [`Observer::Live`]
    /// future.
    ///
    /// # Errors
    ///
    /// Returns the payload unchanged if the slab is full, which happens when
    /// [`MAX_SLOTS`] slots are already in use.
    pub fn insert(&mut self, payload: Box<dyn Any>) -> Result<Token, Box<dyn Any>> {
        let index = match self.free_head {
            Some(index) => {
                let SlotState::Vacant { next_free } = self.slots[index].state else {
                    unreachable!("free list pointed at an occupied slot");
                };
                self.free_head = next_free;
                index
            }
            None => {
                if self.slots.len() >= MAX_SLOTS {
                    return Err(payload);
                }
                self.slots.push(Slot {
                    generation: 0,
                    state: SlotState::Vacant { next_free: None },
                });
                self.slots.len() - 1
            }
        };

        let slot = &mut self.slots[index];
        slot.state = SlotState::Occupied {
            lifecycle: Lifecycle::Described,
            observer: Observer::Live,
            payload,
            cancel: CancelState::NeverRequested,
        };
        self.occupied += 1;
        Ok(Token::new(TokenKind::Operation, index, slot.generation))
    }

    fn slot_for(&self, token: Token) -> Option<&Slot> {
        let slot = self.slots.get(token.index())?;
        if slot.generation == token.generation() {
            Some(slot)
        } else {
            None
        }
    }

    fn slot_for_mut(&mut self, token: Token) -> Option<&mut Slot> {
        let generation = token.generation();
        let slot = self.slots.get_mut(token.index())?;
        if slot.generation == generation {
            Some(slot)
        } else {
            None
        }
    }

    /// Classifies a token without modifying anything.
    pub fn lookup(&self, token: Token) -> Lookup {
        match self.slot_for(token).map(|s| &s.state) {
            Some(SlotState::Occupied { .. }) => Lookup::Live,
            Some(SlotState::Tombstone) => Lookup::Tombstoned,
            _ => Lookup::Unknown,
        }
    }

    /// Returns the payload for a live slot.
    ///
    /// The reference borrows through the box, so the payload cannot be moved
    /// while the slot is occupied.
    pub fn payload_mut(&mut self, token: Token) -> Option<&mut dyn Any> {
        match self.slot_for_mut(token)?.state {
            SlotState::Occupied {
                ref mut payload, ..
            } => Some(payload.as_mut()),
            _ => None,
        }
    }

    /// Returns the lifecycle and observer state of a live slot.
    pub fn state(&self, token: Token) -> Option<(Lifecycle, Observer)> {
        match self.slot_for(token)?.state {
            SlotState::Occupied {
                lifecycle,
                observer,
                ..
            } => Some((lifecycle, observer)),
            _ => None,
        }
    }

    /// Advances a slot's lifecycle.
    ///
    /// Returns `false` if the token does not refer to a live slot. Lifecycle
    /// only ever moves forward; an attempt to move it backwards is ignored.
    pub fn set_lifecycle(&mut self, token: Token, next: Lifecycle) -> bool {
        let Some(slot) = self.slot_for_mut(token) else {
            return false;
        };
        match slot.state {
            SlotState::Occupied {
                ref mut lifecycle, ..
            } => {
                let forward = matches!(
                    (*lifecycle, next),
                    (Lifecycle::Described, Lifecycle::Built)
                        | (Lifecycle::Described, Lifecycle::Submitted)
                        | (Lifecycle::Built, Lifecycle::Submitted)
                );
                if forward {
                    *lifecycle = next;
                }
                true
            }
            _ => false,
        }
    }

    /// Marks that the future awaiting this operation has been dropped.
    ///
    /// Returns the lifecycle the operation had reached, which determines what
    /// the caller must do next: a [`Lifecycle::Described`] operation can be
    /// released immediately, a [`Lifecycle::Built`] one must be retained, and a
    /// [`Lifecycle::Submitted`] one may be worth cancelling.
    pub fn detach(&mut self, token: Token) -> Option<Lifecycle> {
        match self.slot_for_mut(token)?.state {
            SlotState::Occupied {
                lifecycle,
                ref mut observer,
                ..
            } => {
                *observer = Observer::Detached;
                Some(lifecycle)
            }
            _ => None,
        }
    }

    /// Records that a cancellation request has been issued against a slot.
    ///
    /// Returns the token to give that cancellation request as its own user
    /// data, or `None` if there is nothing to do — because the token is
    /// unknown, is not an operation token, names an operation that has already
    /// completed, or names one that has **ever** been cancelled before.
    /// Cancelling twice is a no-op rather than an error, so exactly one
    /// cancellation is ever issued per operation.
    pub fn register_cancel(&mut self, token: Token) -> Option<Token> {
        if token.kind() != TokenKind::Operation {
            return None;
        }
        let slot = self.slot_for_mut(token)?;
        match slot.state {
            SlotState::Occupied {
                cancel: ref mut cancel @ CancelState::NeverRequested,
                ..
            } => {
                *cancel = CancelState::Pending;
                Some(token.to_cancel())
            }
            // Already cancelled once, already over, or not tracking anything.
            _ => None,
        }
    }

    /// Records that a cancellation request was never actually enqueued.
    ///
    /// Restores the slot to a state where cancellation can be requested again.
    /// This is **not** the same as [`OpSlab::complete_cancel`], which handles a
    /// cancellation that reached the platform and has now reported: here nothing
    /// was ever queued, so there is no completion to expect and nothing has been
    /// duplicated. Treating the two the same is what would otherwise make a
    /// failed request permanent, since [`OpSlab::register_cancel`] only accepts a
    /// slot that has never been requested.
    ///
    /// Returns `true` if a pending request was withdrawn.
    pub fn cancel_request_not_enqueued(&mut self, token: Token) -> bool {
        if token.kind() != TokenKind::Cancel {
            return false;
        }
        let index = token.index();
        let generation = token.generation();
        let Some(slot) = self.slots.get_mut(index) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        match slot.state {
            SlotState::Occupied {
                cancel: ref mut cancel @ CancelState::Pending,
                ..
            } => {
                *cancel = CancelState::NeverRequested;
                true
            }
            // A tombstone is waiting on a cancellation completion that will now
            // never arrive, so the slot must be released rather than withheld
            // forever.
            SlotState::Tombstone => {
                self.free_slot(index);
                true
            }
            _ => false,
        }
    }

    /// Handles an operation's own terminal completion.
    ///
    /// Returns the payload so the caller can resolve the future and release the
    /// buffer. If cancellation requests against this operation are still
    /// outstanding, the slot becomes a tombstone and its index is withheld from
    /// reuse until those cancellations report; otherwise the slot is freed
    /// immediately.
    ///
    /// Returns `None` for a token the slab is not tracking, and for a token
    /// that is not an operation token — a cancellation's completion must never
    /// release the operation's buffer. This is how a stale or unrecognised
    /// completion is safely ignored.
    pub fn complete(&mut self, token: Token) -> Option<Box<dyn Any>> {
        if token.kind() != TokenKind::Operation {
            return None;
        }
        let index = token.index();
        let generation = token.generation();
        let slot = self.slots.get_mut(index)?;
        if slot.generation != generation {
            return None;
        }
        // Check before taking. Replacing first and restoring on mismatch would
        // corrupt a tombstone's cancellation accounting.
        if !matches!(slot.state, SlotState::Occupied { .. }) {
            return None;
        }
        let SlotState::Occupied {
            payload, cancel, ..
        } = std::mem::replace(&mut slot.state, SlotState::Vacant { next_free: None })
        else {
            unreachable!("state was checked to be occupied");
        };

        self.occupied -= 1;
        if cancel == CancelState::Pending {
            slot.state = SlotState::Tombstone;
        } else {
            self.free_slot(index);
        }
        Some(payload)
    }

    /// Handles a cancellation request's own completion.
    ///
    /// Returns `true` if the completion was accounted for. When a cancellation
    /// against a tombstoned slot reports, the slot is finally freed for reuse.
    ///
    /// Returns `false` for a token that is not a cancellation token, for one
    /// the slab is not tracking, and for a slot that was not expecting a
    /// cancellation completion. Silently absorbing an unexpected completion
    /// could reclaim a slot while a real cancellation was still in flight.
    pub fn complete_cancel(&mut self, token: Token) -> bool {
        if token.kind() != TokenKind::Cancel {
            return false;
        }
        let index = token.index();
        let generation = token.generation();
        let Some(slot) = self.slots.get_mut(index) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        match slot.state {
            SlotState::Occupied {
                cancel: ref mut cancel @ CancelState::Pending,
                ..
            } => {
                // Remember that a cancellation happened, so a later request is
                // correctly refused as a no-op.
                *cancel = CancelState::Completed;
                true
            }
            SlotState::Tombstone => {
                self.free_slot(index);
                true
            }
            _ => false,
        }
    }

    /// Frees a slot and bumps its generation so outstanding tokens for it
    /// become stale.
    ///
    /// A slot whose generation counter is exhausted is **retired** instead of
    /// being recycled. Wrapping the counter would let a stale token match a
    /// future occupant and complete somebody else's operation, so the index is
    /// permanently withdrawn. That costs one slot after
    /// `2^GENERATION_BITS` reuses of it — 32768 on a 32-bit target, and around
    /// 1.4 x 10^14 on a 64-bit one.
    fn free_slot(&mut self, index: usize) {
        let free_head = self.free_head;
        let slot = &mut self.slots[index];
        if slot.generation >= GENERATION_MASK {
            slot.state = SlotState::Retired;
            return;
        }
        slot.generation += 1;
        slot.state = SlotState::Vacant {
            next_free: free_head,
        };
        self.free_head = Some(index);
    }

    /// Removes every remaining payload, returning them to the caller.
    ///
    /// Used at teardown once the kernel is known to be finished with them.
    pub fn drain(&mut self) -> Vec<Box<dyn Any>> {
        let mut out = Vec::new();
        for index in 0..self.slots.len() {
            let state = std::mem::replace(
                &mut self.slots[index].state,
                SlotState::Vacant { next_free: None },
            );
            match state {
                SlotState::Occupied { payload, .. } => {
                    self.occupied -= 1;
                    out.push(payload);
                    self.free_slot(index);
                }
                other => self.slots[index].state = other,
            }
        }
        out
    }

    /// Visits every live payload.
    #[cfg(test)]
    pub fn for_each_payload(&mut self, mut f: impl FnMut(&mut dyn Any)) {
        for slot in &mut self.slots {
            if let SlotState::Occupied {
                ref mut payload, ..
            } = slot.state
            {
                f(payload.as_mut());
            }
        }
    }

    /// Moves every operation that has been built into the submission queue on
    /// to [`Lifecycle::Submitted`].
    ///
    /// Called once the kernel has accepted the queued entries.
    pub fn promote_built_to_submitted(&mut self) {
        for slot in &mut self.slots {
            if let SlotState::Occupied {
                ref mut lifecycle, ..
            } = slot.state
                && *lifecycle == Lifecycle::Built
            {
                *lifecycle = Lifecycle::Submitted;
            }
        }
    }

    /// Returns the tokens of submitted operations whose future has gone away
    /// and which have never been cancelled.
    ///
    /// These are operations that were dropped before reaching the kernel, so
    /// there was nothing to cancel at the time. Now that they are submitted,
    /// cancelling them is worthwhile: nobody is waiting for the result.
    pub fn detached_submitted_uncancelled(&self) -> Vec<Token> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot.state {
                SlotState::Occupied {
                    lifecycle: Lifecycle::Submitted,
                    observer: Observer::Detached,
                    cancel: CancelState::NeverRequested,
                    ..
                } => Some(Token::new(TokenKind::Operation, index, slot.generation)),
                _ => None,
            })
            .collect()
    }

    /// Returns every slot for which no queue entry has been built yet.
    ///
    /// Nothing references such a slot's buffer, so teardown can resolve it
    /// directly. Believed unreachable in practice, since every insert is
    /// followed by a build or a cleanup within the same borrow — but a slot left
    /// in this state would be counted as outstanding while no completion could
    /// ever arrive for it, so the drain handles it rather than assuming.
    pub fn described(&self) -> Vec<Token> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot.state {
                SlotState::Occupied {
                    lifecycle: Lifecycle::Described,
                    ..
                } => Some(Token::new(TokenKind::Operation, index, slot.generation)),
                _ => None,
            })
            .collect()
    }

    /// Returns every submitted operation that has never had a cancellation
    /// requested, whether or not a future is still waiting on it.
    ///
    /// Teardown uses this to ask the platform to abandon everything the kernel
    /// currently holds. Unlike
    /// [`OpSlab::detached_submitted_uncancelled`], operations a caller is still
    /// awaiting are included: an immediate shutdown cancels those too.
    pub fn submitted_uncancelled(&self) -> Vec<Token> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot.state {
                SlotState::Occupied {
                    lifecycle: Lifecycle::Submitted,
                    cancel: CancelState::NeverRequested,
                    ..
                } => Some(Token::new(TokenKind::Operation, index, slot.generation)),
                _ => None,
            })
            .collect()
    }

    /// Returns every slot holding a queue entry the kernel has not taken.
    ///
    /// Such an entry cannot be withdrawn and still references the caller's
    /// buffer, so teardown must submit it rather than resolve it — and must not
    /// issue a waiting submission while any remains, since that would submit and
    /// wait in one call and could leave the slot still marked as unsubmitted.
    pub fn built(&self) -> Vec<Token> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot.state {
                SlotState::Occupied {
                    lifecycle: Lifecycle::Built,
                    ..
                } => Some(Token::new(TokenKind::Operation, index, slot.generation)),
                _ => None,
            })
            .collect()
    }

    /// Returns every slot the kernel has accepted, or whose cancellation is
    /// still outstanding.
    ///
    /// These are exactly the slots that will eventually report. Teardown may
    /// only abandon queue entries once this is empty.
    pub fn awaiting_kernel(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| {
                matches!(
                    slot.state,
                    SlotState::Occupied {
                        lifecycle: Lifecycle::Submitted,
                        ..
                    } | SlotState::Tombstone
                )
            })
            .count()
    }
}

impl Default for OpSlab {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl OpSlab {
    /// Forces a slot's generation counter, so that exhaustion can be exercised
    /// without performing 2^47 reuse cycles.
    fn force_generation(&mut self, index: usize, generation: usize) {
        self.slots[index].generation = generation & GENERATION_MASK;
    }

    /// Returns `true` if the slot has been permanently withdrawn from use.
    fn is_retired(&self, index: usize) -> bool {
        matches!(self.slots[index].state, SlotState::Retired)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(v: u32) -> Box<dyn Any> {
        Box::new(vec![v; 8])
    }

    fn payload_addr(slab: &mut OpSlab, token: Token) -> *const u8 {
        // Take the address of the payload itself, not of an allocation it
        // happens to own. A `Vec`'s heap buffer would stay put even if the
        // `Vec` value moved, so measuring that would prove nothing.
        slab.payload_mut(token).unwrap() as *const dyn Any as *const u8
    }

    /// The whole design rests on a payload's address being independent of the
    /// slab's own storage, which reallocates as it grows.
    #[test]
    fn payload_addresses_survive_slab_growth() {
        let mut slab = OpSlab::new();
        let mut tokens = Vec::new();
        let mut addrs = Vec::new();

        for i in 0..8 {
            let t = slab.insert(payload(i)).unwrap();
            addrs.push(payload_addr(&mut slab, t));
            tokens.push(t);
        }

        // Force many reallocations of the slab's own vector.
        for i in 8..2048 {
            slab.insert(payload(i)).unwrap();
        }

        for (t, expected) in tokens.iter().zip(&addrs) {
            assert_eq!(
                payload_addr(&mut slab, *t),
                *expected,
                "payload moved when the slab grew"
            );
        }
    }

    #[test]
    fn token_fields_round_trip() {
        let t = Token::new(TokenKind::Operation, 1234, 56);
        assert_eq!(t.kind(), TokenKind::Operation);
        assert_eq!(t.index(), 1234);
        assert_eq!(t.generation(), 56);

        let c = t.to_cancel();
        assert_eq!(c.kind(), TokenKind::Cancel);
        assert_eq!(c.index(), 1234);
        assert_eq!(c.generation(), 56);
        assert_eq!(c.to_operation(), t);

        assert_eq!(Token::from_user_data(t.as_user_data()), t);
    }

    #[test]
    fn max_index_and_generation_do_not_collide() {
        let max_index = MAX_SLOTS - 1;
        let max_generation = GENERATION_MASK;
        let t = Token::new(TokenKind::Cancel, max_index, max_generation);
        assert_eq!(t.kind(), TokenKind::Cancel);
        assert_eq!(t.index(), max_index);
        assert_eq!(t.generation(), max_generation);
    }

    /// A completion for a long-finished operation must never be attributed to
    /// whatever now occupies that slot.
    #[test]
    fn recycled_slots_reject_stale_tokens() {
        let mut slab = OpSlab::new();
        let first = slab.insert(payload(1)).unwrap();
        assert_eq!(slab.lookup(first), Lookup::Live);

        slab.complete(first).unwrap();
        assert_eq!(slab.lookup(first), Lookup::Unknown);

        let second = slab.insert(payload(2)).unwrap();
        // The slot index is reused, but the generation differs.
        assert_eq!(first.index(), second.index());
        assert_ne!(first.generation(), second.generation());

        assert_eq!(slab.lookup(first), Lookup::Unknown);
        assert!(slab.complete(first).is_none());
        // The live operation is untouched by the stale completion.
        assert_eq!(slab.lookup(second), Lookup::Live);
    }

    #[test]
    fn unknown_tokens_are_ignored() {
        let mut slab = OpSlab::new();
        let bogus = Token::new(TokenKind::Operation, 9999, 3);
        assert_eq!(slab.lookup(bogus), Lookup::Unknown);
        assert!(slab.complete(bogus).is_none());
        assert!(!slab.complete_cancel(bogus.to_cancel()));
        assert!(slab.detach(bogus).is_none());
    }

    #[test]
    fn lifecycle_only_moves_forward() {
        let mut slab = OpSlab::new();
        let t = slab.insert(payload(1)).unwrap();
        assert_eq!(slab.state(t).unwrap().0, Lifecycle::Described);

        assert!(slab.set_lifecycle(t, Lifecycle::Built));
        assert_eq!(slab.state(t).unwrap().0, Lifecycle::Built);

        // Backwards is ignored rather than panicking.
        assert!(slab.set_lifecycle(t, Lifecycle::Described));
        assert_eq!(slab.state(t).unwrap().0, Lifecycle::Built);

        assert!(slab.set_lifecycle(t, Lifecycle::Submitted));
        assert_eq!(slab.state(t).unwrap().0, Lifecycle::Submitted);
    }

    /// Dropping a future moves the observer dimension only, leaving the
    /// lifecycle intact. This is what lets the driver tell a detached-built
    /// operation from a detached-submitted one.
    #[test]
    fn detach_reports_lifecycle_and_leaves_it_alone() {
        let mut slab = OpSlab::new();

        let described = slab.insert(payload(1)).unwrap();
        assert_eq!(slab.detach(described), Some(Lifecycle::Described));
        assert_eq!(
            slab.state(described).unwrap(),
            (Lifecycle::Described, Observer::Detached)
        );

        let built = slab.insert(payload(2)).unwrap();
        slab.set_lifecycle(built, Lifecycle::Built);
        assert_eq!(slab.detach(built), Some(Lifecycle::Built));
        assert_eq!(
            slab.state(built).unwrap(),
            (Lifecycle::Built, Observer::Detached)
        );

        // A detached-built slot can still be promoted by a retry, at which
        // point it becomes cancellable.
        assert!(slab.set_lifecycle(built, Lifecycle::Submitted));
        assert_eq!(
            slab.state(built).unwrap(),
            (Lifecycle::Submitted, Observer::Detached)
        );
    }

    /// The operation's own completion releases the buffer; the cancellation's
    /// completion must not, and must not free the slot early either.
    #[test]
    fn tombstone_withholds_the_slot_until_cancels_report() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(7)).unwrap();
        let cancel = slab.register_cancel(op).unwrap();
        assert_eq!(cancel.kind(), TokenKind::Cancel);

        // The operation finishes first. Its payload comes back, but the slot
        // must not be reusable while the cancellation is still outstanding.
        let recovered = slab.complete(op).unwrap();
        assert_eq!(recovered.downcast_ref::<Vec<u32>>().unwrap()[0], 7);
        assert_eq!(slab.lookup(op), Lookup::Tombstoned);
        assert_eq!(slab.outstanding(), 1);

        let next = slab.insert(payload(8)).unwrap();
        assert_ne!(
            next.index(),
            op.index(),
            "a tombstoned slot index was reused too early"
        );

        // The cancellation reports last, releasing the slot.
        assert!(slab.complete_cancel(cancel));
        assert_eq!(slab.lookup(op), Lookup::Unknown);
    }

    #[test]
    fn cancel_completing_before_its_target_is_accounted_for() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(3)).unwrap();
        let cancel = slab.register_cancel(op).unwrap();

        // The cancellation reports first; the operation is still live.
        assert!(slab.complete_cancel(cancel));
        assert_eq!(slab.lookup(op), Lookup::Live);

        // The operation's own completion then frees everything.
        assert!(slab.complete(op).is_some());
        assert_eq!(slab.lookup(op), Lookup::Unknown);
        assert_eq!(slab.outstanding(), 0);
    }

    /// FR-040: cancelling an operation that has ever been cancelled is a no-op.
    /// A counter could not express this, because it cannot tell "never
    /// requested" from "already done".
    #[test]
    fn repeated_cancellation_is_a_no_op() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(1)).unwrap();
        let first = slab.register_cancel(op).unwrap();
        assert!(
            slab.register_cancel(op).is_none(),
            "a second cancellation must not be issued while the first is pending"
        );

        // The crucial case: once the first cancellation has *completed*, a
        // further request must still be refused.
        assert!(slab.complete_cancel(first));
        assert!(
            slab.register_cancel(op).is_none(),
            "a cancellation must not be re-issued after the first one completed"
        );

        // With no cancellation outstanding, the operation's completion frees
        // the slot directly rather than leaving a tombstone.
        assert!(slab.complete(op).is_some());
        assert_eq!(slab.lookup(op), Lookup::Unknown);
        assert_eq!(slab.outstanding(), 0);
    }

    /// A duplicate completion for an operation that has already completed must
    /// not disturb the tombstone left behind for its outstanding cancellations.
    #[test]
    fn double_completion_does_not_corrupt_a_tombstone() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(5)).unwrap();
        let cancel = slab.register_cancel(op).unwrap();

        assert!(slab.complete(op).is_some());
        assert_eq!(slab.lookup(op), Lookup::Tombstoned);

        // A repeat completion for the same token yields nothing and leaves the
        // tombstone's accounting intact.
        assert!(slab.complete(op).is_none());
        assert_eq!(slab.lookup(op), Lookup::Tombstoned);
        assert_eq!(slab.outstanding(), 1);

        // The outstanding cancellation still resolves the slot correctly.
        assert!(slab.complete_cancel(cancel));
        assert_eq!(slab.lookup(op), Lookup::Unknown);
        assert_eq!(slab.outstanding(), 0);

        // And the freed index is genuinely reusable afterwards.
        let reused = slab.insert(payload(6)).unwrap();
        assert_eq!(reused.index(), op.index());
    }

    /// The kind bit exists precisely so a cancellation's completion can never be
    /// mistaken for its target's. Mixing them up would release the caller's
    /// buffer while the kernel still owned it.
    #[test]
    fn completions_reject_the_wrong_token_kind() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(1)).unwrap();
        let cancel = slab.register_cancel(op).unwrap();

        // A cancellation's completion must not release the operation's payload.
        assert!(slab.complete(cancel).is_none());
        assert_eq!(slab.lookup(op), Lookup::Live);

        // An operation's completion must not be counted as a cancellation.
        assert!(!slab.complete_cancel(op));

        // The correctly-kinded calls still work.
        assert!(slab.complete(op).is_some());
        assert!(slab.complete_cancel(cancel));
        assert_eq!(slab.outstanding(), 0);
    }

    /// An unexpected cancellation completion must not be absorbed, or it could
    /// reclaim a slot while a genuine cancellation was still in flight.
    #[test]
    fn cancel_completion_without_a_pending_cancel_is_rejected() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(1)).unwrap();
        let spurious = op.to_cancel();

        assert!(
            !slab.complete_cancel(spurious),
            "a cancel completion nobody asked for must not be accounted"
        );
        assert_eq!(slab.lookup(op), Lookup::Live);

        // One real cancel, then two completions: only the first is accepted.
        let cancel = slab.register_cancel(op).unwrap();
        assert!(slab.complete_cancel(cancel));
        assert!(!slab.complete_cancel(cancel));
    }

    /// Cancelling an operation that has already completed is a no-op, not an
    /// error, and must not withhold the slot index forever.
    #[test]
    fn cancelling_a_finished_operation_is_a_no_op() {
        let mut slab = OpSlab::new();
        let op = slab.insert(payload(1)).unwrap();
        let cancel = slab.register_cancel(op).unwrap();
        slab.complete(op).unwrap();
        assert_eq!(slab.lookup(op), Lookup::Tombstoned);

        // Registering another cancel against the tombstone must be refused.
        assert!(slab.register_cancel(op).is_none());

        // So the single outstanding cancel still releases the slot.
        assert!(slab.complete_cancel(cancel));
        assert_eq!(slab.lookup(op), Lookup::Unknown);
        assert_eq!(slab.insert(payload(2)).unwrap().index(), op.index());
    }

    /// Wrapping the generation counter would let a stale token match a future
    /// occupant, so an exhausted slot is retired instead.
    #[test]
    fn exhausted_generations_retire_the_slot_instead_of_wrapping() {
        let mut slab = OpSlab::new();
        let first = slab.insert(payload(1)).unwrap();
        assert_eq!(first.index(), 0);

        // Jump the counter to its last usable value.
        slab.force_generation(0, GENERATION_MASK);
        let stale = Token::new(TokenKind::Operation, 0, GENERATION_MASK);

        slab.complete(stale).unwrap();
        assert!(
            slab.is_retired(0),
            "an exhausted slot must be withdrawn, not recycled"
        );

        // The next insert must not reuse the retired index, so the stale token
        // cannot alias the new occupant.
        let next = slab.insert(payload(2)).unwrap();
        assert_ne!(next.index(), 0);
        assert!(slab.complete(stale).is_none());
        assert_eq!(slab.lookup(next), Lookup::Live);
    }

    #[test]
    fn generation_wraps_without_panicking() {
        let mut slab = OpSlab::new();
        // Reuse a single slot enough times to exercise the counter.
        let mut last = slab.insert(payload(0)).unwrap();
        for _ in 0..1000 {
            slab.complete(last).unwrap();
            last = slab.insert(payload(0)).unwrap();
            assert_eq!(last.index(), 0, "expected the free list to reuse slot 0");
        }
        assert_eq!(slab.lookup(last), Lookup::Live);
    }

    #[test]
    fn occupied_and_outstanding_track_separately() {
        let mut slab = OpSlab::new();
        assert_eq!(slab.occupied(), 0);
        assert_eq!(slab.outstanding(), 0);

        let a = slab.insert(payload(1)).unwrap();
        let b = slab.insert(payload(2)).unwrap();
        assert_eq!(slab.occupied(), 2);
        assert_eq!(slab.outstanding(), 2);

        let cancel = slab.register_cancel(a).unwrap();
        slab.complete(a).unwrap();
        // `a` is tombstoned: no longer occupied, but still outstanding.
        assert_eq!(slab.occupied(), 1);
        assert_eq!(slab.outstanding(), 2);

        slab.complete_cancel(cancel);
        assert_eq!(slab.outstanding(), 1);

        slab.complete(b).unwrap();
        assert_eq!(slab.occupied(), 0);
        assert_eq!(slab.outstanding(), 0);
    }

    #[test]
    fn drain_returns_every_payload() {
        let mut slab = OpSlab::new();
        for i in 0..5 {
            slab.insert(payload(i)).unwrap();
        }
        let drained = slab.drain();
        assert_eq!(drained.len(), 5);
        assert_eq!(slab.occupied(), 0);
        assert_eq!(slab.outstanding(), 0);
    }
}
