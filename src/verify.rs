// verify — D9 two-tier verifier state machine.
// SPDX-License-Identifier: GPL-3.0-only
//
// Manages bounded-timeout workspace activation verification:
//   Signal A — INFO-once-per-workspace-handle (distinct ObjectId)
//   Signal B — WARN-once-per-process after N distinct handles time out with no prior confirmation
//
// Pure-logic module: no calloop, no Wayland types.
// Type aliases allow testing without `wayland-client` or `calloop`.
// The caller (Phase 3, T-017) wires up the real Wayland/calloop types.
//
// Design constraint (Constraint 3):
//   record_timeout returns VerifyEvent so emissions are observable in tests.
//
// Implemented in T-007 (Phase 1).

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use crate::ids::WorkspaceId;

// ---------------------------------------------------------------------------
// Type aliases (pure-logic; Phase 3 wires to real wayland_client::backend::ObjectId)
// ---------------------------------------------------------------------------

// WorkspaceId is hoisted to `crate::ids` so Phase 3 can swap its underlying
// representation in one place. TimerId stays local because it is a
// calloop-specific concept introduced in Phase 3 wiring.
pub type TimerId = u64;

// ---------------------------------------------------------------------------
// Event returned from record_* methods (observable in tests; caller logs these)
// ---------------------------------------------------------------------------

/// Events emitted from state transitions. The caller logs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyEvent {
    /// No emission required.
    None,
    /// INFO-once-per-handle: workspace activation not confirmed within timeout.
    InfoOnceForHandle,
    /// WARN-once-per-process: compositor appears to not honor workspace activation.
    WarnOnceForProcess,
    /// Both INFO and WARN fire in the same timeout event (first timeout at N==warn_threshold).
    InfoAndWarn,
}

// ---------------------------------------------------------------------------
// State machine
// ---------------------------------------------------------------------------

pub struct VerifierState {
    /// workspace handle id → timer id (opaque to this module; caller manages timer lifecycle)
    pending: HashMap<WorkspaceId, TimerId>,
    /// All distinct workspace handle ids attempted in process lifetime.
    attempted_distinct: HashSet<WorkspaceId>,
    /// Once true (any activation ever confirmed), never cleared.
    ever_confirmed: bool,
    /// WARN-once-per-process latch.
    process_warn_fired: bool,
    /// INFO-once-per-workspace-handle latch.
    info_emitted: HashSet<WorkspaceId>,
    /// N threshold for the WARN-once-per-process Signal B. Default 3.
    warn_threshold: usize,
}

impl VerifierState {
    /// Create a new verifier with the given warn threshold (N=3 by default in production).
    ///
    /// `warn_threshold` MUST be >= 1. A threshold of 0 would make the WARN-once
    /// trigger evaluate `attempted_distinct.len() >= 0` — trivially true — so the
    /// very first timeout would emit WARN without any attempts having been made.
    /// That degenerate state has no defensible interpretation under D9's two-tier
    /// semantics, so we reject it at construction rather than smuggling it into
    /// the state machine.
    pub fn new(warn_threshold: usize) -> Self {
        assert!(
            warn_threshold >= 1,
            "VerifierState::new: warn_threshold must be >= 1 (got {}); a threshold of 0 would fire WARN on the first timeout with no prior attempts, which has no meaning under D9 Signal B",
            warn_threshold,
        );
        Self {
            pending: HashMap::new(),
            attempted_distinct: HashSet::new(),
            ever_confirmed: false,
            process_warn_fired: false,
            info_emitted: HashSet::new(),
            warn_threshold,
        }
    }

    /// Called by placement after issuing activate + commit for a workspace handle.
    ///
    /// Registers the handle as pending.
    /// If the caller passes `timer_id = None`, the handle is still tracked in
    /// `attempted_distinct` (caller opted for no timer but still wants accounting).
    pub fn record_attempt(&mut self, handle: WorkspaceId, timer_id: Option<TimerId>) {
        self.attempted_distinct.insert(handle);
        if let Some(tid) = timer_id {
            // S4/S5: a second attempt for the SAME handle while a previous timer is
            // still pending almost certainly indicates a caller-side correlation bug
            // (Phase 3 forgot to cancel the prior timer before re-issuing). In release
            // we tolerate it (insert overwrites), but debug builds fail loudly so the
            // bug surfaces in tests instead of silently leaking timer state.
            debug_assert!(
                !self.pending.contains_key(&handle),
                "record_attempt: handle {:?} already pending (caller likely forgot to cancel the prior timer before re-issuing activate); existing timer {:?}, new timer {:?}",
                handle,
                self.pending.get(&handle),
                tid,
            );
            self.pending.insert(handle, tid);
        }
    }

    /// Called when the compositor confirms the workspace is active (active state bit observed).
    ///
    /// Clears suspicion: removes from pending, sets `ever_confirmed`, resets
    /// `attempted_distinct` and `info_emitted`.
    /// NOTE: `process_warn_fired` is NOT cleared — a historical WARN is not "un-emitted".
    pub fn record_confirm(&mut self, handle: WorkspaceId) {
        self.pending.remove(&handle);
        self.ever_confirmed = true;
        self.attempted_distinct.clear();
        self.info_emitted.clear();
        // process_warn_fired is intentionally NOT cleared — per design constraint 3.
    }

    /// Called when the bounded timeout expires for a workspace handle.
    ///
    /// Returns a `VerifyEvent` that the caller should log at the appropriate level.
    /// Signal A: INFO-once-per-handle
    /// Signal B: WARN-once-per-process (when !ever_confirmed && !process_warn_fired && distinct >= N)
    pub fn record_timeout(&mut self, handle: WorkspaceId) -> VerifyEvent {
        // S4/S5: a timeout for a handle that was never recorded as an attempt
        // is a caller-side correlation bug — Phase 3 wired a timer for a handle
        // it didn't track. Release tolerates it (handle gets info_emitted, no
        // WARN side-effect since attempted_distinct is unaffected by this call).
        // Debug builds fail loudly so the bug surfaces in tests.
        debug_assert!(
            self.attempted_distinct.contains(&handle),
            "record_timeout: handle {:?} timed out but was never recorded as an attempt (caller-side correlation bug — a timer fired for a handle that record_attempt was never called with)",
            handle,
        );

        self.pending.remove(&handle);

        let info_fired = self.info_emitted.insert(handle);
        // info_fired == true means this is the FIRST timeout for this handle.

        let warn_fires = !self.ever_confirmed
            && !self.process_warn_fired
            && self.attempted_distinct.len() >= self.warn_threshold;

        if warn_fires {
            self.process_warn_fired = true;
        }

        match (info_fired, warn_fires) {
            (true, true) => VerifyEvent::InfoAndWarn,
            (true, false) => VerifyEvent::InfoOnceForHandle,
            (false, true) => VerifyEvent::WarnOnceForProcess,
            (false, false) => VerifyEvent::None,
        }
    }

    // --- Accessors for testing ---

    pub fn is_pending(&self, handle: WorkspaceId) -> bool {
        self.pending.contains_key(&handle)
    }

    pub fn ever_confirmed(&self) -> bool {
        self.ever_confirmed
    }

    pub fn process_warn_fired(&self) -> bool {
        self.process_warn_fired
    }

    pub fn info_was_emitted(&self, handle: WorkspaceId) -> bool {
        self.info_emitted.contains(&handle)
    }

    pub fn attempted_distinct_count(&self) -> usize {
        self.attempted_distinct.len()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn verifier(n: usize) -> VerifierState {
        VerifierState::new(n)
    }

    fn verifier_default() -> VerifierState {
        VerifierState::new(3)
    }

    // --- record_attempt ---

    #[test]
    fn record_attempt_adds_handle_to_pending_and_attempted_distinct() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        assert!(v.is_pending(1));
        assert_eq!(v.attempted_distinct_count(), 1);
    }

    #[test]
    fn record_attempt_with_no_timer_still_tracks_in_attempted_distinct() {
        let mut v = verifier_default();
        v.record_attempt(1, None);
        assert!(!v.is_pending(1));
        assert_eq!(v.attempted_distinct_count(), 1);
    }

    // --- record_confirm ---

    #[test]
    fn record_confirm_removes_handle_from_pending_and_sets_ever_confirmed() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        v.record_confirm(1);
        assert!(!v.is_pending(1));
        assert!(v.ever_confirmed());
    }

    #[test]
    fn record_confirm_clears_attempted_distinct_and_info_emitted() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        v.record_attempt(2, Some(200));
        // Manually mark info emitted for handle 1
        let _ = v.record_timeout(1); // advances info_emitted
        v.record_confirm(2);
        assert_eq!(v.attempted_distinct_count(), 0);
        assert!(!v.info_was_emitted(1));
    }

    #[test]
    fn record_confirm_does_not_clear_process_warn_fired() {
        let mut v = verifier(1); // N=1 so WARN fires on first timeout
        v.record_attempt(1, Some(100));
        let event = v.record_timeout(1);
        assert!(matches!(event, VerifyEvent::InfoAndWarn));
        assert!(v.process_warn_fired());

        // Now confirm handle 2 — process_warn_fired should still be true
        v.record_attempt(2, Some(200));
        v.record_confirm(2);
        assert!(v.process_warn_fired()); // per design: NOT cleared on confirm
    }

    // --- record_timeout: Signal A (INFO-once-per-handle) ---

    #[test]
    fn verifier_emits_info_once_per_handle_on_first_timeout() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        let event = v.record_timeout(1);
        assert!(matches!(event, VerifyEvent::InfoOnceForHandle | VerifyEvent::InfoAndWarn));
        assert!(v.info_was_emitted(1));
    }

    #[test]
    fn verifier_does_not_emit_info_again_for_same_handle_on_second_timeout() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        let first = v.record_timeout(1);
        assert!(matches!(first, VerifyEvent::InfoOnceForHandle));

        // Second timeout for the same handle
        let second = v.record_timeout(1);
        assert_eq!(second, VerifyEvent::None);
    }

    // --- record_timeout: Signal B (WARN-once-per-process) ---

    #[test]
    fn verifier_emits_warn_once_after_n_distinct_timeouts() {
        // FR-018: WARN fires when N distinct handles have been *attempted* with none confirmed.
        // All 3 attempts happen first; the first timeout that brings total timeouts >= 1
        // while attempted_distinct.len() >= 3 triggers the WARN.
        let mut v = verifier(3); // N=3
        v.record_attempt(1, Some(101));
        v.record_attempt(2, Some(102));
        v.record_attempt(3, Some(103));
        // attempted_distinct.len() is now 3 (all attempted before any timeout).
        // First timeout: INFO fires AND WARN fires (N >= 3 and no confirmation yet).

        let e1 = v.record_timeout(1);
        assert!(
            matches!(e1, VerifyEvent::InfoAndWarn),
            "1st timeout when 3 already attempted: INFO + WARN, got {e1:?}"
        );
        assert!(v.process_warn_fired());

        let e2 = v.record_timeout(2);
        assert_eq!(e2, VerifyEvent::InfoOnceForHandle, "2nd timeout: only INFO (WARN already fired)");

        let e3 = v.record_timeout(3);
        assert_eq!(e3, VerifyEvent::InfoOnceForHandle, "3rd timeout: only INFO");
    }

    #[test]
    fn verifier_does_not_emit_warn_again_after_process_warn_fired() {
        let mut v = verifier(1); // N=1
        v.record_attempt(1, Some(101));
        let e1 = v.record_timeout(1);
        assert!(matches!(e1, VerifyEvent::InfoAndWarn)); // WARN fires here

        // Additional distinct handles timing out
        v.record_attempt(2, Some(102));
        let e2 = v.record_timeout(2);
        assert_eq!(e2, VerifyEvent::InfoOnceForHandle); // No second WARN
    }

    #[test]
    fn verifier_does_not_emit_warn_when_ever_confirmed() {
        let mut v = verifier(1); // N=1
        v.record_attempt(1, Some(101));
        v.record_confirm(1); // ever_confirmed = true; attempted_distinct cleared

        v.record_attempt(2, Some(102));
        let event = v.record_timeout(2);
        // INFO fires (new handle), but WARN does NOT because ever_confirmed
        assert_eq!(event, VerifyEvent::InfoOnceForHandle);
        assert!(!v.process_warn_fired());
    }

    #[test]
    fn verifier_clears_suspicion_after_confirmation() {
        let mut v = verifier(3); // N=3
        v.record_attempt(1, Some(101));
        v.record_attempt(2, Some(102));
        let _ = v.record_timeout(1); // 1 distinct timed out
        let _ = v.record_timeout(2); // 2 distinct timed out — not yet N=3

        // Confirm handle 3 before reaching N
        v.record_attempt(3, Some(103));
        v.record_confirm(3);
        assert!(v.ever_confirmed());
        assert_eq!(v.attempted_distinct_count(), 0, "confirm should clear attempted_distinct");

        // Now more handles time out — WARN must NOT fire
        v.record_attempt(4, Some(104));
        v.record_attempt(5, Some(105));
        v.record_attempt(6, Some(106));
        let _ = v.record_timeout(4);
        let _ = v.record_timeout(5);
        let e6 = v.record_timeout(6);
        // ever_confirmed is true → no WARN
        assert!(!matches!(e6, VerifyEvent::WarnOnceForProcess | VerifyEvent::InfoAndWarn));
        assert!(!v.process_warn_fired());
    }

    // --- pending cleanup ---

    #[test]
    fn record_timeout_removes_handle_from_pending() {
        let mut v = verifier_default();
        v.record_attempt(1, Some(100));
        assert!(v.is_pending(1));
        let _ = v.record_timeout(1);
        assert!(!v.is_pending(1));
    }

    // --- warn fires when N==threshold exactly (off-by-one check) ---

    #[test]
    fn verifier_warn_fires_when_attempted_distinct_reaches_exactly_threshold() {
        // N=2: both attempts happen first, so the FIRST timeout (with 2 already attempted)
        // triggers the WARN.
        let mut v = verifier(2); // N=2
        v.record_attempt(1, Some(101));
        v.record_attempt(2, Some(102));
        // attempted_distinct.len() == 2 == N at first timeout.

        let e1 = v.record_timeout(1);
        assert!(
            matches!(e1, VerifyEvent::InfoAndWarn),
            "1st timeout when 2 already attempted (at threshold): WARN expected, got {e1:?}"
        );

        let e2 = v.record_timeout(2);
        assert_eq!(e2, VerifyEvent::InfoOnceForHandle, "2nd timeout: only INFO (warn already fired)");
    }

    #[test]
    fn verifier_warn_fires_on_nth_attempt_not_nth_timeout() {
        // Interleaved: attempt → timeout → attempt → timeout → attempt → timeout.
        // With N=3, WARN fires on the 3rd timeout (3rd distinct attempt, all timed out).
        let mut v = verifier(3);
        v.record_attempt(1, Some(101));
        let e1 = v.record_timeout(1);
        assert_eq!(e1, VerifyEvent::InfoOnceForHandle, "1 attempted+timed out, only INFO");

        v.record_attempt(2, Some(102));
        let e2 = v.record_timeout(2);
        assert_eq!(e2, VerifyEvent::InfoOnceForHandle, "2 attempted+timed out, only INFO");

        v.record_attempt(3, Some(103));
        let e3 = v.record_timeout(3);
        // Now attempted_distinct.len() == 3 == N: WARN fires.
        assert!(
            matches!(e3, VerifyEvent::InfoAndWarn),
            "3rd distinct attempt+timeout: INFO + WARN expected, got {e3:?}"
        );
    }

    // -----------------------------------------------------------------------
    // S6 — degenerate state guard
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "warn_threshold must be >= 1")]
    fn verifier_new_panics_on_zero_warn_threshold() {
        // warn_threshold = 0 has no defensible interpretation: the WARN check
        // `attempted_distinct.len() >= 0` would be trivially true, firing WARN
        // on the first timeout with no prior attempts. Construction must reject it.
        let _ = VerifierState::new(0);
    }

    // -----------------------------------------------------------------------
    // S4 / S5 — debug-asserts catch caller-side correlation bugs
    // -----------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "already pending")]
    fn verifier_record_attempt_panics_on_duplicate_pending_handle_debug() {
        // A second attempt for a handle whose previous timer is still pending
        // indicates the caller forgot to cancel the prior timer. In debug builds
        // we fail loudly to surface the bug in tests; in release we tolerate
        // (insert overwrites). This test only runs in debug builds (cargo test).
        let mut v = VerifierState::new(3);
        v.record_attempt(1, Some(100));
        // Caller bug: re-attempt without prior timer cancellation.
        v.record_attempt(1, Some(101));
    }

    #[test]
    #[should_panic(expected = "never recorded as an attempt")]
    fn verifier_record_timeout_panics_on_unknown_handle_debug() {
        // A timeout for a handle that was never recorded as an attempt indicates
        // a caller-side timer-handle correlation bug. Debug builds catch this;
        // release tolerates (no WARN side-effect because attempted_distinct is
        // unchanged by this call).
        let mut v = VerifierState::new(3);
        v.record_timeout(99); // 99 was never an attempt
    }
}
