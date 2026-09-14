//! Pure next and previous selection across the playback queue.
//!
//! The engine turns the owner [`PlaybackMode`] plus the shuffle bookkeeping
//! into a concrete queue index or a stop decision, so no application layer
//! ever encodes ordering rules itself. Randomness comes from a tiny inline
//! generator because playback ordering is comfort rather than security and
//! a single u64 state keeps tests deterministic through explicit seeds.
//!
//! Semantics owned here, deliberately documented instead of inherited:
//!
//! - track repeat pins the finished entry forever on auto advance
//! - repeat off halts once the active sequence is exhausted
//! - repeat all wraps around, regenerating a shuffled pass when needed
//! - shuffle draws only from tracks not yet played in the current pass and
//!   avoids handing back the immediately previous draw after a refill
//! - manual next shares the ordering but never gets trapped by track
//!   repeat, and manual previous walks the played history backwards before
//!   falling back to the sequential predecessor

use std::time::{SystemTime, UNIX_EPOCH};

use crate::playback_mode::{PlaybackMode, RepeatMode};

/// Outcome of one selection request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionAction {
    /// Select this queue index and start playing it.
    Play(usize),
    /// End playback because the sequence offered nothing further.
    Stop,
}

/// Minimal SplitMix64 generator backing shuffle draws.
///
/// A cryptographic generator would buy nothing here since the output only
/// orders local playback, while one u64 of state makes reseeding and
/// deterministic tests trivial.
#[derive(Debug, Clone, Copy)]
struct SplitMix64(u64);

impl SplitMix64 {
    /// Seed from the wall clock, precise enough for comfort shuffling.
    fn from_entropy() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        Self(nanos)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Draw below `bound`, which must be positive.
    ///
    /// The modulo bias is far below any perceptible threshold for playlist
    /// sized ranges, so rejection sampling would only add branching without
    /// any user visible improvement.
    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }
}

/// Shuffle pass bookkeeping plus the played history trail.
///
/// Everything is index based by design. Every structural queue mutation
/// rebuilds the state in one step because tracking piecemeal index remaps
/// across removals and reorders invites subtle staleness bugs that a full
/// rebuild rules out.
#[derive(Debug, Clone)]
pub struct NavigationState {
    /// Queue indices not yet visited during the current shuffle pass.
    ///
    /// A `Vec` with swap-remove keeps the draw O(1) and the pool compact, and
    /// avoids the allocation of a full candidate slice per shuffle pick. The
    /// no-immediate-repeat contract is enforced by temporarily moving the
    /// previous index aside rather than by rebuilding the pool each draw.
    unplayed: Vec<usize>,
    /// Visited indices newest last, the trail walked by manual previous.
    history: Vec<usize>,
    rng: SplitMix64,
}

impl Default for NavigationState {
    fn default() -> Self {
        Self::new()
    }
}

impl NavigationState {
    /// Create bookkeeping seeded from the wall clock.
    pub fn new() -> Self {
        Self::with_rng(SplitMix64::from_entropy())
    }

    /// Create bookkeeping from an explicit seed for deterministic tests.
    pub fn with_seed(seed: u64) -> Self {
        Self::with_rng(SplitMix64(seed))
    }

    fn with_rng(rng: SplitMix64) -> Self {
        Self {
            unplayed: Vec::new(),
            history: Vec::new(),
            rng,
        }
    }

    /// Record that playback started on `index`.
    ///
    /// Consecutive duplicates collapse so track repeat loops do not bury
    /// the trail under identical entries, keeping one previous press equal
    /// to one step back.
    pub fn record_started(&mut self, index: usize) {
        if self.history.last() != Some(&index) {
            self.history.push(index);
        }
        remove_value(&mut self.unplayed, index);
    }

    /// Register freshly appended tail indices as drawable.
    ///
    /// Tail appends never move existing indices, so the new entries can
    /// join the pool incrementally without discarding pass progress.
    pub fn note_appended(&mut self, previous_len: usize, new_len: usize) {
        self.unplayed.extend(previous_len..new_len);
    }

    /// Rebuild everything after a structural queue mutation such as a
    /// removal or a reorder.
    ///
    /// History dies here on purpose because old indices may point at other
    /// tracks after the edit, and a wrong jump is worse than a shorter one.
    /// The playing slot stays reserved so the current track can never be
    /// drawn as the immediate next after an edit.
    pub fn rebuild_after_mutation(&mut self, len: usize, playing: Option<usize>) {
        self.refill(len, playing);
        self.history.clear();
    }

    /// Refresh the unplayed pool while keeping the history trail.
    ///
    /// Enabling shuffle mid session uses this so previously heard tracks
    /// become drawable again without erasing where the listener came from.
    pub fn reshuffle(&mut self, len: usize, playing: Option<usize>) {
        self.refill(len, playing);
    }

    /// Drop every trace, used when the queue empties.
    pub fn reset(&mut self) {
        self.unplayed.clear();
        self.history.clear();
    }

    /// Pop back to the previously distinct visited track, if one exists.
    ///
    /// Trailing entries matching `current` are discarded first so repeated
    /// presses keep walking backwards even through repeat loops. The
    /// revealed entry stays recorded because the caller immediately starts
    /// it again, whose dedupe keeps the trail consistent.
    pub fn step_back(&mut self, current: usize) -> Option<usize> {
        while self.history.last() == Some(&current) {
            self.history.pop();
        }
        self.history.last().copied()
    }

    /// Draw the next shuffled index.
    ///
    /// Draws come from the unplayed pool first, guaranteeing no accidental
    /// repeats before every available track was heard. Once the pool runs
    /// dry it refills only when `regenerate` allows it, and the just heard
    /// track stays out of the immediate draw whenever another candidate
    /// exists. `None` means exhausted or empty queue.
    pub fn pick_next(
        &mut self,
        len: usize,
        previous: Option<usize>,
        regenerate: bool,
    ) -> Option<usize> {
        if len == 0 {
            return None;
        }

        if self.unplayed.is_empty() {
            if !regenerate {
                return None;
            }
            self.unplayed.clear();
            self.unplayed.extend(0..len);
        }

        // No-immediate-repeat without rebuilding a candidate vector: park the
        // previous track at the pool tail, draw from the prefix, and remove it
        // with swap_remove (which pulls the tail — the parked previous — into
        // the vacated slot, so it stays in the pool for later passes).
        if self.unplayed.len() > 1
            && let Some(prev_pos) = self.unplayed.iter().position(|&i| Some(i) == previous)
        {
            let last = self.unplayed.len() - 1;
            self.unplayed.swap(prev_pos, last);
            let pos = self.rng.below(last);
            let chosen = self.unplayed[pos];
            self.unplayed.swap_remove(pos);
            return Some(chosen);
        }

        let pos = self.rng.below(self.unplayed.len());
        let chosen = self.unplayed.swap_remove(pos);
        Some(chosen)
    }

    /// Test observability for the unplayed pool.
    pub fn unplayed_contains(&self, index: usize) -> bool {
        self.unplayed.contains(&index)
    }

    /// Test observability for the trail length.
    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// Shared refill behind mutation rebuilds and reshuffles.
    fn refill(&mut self, len: usize, playing: Option<usize>) {
        self.unplayed.clear();
        self.unplayed.extend(0..len);
        if let Some(index) = playing.filter(|&index| index < len) {
            remove_value(&mut self.unplayed, index);
        }
    }
}

/// Decision for the natural end of a track reported by the audio worker.
///
/// Callers must pass the queue cursor, which by playlist invariant is a
/// valid index whenever the length is nonzero.
pub fn auto_advance_action(
    mode: &PlaybackMode,
    current: usize,
    len: usize,
    nav: &mut NavigationState,
) -> SelectionAction {
    if len == 0 {
        return SelectionAction::Stop;
    }

    match mode.repeat() {
        // Track repeat pins the finished entry forever, shuffle cannot
        // override it because the listener asked for exactly this track
        RepeatMode::Track => SelectionAction::Play(current),
        RepeatMode::Off if !mode.shuffle() => sequential_next(current, len),
        RepeatMode::All if !mode.shuffle() => wrapped_next(current, len),
        RepeatMode::Off | RepeatMode::All => {
            // Exhaustion stops under off and opens a fresh pass under all
            let regenerate = mode.repeat() == RepeatMode::All;
            match nav.pick_next(len, Some(current), regenerate) {
                Some(index) => SelectionAction::Play(index),
                None => SelectionAction::Stop,
            }
        }
    }
}

/// Decision for an explicit next request from the listener.
///
/// Manual skips share the ordering of auto advance but track repeat never
/// traps them, mirroring how players keep the skip key useful in every
/// mode. At a sequential tail without wrap the request reports stop and
/// the caller decides how to surface that to the user.
pub fn manual_next_action(
    mode: &PlaybackMode,
    current: usize,
    len: usize,
    nav: &mut NavigationState,
) -> SelectionAction {
    if len == 0 {
        return SelectionAction::Stop;
    }

    if mode.shuffle() {
        // A manual skip may always open the next shuffled pass
        return match nav.pick_next(len, Some(current), true) {
            Some(index) => SelectionAction::Play(index),
            None => SelectionAction::Stop,
        };
    }

    match mode.repeat() {
        RepeatMode::All => wrapped_next(current, len),
        _ => sequential_next(current, len),
    }
}

/// Target for a manual previous step once the restart rule allowed a move.
///
/// The played history wins because it reflects what the listener actually
/// heard, including shuffle detours, then the sequential predecessor takes
/// over. `None` means the first track with an empty trail, where callers
/// restart the current track like before.
pub fn manual_previous_target(nav: &mut NavigationState, current: usize) -> Option<usize> {
    nav.step_back(current).or_else(|| current.checked_sub(1))
}

/// Sequential advance that stops at the tail.
fn sequential_next(current: usize, len: usize) -> SelectionAction {
    match current.checked_add(1) {
        Some(next) if next < len => SelectionAction::Play(next),
        _ => SelectionAction::Stop,
    }
}

/// Sequential advance that wraps back to the head at the tail.
fn wrapped_next(current: usize, len: usize) -> SelectionAction {
    // The cursor invariant keeps current below len, so the modulo lands
    // inside the queue even for the final entry. `len` must be positive:
    // modulo by zero would panic, and an empty queue never reaches the wrap
    // path (callers short-circuit on empty), which this assertion documents
    // as a link-time contract rather than an accident.
    debug_assert!(len > 0, "wrapped_next requires a non-empty queue");
    let next = current.checked_add(1).map_or(0, |value| value % len);
    SelectionAction::Play(next)
}

/// Remove `value` from a swap pool, preserving the O(1) draw invariant.
///
/// `swap_remove` is order independent, which is exactly what a shuffled pool
/// needs: it keeps the pool compact and lets `pick_next` draw by position
/// without a linear scan to delete an arbitrary value.
fn remove_value(pool: &mut Vec<usize>, value: usize) {
    if let Some(pos) = pool.iter().position(|&candidate| candidate == value) {
        pool.swap_remove(pos);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a table row expects from the engine.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Expected {
        /// The exact decision.
        Exact(SelectionAction),
        /// Any live draw from the queue except the just heard track.
        DrawExcluding { forbidden: usize, len: usize },
    }

    impl Expected {
        fn assert_matches(&self, action: SelectionAction, label: &str) {
            match *self {
                Expected::Exact(expected) => assert_eq!(
                    action, expected,
                    "{label}: expected {expected:?}, got {action:?}"
                ),
                Expected::DrawExcluding { forbidden, len } => match action {
                    SelectionAction::Play(index) => {
                        assert!(index < len, "{label}: draw {index} outside 0..{len}");
                        assert_ne!(
                            index, forbidden,
                            "{label}: drew the just heard track {index}"
                        );
                    }
                    SelectionAction::Stop => {
                        panic!("{label}: expected a live draw, got stop")
                    }
                },
            }
        }
    }

    struct Case {
        name: &'static str,
        mode: PlaybackMode,
        current: usize,
        len: usize,
        expected_auto: Expected,
        expected_manual: Expected,
    }

    fn mode(repeat: RepeatMode, shuffle: bool) -> PlaybackMode {
        PlaybackMode::new(repeat, shuffle)
    }

    fn run_case(case: &Case) {
        // Mirror the application wiring, every queued index joins the
        // shuffle pool when queued and the playing track registers itself
        // as heard before it can finish
        let mut nav_mid = NavigationState::with_seed(7);
        nav_mid.note_appended(0, case.len);
        nav_mid.record_started(case.current);

        let mut nav_tail = NavigationState::with_seed(11);
        nav_tail.note_appended(0, case.len);
        nav_tail.record_started(case.current);

        let auto = auto_advance_action(&case.mode, case.current, case.len, &mut nav_mid);
        case.expected_auto.assert_matches(auto, case.name);

        let manual = manual_next_action(&case.mode, case.current, case.len, &mut nav_tail);
        case.expected_manual.assert_matches(manual, case.name);
    }

    #[test]
    fn selection_table_covers_all_six_mode_states() {
        let cases = [
            Case {
                name: "off sequential advances then stops",
                mode: mode(RepeatMode::Off, false),
                current: 1,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Play(2)),
                expected_manual: Expected::Exact(SelectionAction::Play(2)),
            },
            Case {
                name: "off sequential tail stops",
                mode: mode(RepeatMode::Off, false),
                current: 2,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Stop),
                expected_manual: Expected::Exact(SelectionAction::Stop),
            },
            Case {
                name: "track repeat replays on auto while manual skips",
                mode: mode(RepeatMode::Track, false),
                current: 1,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Play(1)),
                expected_manual: Expected::Exact(SelectionAction::Play(2)),
            },
            Case {
                name: "track repeat manual skip still walks forward",
                mode: mode(RepeatMode::Track, false),
                current: 2,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Play(2)),
                expected_manual: Expected::Exact(SelectionAction::Stop),
            },
            Case {
                name: "repeat all wraps at the tail",
                mode: mode(RepeatMode::All, false),
                current: 2,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Play(0)),
                expected_manual: Expected::Exact(SelectionAction::Play(0)),
            },
            Case {
                name: "shuffle draws around the playing track",
                mode: mode(RepeatMode::Off, true),
                current: 1,
                len: 3,
                expected_auto: Expected::DrawExcluding {
                    forbidden: 1,
                    len: 3,
                },
                expected_manual: Expected::DrawExcluding {
                    forbidden: 1,
                    len: 3,
                },
            },
            Case {
                name: "track repeat beats shuffle on auto advance only",
                mode: mode(RepeatMode::Track, true),
                current: 1,
                len: 3,
                expected_auto: Expected::Exact(SelectionAction::Play(1)),
                expected_manual: Expected::DrawExcluding {
                    forbidden: 1,
                    len: 3,
                },
            },
            Case {
                name: "shuffle plus repeat all keeps drawing",
                mode: mode(RepeatMode::All, true),
                current: 1,
                len: 3,
                expected_auto: Expected::DrawExcluding {
                    forbidden: 1,
                    len: 3,
                },
                expected_manual: Expected::DrawExcluding {
                    forbidden: 1,
                    len: 3,
                },
            },
        ];

        for case in &cases {
            run_case(case);
        }
    }

    #[test]
    fn shuffle_pass_yields_a_full_permutation_before_repeating() {
        let mut nav = NavigationState::with_seed(42);
        // Mirror the application wiring, every queued index joins the pool
        // before playback starts on the first entry
        nav.note_appended(0, 7);
        nav.record_started(0);

        let mut picks = Vec::new();
        for _ in 0..6 {
            match manual_next_action(&mode(RepeatMode::Off, true), 0, 7, &mut nav) {
                SelectionAction::Play(index) => picks.push(index),
                SelectionAction::Stop => panic!("pass must not stop before exhaustion"),
            }
            // The engine owns pool updates through record_started at the
            // caller, simulate the application recording each begun track
            let last = *picks.last().expect("just picked");
            nav.record_started(last);
        }

        picks.sort_unstable();
        assert_eq!(picks, vec![1, 2, 3, 4, 5, 6], "one pass visits each once");
    }

    #[test]
    fn shuffle_exhaustion_stops_under_repeat_off_and_regenerates_under_all() {
        let mut nav = NavigationState::with_seed(5);
        nav.note_appended(0, 3);
        for index in 0..3 {
            nav.record_started(index);
        }

        let stopped = auto_advance_action(&mode(RepeatMode::Off, true), 2, 3, &mut nav);
        assert_eq!(stopped, SelectionAction::Stop);

        // A manual skip always finds a fresh pass even under repeat off
        let skipped = manual_next_action(&mode(RepeatMode::Off, true), 2, 3, &mut nav);
        match skipped {
            SelectionAction::Play(index) => {
                assert!(index < 3, "manual draw must stay inside the queue");
                assert_ne!(index, 2, "fresh pass may not hand back the same track");
            }
            SelectionAction::Stop => panic!("manual next regenerates instead of stopping"),
        }

        let looping = auto_advance_action(&mode(RepeatMode::All, true), 2, 3, &mut nav);
        match looping {
            SelectionAction::Play(index) => {
                assert!(index < 3, "draw must stay inside the queue");
                assert_ne!(index, 2, "fresh pass may not hand back the same track");
            }
            SelectionAction::Stop => panic!("repeat all regenerates instead of stopping"),
        }
    }

    #[test]
    fn regeneration_avoids_the_immediately_previous_track_when_possible() {
        for seed in 0..64u64 {
            let mut nav = NavigationState::with_seed(seed);
            for index in 0..4 {
                nav.record_started(index);
            }

            for step in 0..12 {
                let previous = if step == 0 { Some(3) } else { None };
                let picked = nav
                    .pick_next(4, previous, true)
                    .expect("regeneration keeps offering tracks");
                assert!(picked < 4, "seed {seed}: draw outside the queue");

                if step == 0 {
                    assert_ne!(picked, 3, "seed {seed}: fresh pass reused the last track");
                    nav.record_started(picked);
                }
            }
        }
    }

    #[test]
    fn shuffle_refill_does_not_lose_a_track_when_previous_is_in_pool() {
        let mut nav = NavigationState::with_seed(1);
        for index in 0..5 {
            nav.record_started(index);
        }

        let mut previous = Some(3);
        let mut picks = Vec::new();
        for _ in 0..5 {
            let picked = nav
                .pick_next(5, previous, true)
                .expect("regeneration keeps offering tracks");
            assert_ne!(
                Some(picked),
                previous,
                "shuffle repeated the previous track"
            );
            picks.push(picked);
            nav.record_started(picked);
            previous = Some(picked);
        }

        picks.sort_unstable();
        assert_eq!(picks, (0..5).collect::<Vec<_>>());
    }

    #[test]
    fn consecutive_shuffle_draws_never_repeat_for_multi_track_queues() {
        for seed in 0..32u64 {
            let mut nav = NavigationState::with_seed(seed);
            nav.note_appended(0, 5);
            nav.record_started(0);

            let mut last = Some(0usize);
            for _ in 0..40 {
                let previous = last;
                let picked = nav.pick_next(5, previous, true).expect("queue nonempty");
                assert_ne!(Some(picked), previous, "seed {seed} repeated immediately");
                nav.record_started(picked);
                last = Some(picked);
            }
        }
    }

    #[test]
    fn degenerate_queues_stay_safe_in_every_engine_path() {
        let mut empty = NavigationState::with_seed(1);

        for repeat in [RepeatMode::Off, RepeatMode::Track, RepeatMode::All] {
            for shuffle in [false, true] {
                let play_mode = mode(repeat, shuffle);
                assert_eq!(
                    auto_advance_action(&play_mode, 0, 0, &mut empty),
                    SelectionAction::Stop
                );
                assert_eq!(
                    manual_next_action(&play_mode, 0, 0, &mut empty),
                    SelectionAction::Stop
                );
            }
        }
        assert_eq!(empty.pick_next(0, None, true), None);

        // Single track queues loop onto themselves under all and shuffle
        let mut solo = NavigationState::with_seed(2);
        solo.record_started(0);
        assert_eq!(
            auto_advance_action(&mode(RepeatMode::All, true), 0, 1, &mut solo),
            SelectionAction::Play(0)
        );
        assert_eq!(
            auto_advance_action(&mode(RepeatMode::All, false), 0, 1, &mut solo),
            SelectionAction::Play(0)
        );

        // A single finished track stops under off, shuffled or not
        let mut once = NavigationState::with_seed(3);
        once.record_started(0);
        assert_eq!(
            auto_advance_action(&mode(RepeatMode::Off, true), 0, 1, &mut once),
            SelectionAction::Stop
        );
    }

    #[test]
    fn manual_previous_walks_the_history_back_one_step_per_press() {
        let mut nav = NavigationState::with_seed(9);
        for index in 0..3 {
            nav.record_started(index);
        }

        assert_eq!(manual_previous_target(&mut nav, 2), Some(1));
        // The application records the begun target, whose dedupe keeps it
        nav.record_started(1);
        assert_eq!(manual_previous_target(&mut nav, 1), Some(0));
        nav.record_started(0);
        assert_eq!(manual_previous_target(&mut nav, 0), None);
    }

    #[test]
    fn history_collapse_skips_track_repeat_loops() {
        let mut nav = NavigationState::with_seed(10);
        nav.record_started(0);
        nav.record_started(1);
        nav.record_started(2);
        // Track repeat replays the same entry many times
        nav.record_started(2);
        nav.record_started(2);

        assert_eq!(nav.history_len(), 3, "duplicates collapse onto one entry");
        assert_eq!(manual_previous_target(&mut nav, 2), Some(1));
    }

    #[test]
    fn structural_mutation_rebuilds_pools_and_drops_stale_trail() {
        let mut nav = NavigationState::with_seed(13);
        for index in 0..4 {
            nav.record_started(index);
        }

        // Queue shrank from four entries to three, playing slot reserved
        nav.rebuild_after_mutation(3, Some(1));

        assert!(!nav.unplayed_contains(1), "playing slot stays reserved");
        assert!(nav.unplayed_contains(0));
        assert_eq!(nav.history_len(), 0, "old trail points at stale indices");
        assert_eq!(nav.step_back(1), None, "no stale jumps after an edit");

        let picked = manual_next_action(&mode(RepeatMode::Off, true), 1, 3, &mut nav);
        match picked {
            SelectionAction::Play(index) => assert!(index < 3 && index != 1),
            SelectionAction::Stop => panic!("two drawable tracks remain"),
        }
    }

    #[test]
    fn appended_tracks_join_the_pool_without_resetting_progress() {
        let mut nav = NavigationState::with_seed(17);
        nav.note_appended(0, 2);
        nav.record_started(0);
        nav.record_started(1);

        nav.note_appended(2, 4);

        assert!(nav.unplayed_contains(2));
        assert!(nav.unplayed_contains(3));
        assert!(!nav.unplayed_contains(0), "heard tracks stay heard");

        // Exhausting the original pair still stops under off until the new
        // tail entries are drawn, proving they joined the same pass
        let first_new = manual_next_action(&mode(RepeatMode::Off, true), 1, 4, &mut nav);
        match first_new {
            SelectionAction::Play(index) => {
                assert!(index == 2 || index == 3, "only the new entries remain");
                nav.record_started(index);
            }
            SelectionAction::Stop => panic!("appended entries must be drawable"),
        }
    }
}
