//! Voice pool — a fixed-capacity, ID-keyed allocator for `Voice`s.
//!
//! Every active note (live or NNA-detached) lives in this pool.
//! Tracks reference voices through `VoiceId` handles instead of
//! owning them directly. The decoupling lets voice stealing happen
//! against the global voice population, the way schismtracker does
//! it (`csf_get_nna_channel`, `effects.c:1620`).
//!
//! The pool itself is an array of generational slots. `VoiceId`
//! encodes `(slot_index, generation)`, so a stale handle (whose
//! voice has since been released and the slot reused) is detected
//! via the generation mismatch and silently fails on
//! `get`/`get_mut`. Standard generational-arena pattern.
//!
//! Allocation is split into two methods reflecting two different
//! callers:
//!
//! - [`crate::voice_pool::VoicePool::allocate_live`]: must succeed
//!   (the pattern's note has to play). Falls back to evicting any
//!   voice — including a sustained one — when no evictable victim
//!   exists.
//! - [`crate::voice_pool::VoicePool::allocate_ghost`]: may refuse (the NNA spawn is
//!   optional). Returns `None` when every voice is sustained-and-
//!   audible, mirroring schism's `csf_get_nna_channel` returning 0.

use crate::voice::Voice;
use alloc::vec::Vec;
use core::num::NonZeroU32;

/// Opaque handle to a voice inside a `VoicePool`.
///
/// The `NonZeroU32` lets the compiler use the niche so
/// `size_of::<Option<VoiceId>>() == size_of::<VoiceId>()`. The encoded
/// value combines the slot index (low 16 bits) and the slot's
/// generation counter (high 16 bits). A handle is invalidated when
/// its slot is released — subsequent `get`/`get_mut` return `None`,
/// even if the slot has since been reused.
///
/// Handles are cheap to copy and can be safely embedded in tracks or
/// past-note effect queues without ownership concerns. They are not
/// portable across pools: a `VoiceId` minted by pool A will not
/// resolve in pool B even if the same slot index happens to be live
/// there.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct VoiceId(NonZeroU32);

impl VoiceId {
    /// Maximum slot index a single pool can address. Beyond this the
    /// generation counter would collide with the slot bits in the
    /// packed `NonZeroU32`. 65535 is well above the schism reference
    /// (256 voices) and any realistic xmrsplayer cap.
    pub const MAX_SLOTS: usize = 0xFFFF;

    #[inline(always)]
    fn new(slot: usize, generation: u32) -> Self {
        debug_assert!(slot < Self::MAX_SLOTS, "slot index overflow");
        // generation always >= 1; we initialise slots at gen=1 and
        // increment on every release. Combined with `slot+1` in the
        // low bits, the packed value is guaranteed non-zero.
        let packed = ((generation & 0xFFFF) << 16) | ((slot as u32 + 1) & 0xFFFF);
        // Safety: `slot + 1` is at least 1 (since slot < MAX_SLOTS),
        // so `packed` has at least bit 0 set and is non-zero.
        Self(NonZeroU32::new(packed).expect("encoded VoiceId is non-zero by construction"))
    }

    #[inline(always)]
    fn slot(self) -> usize {
        ((self.0.get() & 0xFFFF) - 1) as usize
    }

    #[inline(always)]
    fn generation(self) -> u32 {
        (self.0.get() >> 16) & 0xFFFF
    }
}

/// State of a single slot in the pool.
//
// `Voice` is intentionally large (the full instrument-state engine
// lives inline) and `Occupied` therefore dominates the variant
// size. Boxing it the way clippy suggests would push the voice
// onto the heap, costing one allocation per `allocate_live` /
// `allocate_ghost` and a pointer indirection on every per-sample
// mix touch — exactly the wrong tradeoff for an audio hot path.
// Vacant slots already cost the full enum size today, but those
// are bounded by `MAX_VOICES` (256 by default) so the worst-case
// dead memory is a few tens of KiB and never grows past pool
// capacity.
#[allow(clippy::large_enum_variant)]
enum SlotState<'a> {
    /// Slot is free. `next_free` is the index of the next free slot,
    /// or `usize::MAX` to mark the end of the chain.
    Vacant { next_free: usize },
    /// Slot is occupied by a live voice.
    Occupied(Voice<'a>),
}

struct Slot<'a> {
    /// Bumped each time the slot transitions Occupied -> Vacant. A
    /// `VoiceId` is valid only while its `generation()` matches the
    /// slot's current value.
    generation: u32,
    state: SlotState<'a>,
}

/// Fixed-capacity pool of `Voice`s, accessed via `VoiceId` handles.
///
/// The capacity is set at construction and never changes — relocating
/// the slot array would force every outstanding handle to be
/// rewritten, which we avoid by never reallocating.
pub(crate) struct VoicePool<'a> {
    slots: Vec<Slot<'a>>,
    /// Index of the first free slot, or `usize::MAX` when the pool is
    /// full. Free slots form a singly-linked list through their
    /// `next_free` field, in LIFO order (most-recently-released first).
    /// LIFO keeps the working set hot in cache and is conventional
    /// for arena allocators.
    free_head: usize,
    /// Number of slots currently `Occupied`. Maintained alongside the
    /// free-list so `allocated_count` is O(1).
    allocated: usize,
}

const NIL: usize = usize::MAX;

impl<'a> VoicePool<'a> {
    /// Build a pool with room for `capacity` simultaneous voices. The
    /// pool starts empty; every slot is on the free-list.
    ///
    /// Panics if `capacity == 0` or `capacity >= VoiceId::MAX_SLOTS`.
    /// Both bounds reflect intentional limits: a zero-capacity pool
    /// can never allocate, so it would only mask logic errors; a
    /// capacity above MAX_SLOTS would overflow the packed `VoiceId`.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "VoicePool capacity must be positive");
        assert!(
            capacity < VoiceId::MAX_SLOTS,
            "VoicePool capacity {} exceeds VoiceId::MAX_SLOTS ({})",
            capacity,
            VoiceId::MAX_SLOTS
        );
        let mut slots = Vec::with_capacity(capacity);
        for i in 0..capacity {
            // Each slot points to the next; the last points to NIL.
            let next_free = if i + 1 < capacity { i + 1 } else { NIL };
            slots.push(Slot {
                generation: 1,
                state: SlotState::Vacant { next_free },
            });
        }
        Self {
            slots,
            free_head: 0,
            allocated: 0,
        }
    }

    /// Total number of slots in the pool (live + free). Test-only:
    /// no production consumer reads the capacity today.
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Number of slots currently occupied.
    pub fn allocated_count(&self) -> usize {
        self.allocated
    }

    /// `true` when every slot is occupied. Consulted by the NNA
    /// trigger path (`Channel::spawn_ghost_for_outgoing`) to decide
    /// whether the outgoing voice can be backgrounded without forcing
    /// an eviction — mirroring schism's `csf_get_nna_channel`, which
    /// returns 0 (refuse to background) when no NNA channel is free.
    pub fn is_full(&self) -> bool {
        self.free_head == NIL
    }

    /// Allocate a voice that *must* succeed.
    ///
    /// Used for the live voice driving a track's pattern column —
    /// the user heard a note typed in the pattern, the player has
    /// to play it. When the pool is full, evicts the
    /// least-defensible voice to make room. The returned handle is
    /// always valid.
    ///
    /// Eviction policy mirrors schism's `csf_get_nna_channel`
    /// (`effects.c:1620`): voices that are still sustained (not yet
    /// in their fadeout phase) are heavily protected — their score
    /// is multiplied by a large constant. Only when *every* voice
    /// is sustained do we fall back to evicting one of them. In
    /// that pathological case we pick the lowest-volume of the
    /// sustained set.
    pub fn allocate_live(&mut self, voice: Voice<'a>) -> VoiceId {
        if self.free_head == NIL {
            // Pool full → must evict. `find_eviction_victim` always
            // returns a slot when there's at least one occupied
            // slot — and the pool has capacity ≥ 1, so this never
            // panics.
            let victim_slot = self
                .find_eviction_victim()
                .expect("allocate_live: pool full ⇒ at least one slot is Occupied");
            let gen = self.slots[victim_slot].generation;
            self.release(VoiceId::new(victim_slot, gen));
        }
        self.place_in_free_slot(voice)
    }

    /// Allocate a voice that *may* be refused.
    ///
    /// Used for ghost voices spawned by NNA on a channel retrigger.
    /// Returns `None` when every existing voice is protected — in
    /// that case the caller drops the ghost, the channel just plays
    /// its new note without leaving a tail. Mirrors schism's
    /// `csf_get_nna_channel` returning 0 (`effects.c:1638`):
    ///
    /// ```c
    /// if (!chan->fadeout_volume) return 0;  // refuse the NNA spawn
    /// ```
    ///
    /// This refusal is exactly what stops `Another Life`'s 96-note
    /// ghost storm on pattern 4 from saturating the pool: once 256
    /// NNA=Continue voices are sustaining, the 257th request is
    /// silently dropped instead of stealing one of the audible
    /// existing voices.
    pub fn allocate_ghost(&mut self, voice: Voice<'a>) -> Option<VoiceId> {
        if self.free_head == NIL {
            let victim_slot = self.find_evictable_victim()?;
            let gen = self.slots[victim_slot].generation;
            self.release(VoiceId::new(victim_slot, gen));
        }
        Some(self.place_in_free_slot(voice))
    }

    /// Internal: pop the head of the free list and place `voice`
    /// there. Caller must have ensured `free_head != NIL`.
    ///
    /// The free-list invariant — every index it points to refers
    /// to a `Vacant` slot — is upheld by [`Self::new`] (initial
    /// chain) and [`Self::release`] (re-insertion). Callers that
    /// don't violate that invariant cannot trigger the
    /// `Occupied` branch of the match below; reaching it would
    /// mean the list was corrupted by some other code path.
    fn place_in_free_slot(&mut self, voice: Voice<'a>) -> VoiceId {
        let slot_idx = self.free_head;
        let slot = &mut self.slots[slot_idx];
        let next = match slot.state {
            SlotState::Vacant { next_free } => next_free,
            SlotState::Occupied(_) => unreachable!(
                "VoicePool free-list invariant violated: free_head ({}) points to an Occupied slot. \
                 This indicates a bug in release() or a double-free; the free-list and SlotState \
                 must agree by construction.",
                slot_idx
            ),
        };
        slot.state = SlotState::Occupied(voice);
        self.free_head = next;
        self.allocated += 1;
        VoiceId::new(slot_idx, slot.generation)
    }

    pub fn set_interpolation(&mut self, interpolation: crate::interpolation::Interpolation) {
        for slot in &mut self.slots {
            if let SlotState::Occupied(voice) = &mut slot.state {
                voice.instr.set_interpolation(interpolation);
            }
        }
    }

    /// Eviction-victim picker for `allocate_ghost`.
    ///
    /// Returns `Some(slot)` only when the pool contains at least one
    /// "evictable" voice — i.e. a voice that has already been keyed
    /// off (`!sustained`) and is now in its fadeout phase. Returns
    /// `None` when every occupied voice is still sustained — that's
    /// the case schism handles by refusing the NNA, see
    /// `effects.c:1638` (`if (!chan->fadeout_volume) return 0;`).
    ///
    /// Sustained voices are protected unconditionally, even if their
    /// volume envelope momentarily reads zero. The envelope is just
    /// a curve the author drew; a sustain that holds zero (swell
    /// pads, V-shapes, release sections crossing zero) is a
    /// legitimate IT shape and must not become an eviction
    /// candidate. Mirrors schism's score loop, which only ranks
    /// voices in the unsustained / `CHN_NOTEFADE` group.
    ///
    /// Among evictable voices, picks the one with the lowest
    /// fadeout-weighted volume. Looping voices' score is halved
    /// (they can ring forever, so they're cheaper to drop than
    /// one-shot tails) — same heuristic as schism's
    /// `if (CHN_LOOP) v >>= 1`.
    fn find_evictable_victim(&self) -> Option<usize> {
        let mut best: Option<(usize, i32)> = None;
        for (i, slot) in self.slots.iter().enumerate() {
            let SlotState::Occupied(v) = &slot.state else {
                continue;
            };
            // Sustained voices are never evictable from this path.
            // The envelope value is irrelevant — sustain means "the
            // author is holding this note", and we don't pre-empt
            // that just because the current envelope point happens
            // to be zero.
            if v.instr.sustained {
                continue;
            }
            let score = self.eviction_score(v);
            best = match best {
                None => Some((i, score)),
                Some((_, bs)) if score < bs => Some((i, score)),
                Some(p) => Some(p),
            };
        }
        best.map(|(i, _)| i)
    }

    /// Eviction-victim picker for `allocate_live`. Falls back to
    /// stealing a sustained voice when no evictable voice exists —
    /// the live note must play, even at the cost of cutting an
    /// audible NNA tail.
    fn find_eviction_victim(&self) -> Option<usize> {
        // First try the polite path: an evictable voice.
        if let Some(slot) = self.find_evictable_victim() {
            return Some(slot);
        }
        // Fallback: every voice is sustained and audible. Pick the
        // lowest-scoring one anyway — better to lose one NNA tail
        // than to drop the live note from the pattern.
        let mut best: Option<(usize, i32)> = None;
        for (i, slot) in self.slots.iter().enumerate() {
            let SlotState::Occupied(v) = &slot.state else {
                continue;
            };
            let score = self.eviction_score(v);
            best = match best {
                None => Some((i, score)),
                Some((_, bs)) if score < bs => Some((i, score)),
                Some(p) => Some(p),
            };
        }
        best.map(|(i, _)| i)
    }

    /// Score used to rank voices for eviction. Lower = more likely
    /// victim. Mirrors schism's formula in `csf_get_nna_channel`
    /// (`effects.c:1646`):
    ///
    /// ```c
    /// uint32_t v = pj->volume;
    /// if (pj->flags & CHN_NOTEFADE)
    ///     v = v * pj->fadeout_volume;
    /// else
    ///     v <<= 16;
    /// if (pj->flags & CHN_LOOP) v >>= 1;
    /// ```
    ///
    /// In other words: score uses the channel volume × the fadeout
    /// register for unsustained voices, and a large protection
    /// constant times channel volume for sustained ones. The current
    /// envelope value does not appear — schism's `pj->volume` is the
    /// pre-envelope channel volume, and the alive predicate works the
    /// same way. Mixing the live envelope into the ranking would
    /// (a) make a transient zero on a long-tail pad rate it lower
    /// than a near-silent dying note, and (b) flicker the ranking
    /// on every tick instead of letting fadeout's monotonic decay
    /// produce a stable victim.
    ///
    /// We use `vol_frozen` rather than `instr.get_volume()` because
    /// ghost voices snapshot their channel-side gain at fork time;
    /// the channel can no longer modulate them, so the frozen value
    /// is the right "channel volume" to multiply.
    fn eviction_score(&self, v: &Voice<'a>) -> i32 {
        // Pure Q-format ranking. We don't need an absolute
        // magnitude, just a consistent ordering — so the score
        // stays in `i32` and callers compare with `<`.
        //
        // Formula:
        //   sustained:    vol × 1024       → `vol_q15 << 10`
        //   unsustained:  vol × fadeout    → `(vol_q15 × fadeout_q15) >> 15`
        //   looping:      score × 0.5      → `>> 1`
        //
        // Sustained range: `[0, 32767 × 1024 ≈ 3.4 × 10⁷]`
        // Unsustained range: `[0, 32767]`
        // Sustained always outranks unsustained at equal `vol`.
        let v_q15 = v.instr.volume.as_q15_i32();
        let mut score: i32 = if v.instr.sustained {
            v_q15 << 10
        } else {
            let f_q15 = v.instr.volume_fadeout.as_q15_i32();
            (v_q15 * f_q15) >> 15
        };
        if v.instr
            .state_sample
            .as_ref()
            .is_some_and(|s| s.is_looping())
        {
            score >>= 1;
        }
        score
    }

    /// Release the voice held by `id`. No-op (and not an error) when
    /// `id` is stale — releasing the same handle twice, or a handle
    /// from another pool, simply does nothing.
    pub fn release(&mut self, id: VoiceId) {
        let slot_idx = id.slot();
        let Some(slot) = self.slots.get_mut(slot_idx) else {
            return;
        };
        if slot.generation != id.generation() {
            return;
        }
        if matches!(slot.state, SlotState::Vacant { .. }) {
            return;
        }
        slot.generation = slot.generation.wrapping_add(1);
        // generation 0 is reserved (would make the packed VoiceId
        // potentially zero); skip past it on wrap. With 16-bit
        // generations this happens after 65535 reuses of a slot —
        // well beyond any realistic playback session, but we handle
        // it correctly anyway.
        if slot.generation == 0 {
            slot.generation = 1;
        }
        slot.state = SlotState::Vacant {
            next_free: self.free_head,
        };
        self.free_head = slot_idx;
        self.allocated -= 1;
    }

    /// Resolve a handle to an immutable reference, or `None` if the
    /// handle is stale.
    #[inline(always)]
    pub fn get(&self, id: VoiceId) -> Option<&Voice<'a>> {
        let slot = self.slots.get(id.slot())?;
        if slot.generation != id.generation() {
            return None;
        }
        match &slot.state {
            SlotState::Occupied(v) => Some(v),
            SlotState::Vacant { .. } => None,
        }
    }

    /// Resolve a handle to a mutable reference, or `None` if the
    /// handle is stale.
    #[inline(always)]
    pub fn get_mut(&mut self, id: VoiceId) -> Option<&mut Voice<'a>> {
        let slot = self.slots.get_mut(id.slot())?;
        if slot.generation != id.generation() {
            return None;
        }
        match &mut slot.state {
            SlotState::Occupied(v) => Some(v),
            SlotState::Vacant { .. } => None,
        }
    }

    /// Iterate over every live voice and its handle. Test-only —
    /// production callers walk `Channel::ghosts` directly because
    /// they need to update the channel-side handle list in the same
    /// pass.
    #[cfg(test)]
    pub fn iter(&self) -> impl Iterator<Item = (VoiceId, &Voice<'a>)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| {
            if let SlotState::Occupied(v) = &slot.state {
                Some((VoiceId::new(i, slot.generation), v))
            } else {
                None
            }
        })
    }

    /// Mutable counterpart to `iter`. Test-only.
    #[cfg(test)]
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (VoiceId, &mut Voice<'a>)> {
        self.slots.iter_mut().enumerate().filter_map(|(i, slot)| {
            let gen = slot.generation;
            if let SlotState::Occupied(v) = &mut slot.state {
                Some((VoiceId::new(i, gen), v))
            } else {
                None
            }
        })
    }

    /// Run `f` against every live voice; release voices for which
    /// `f` returns `false`. Test-only.
    #[cfg(test)]
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(VoiceId, &mut Voice<'a>) -> bool,
    {
        let mut to_drop: Vec<VoiceId> = Vec::new();
        for (id, voice) in self.iter_mut() {
            if !f(id, voice) {
                to_drop.push(id);
            }
        }
        for id in to_drop {
            self.release(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_instr_default::StateInstrDefault;
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    use xmrs::core::fixed::units::{ChannelVolume, EnvValue, Panning, Volume};
    use xmrs::prelude::*;

    /// Minimal helper to build a fake `Voice`. The internals don't
    /// matter for pool tests — we only check allocation bookkeeping.
    fn dummy_voice() -> Voice<'static> {
        // We need an `&'static InstrDefault` to make the `'static`
        // lifetime work. A leaked Box gives us that for the duration
        // of the test process. Acceptable in #[cfg(test)] only.
        let instr: &'static InstrDefault = Box::leak(Box::new(InstrDefault::default()));
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        // `true` mirrors the historical unconditional `× self.volume`.
        let state = StateInstrDefault::new(instr, 0, ph, SampleRate::from_hz(44100), true);
        Voice::new_ghost(
            state,
            Volume::FULL,
            ChannelVolume::FULL,
            Panning::CENTER,
            None,
            None,
            Period::ZERO,
        )
    }

    #[test]
    fn empty_pool_reports_correct_state() {
        let pool: VoicePool<'_> = VoicePool::new(4);
        assert_eq!(pool.capacity(), 4);
        assert_eq!(pool.allocated_count(), 0);
        assert!(!pool.is_full());
    }

    #[test]
    #[should_panic]
    fn zero_capacity_panics() {
        let _pool: VoicePool<'_> = VoicePool::new(0);
    }

    #[test]
    fn allocate_then_release_returns_to_free_list() {
        let mut pool = VoicePool::new(2);
        let id1 = pool.allocate_live(dummy_voice());
        assert_eq!(pool.allocated_count(), 1);
        pool.release(id1);
        assert_eq!(pool.allocated_count(), 0);
        assert!(!pool.is_full());

        // The freed slot must be reusable. Allocating again should
        // succeed without touching the second slot.
        let id2 = pool.allocate_live(dummy_voice());
        assert_eq!(pool.allocated_count(), 1);
        // The slot index is the same (LIFO free-list), but the
        // generation has incremented so the two handles must differ.
        assert_eq!(id1.slot(), id2.slot());
        assert_ne!(id1, id2);
    }

    #[test]
    fn full_pool_evicts_lowest_volume() {
        // With 1.5's eviction strategy, allocate on a full pool steals
        // the voice with the lowest audible volume rather than failing.
        // We build voices with deliberately different `volume` values
        // so the eviction heuristic has a clear loser.
        let make_voice = |vol: Volume| -> Voice<'static> {
            let instr: &'static InstrDefault = Box::leak(Box::new(InstrDefault::default()));
            let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
            // `true` keeps `state.volume` (which the test sets just
            // below) flowing into `get_volume`, matching the eviction-
            // score arithmetic this test was written against.
            let mut state = StateInstrDefault::new(instr, 0, ph, SampleRate::from_hz(44100), true);
            // `state.volume` is the per-voice scalar that flows into
            // `get_volume()` and from there into the pool's score.
            state.volume = vol;
            Voice::new_ghost(
                state,
                Volume::FULL,
                ChannelVolume::FULL,
                Panning::CENTER,
                None,
                None,
                Period::ZERO,
            )
        };

        let mut pool = VoicePool::new(2);
        let _loud = pool.allocate_live(make_voice(Volume::FULL));
        let quiet = pool.allocate_live(make_voice(Volume::from_ratio(5, 100)));
        assert!(pool.is_full());

        // Allocating a third voice on a full pool steals the
        // quietest occupant. Its handle is now stale.
        let _new = pool.allocate_live(make_voice(Volume::from_ratio(50, 100)));
        assert_eq!(pool.allocated_count(), 2);
        assert!(
            pool.get(quiet).is_none(),
            "quietest voice should have been evicted"
        );
    }

    #[test]
    fn stale_handle_after_release_returns_none() {
        let mut pool = VoicePool::new(2);
        let id = pool.allocate_live(dummy_voice());
        pool.release(id);
        // The handle is stale; reading it must not return the
        // newly-released slot.
        assert!(pool.get(id).is_none());
        assert!(pool.get_mut(id).is_none());
    }

    #[test]
    fn stale_handle_after_slot_reuse_returns_none() {
        let mut pool = VoicePool::new(1);
        let id1 = pool.allocate_live(dummy_voice());
        pool.release(id1);
        let _id2 = pool.allocate_live(dummy_voice());
        // The original handle now points to a slot occupied by a
        // different voice. Generation mismatch must reject it.
        assert!(pool.get(id1).is_none());
    }

    #[test]
    fn double_release_is_a_noop() {
        let mut pool = VoicePool::new(1);
        let id = pool.allocate_live(dummy_voice());
        pool.release(id);
        pool.release(id); // must not corrupt state
        assert_eq!(pool.allocated_count(), 0);
        // Free-list must still be sane.
        let id3 = pool.allocate_live(dummy_voice());
        assert_eq!(pool.allocated_count(), 1);
        // generation should have only advanced once (first release),
        // not twice — the second release was rejected.
        assert_ne!(id, id3);
    }

    #[test]
    fn iter_visits_every_live_voice() {
        let mut pool = VoicePool::new(4);
        let id_a = pool.allocate_live(dummy_voice());
        let id_b = pool.allocate_live(dummy_voice());
        let _id_c = pool.allocate_live(dummy_voice());
        pool.release(id_b); // hole in the middle

        let live: Vec<VoiceId> = pool.iter().map(|(id, _)| id).collect();
        assert_eq!(live.len(), 2);
        assert!(live.contains(&id_a));
        assert!(!live.contains(&id_b));
    }

    #[test]
    fn retain_drops_voices_when_predicate_returns_false() {
        let mut pool = VoicePool::new(4);
        let _id1 = pool.allocate_live(dummy_voice());
        let _id2 = pool.allocate_live(dummy_voice());
        let _id3 = pool.allocate_live(dummy_voice());
        assert_eq!(pool.allocated_count(), 3);

        // Drop the first one we see. With three voices alive that
        // means we should end up with two.
        let mut dropped = false;
        pool.retain(|_id, _voice| {
            if !dropped {
                dropped = true;
                false
            } else {
                true
            }
        });
        assert_eq!(pool.allocated_count(), 2);
    }

    #[test]
    fn voice_id_packing_round_trips() {
        // Cover a range of (slot, generation) pairs to guard against
        // bit-twiddle errors in `VoiceId::new`/`slot`/`generation`.
        for slot in [0usize, 1, 5, 100, 0xFFFE] {
            for generation in [1u32, 2, 100, 0xFFFE, 0xFFFF] {
                let id = VoiceId::new(slot, generation);
                assert_eq!(id.slot(), slot);
                assert_eq!(id.generation(), generation);
            }
        }
    }

    /// Helper: a voice that's still sustained (audible NNA tail).
    fn sustained_voice(vol: Volume) -> Voice<'static> {
        let instr: &'static InstrDefault = Box::leak(Box::new(InstrDefault::default()));
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        // `true` keeps `state.volume` in the gain chain so the
        // eviction-score paths exercised below see the value the
        // test assigns just below.
        let mut state = StateInstrDefault::new(instr, 0, ph, SampleRate::from_hz(44100), true);
        state.volume = vol;
        // sustained=true is the default. Don't call key_off.
        Voice::new_ghost(
            state,
            Volume::FULL,
            ChannelVolume::FULL,
            Panning::CENTER,
            None,
            None,
            Period::ZERO,
        )
    }

    /// Helper: a voice that's already in fadeout (evictable).
    fn fading_voice(vol: Volume) -> Voice<'static> {
        let mut v = sustained_voice(vol);
        v.instr.sustained = false;
        v
    }

    #[test]
    fn allocate_ghost_refuses_when_all_voices_sustained() {
        // Schism's `csf_get_nna_channel` returns 0 (= refused) when
        // every existing voice is in `!CHN_NOTEFADE` state. Mirror
        // that: a ghost allocation on a pool full of sustained
        // voices must return None instead of stealing.
        let mut pool = VoicePool::new(2);
        let _a = pool.allocate_live(sustained_voice(Volume::FULL));
        let _b = pool.allocate_live(sustained_voice(Volume::FULL));
        assert!(pool.is_full());
        let result = pool.allocate_ghost(sustained_voice(Volume::FULL));
        assert!(result.is_none(), "ghost allocation must be refused");
        // Pool unchanged.
        assert_eq!(pool.allocated_count(), 2);
    }

    #[test]
    fn allocate_ghost_evicts_when_a_fading_voice_exists() {
        // Mixed pool: one sustained, one fading. The fading one is
        // the only evictable victim; ghost allocation should pick it.
        let mut pool = VoicePool::new(2);
        let sustained = pool.allocate_live(sustained_voice(Volume::FULL));
        let fading = pool.allocate_live(fading_voice(Volume::from_ratio(50, 100)));
        let new_id = pool
            .allocate_ghost(sustained_voice(Volume::FULL))
            .expect("an evictable voice was available");
        // The fading voice was the victim; its handle is stale,
        // the sustained voice survives.
        assert!(pool.get(fading).is_none(), "fading voice should be evicted");
        assert!(
            pool.get(sustained).is_some(),
            "sustained voice must survive"
        );
        assert!(pool.get(new_id).is_some(), "new ghost must be installed");
    }

    #[test]
    fn allocate_live_evicts_even_sustained_voices() {
        // Live allocation must always succeed: the user typed the
        // note, the player has to play it. Pool of two sustained
        // voices, allocate_live evicts one of them anyway.
        let mut pool = VoicePool::new(2);
        let a = pool.allocate_live(sustained_voice(Volume::from_ratio(50, 100)));
        let b = pool.allocate_live(sustained_voice(Volume::FULL));
        assert!(pool.is_full());
        let new_id = pool.allocate_live(sustained_voice(Volume::FULL));
        // The lower-volume sustained voice (`a`) is evicted; the
        // louder one (`b`) survives.
        assert!(pool.get(a).is_none(), "lower-vol sustained voice evicted");
        assert!(pool.get(b).is_some(), "louder sustained voice survives");
        assert!(pool.get(new_id).is_some(), "new live voice installed");
        assert_eq!(pool.allocated_count(), 2);
    }

    // -------- regression tests: voices stopping too early in IT --------
    //
    // The 0.10.x series killed ghost voices the moment their volume
    // envelope passed through zero, because `is_alive()` and the
    // eviction predicates both folded the live envelope value into
    // their "audible?" check. IT modules legitimately run envelopes
    // through zero (V-shapes, sustains authored at 0 for swells,
    // release segments crossing zero) — those voices must keep their
    // pool slot until either the sample exhausts or the monotonic
    // fadeout register decays to 0 post-key-off. Schism's policy:
    // `sndmix.c:1112` only reaps voices in `CHN_NOTEFADE` whose
    // fadeout register and output volumes have all reached 0.

    #[test]
    fn sustained_voice_at_zero_envelope_is_not_evictable() {
        // Pool with two sustained voices: one at full envelope, one
        // whose envelope happens to be at zero this tick. A ghost
        // allocation must NOT pick the zero-envelope voice as a
        // victim — sustained voices are protected unconditionally,
        // regardless of envelope value.
        //
        // Before the fix, `find_evictable_victim` skipped sustained
        // voices only when `instr.get_volume() > 1e-3`, so a
        // sustained voice on a transient envelope-zero would become
        // evictable and get reaped to make room for a new ghost.
        let mut pool = VoicePool::new(2);
        let full = pool.allocate_live(sustained_voice(Volume::FULL));
        let mut silent = sustained_voice(Volume::FULL);
        silent.instr.envelope_volume.value = EnvValue::default();
        let silent_id = pool.allocate_live(silent);
        assert!(pool.is_full());
        let result = pool.allocate_ghost(sustained_voice(Volume::FULL));
        assert!(
            result.is_none(),
            "ghost must be refused — both sustained voices are protected, \
             regardless of envelope value"
        );
        assert!(pool.get(full).is_some(), "full-envelope voice survives");
        assert!(
            pool.get(silent_id).is_some(),
            "zero-envelope sustained voice must not be evicted"
        );
    }
}
