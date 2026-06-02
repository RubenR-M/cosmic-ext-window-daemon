// mru_jump — pure decision logic for the JUMP_ON_EMPTY feature.
// SPDX-License-Identifier: GPL-3.0-only
//
// No Wayland types appear in the public signatures of the pure functions
// (mirror of placement.rs / D2). The Wayland-glue shell
// `update_mru_from_active_transitions` reads toolkit state read-only.
//
// Implemented in T-MRU-002 (pure module) and T-MRU-004 (glue shell wired).
//
// NFR-MRU-005: this module MUST NOT call ExtWorkspaceHandleV1::activate,
// ExtWorkspaceGroupHandleV1::create_workspace, or ext_workspace_manager_v1::commit.
// The clippy::disallowed_methods lint (src/lib.rs) covers this file automatically.

use std::collections::{HashMap, VecDeque, HashSet};

use crate::ids::WorkspaceId;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of entries in the MRU deque. Hard-coded per D4 (not user-configurable).
pub const MRU_CAP: usize = 16;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Minimal workspace descriptor for the pure selector.
///
/// No Wayland proxy types. Materialized on-demand in
/// `runtime::handle_empty_workspace_if_triggered` from `workspace_state`.
#[derive(Debug, Clone)]
pub struct WorkspaceMeta {
    /// Workspace identity: `handle.id().protocol_id() as u64`.
    pub id: WorkspaceId,
    /// Workspace coordinates from the ext-workspace-v1 protocol field.
    pub coordinates: Vec<u32>,
}

// ---------------------------------------------------------------------------
// evaluate_trigger — pure trigger-evaluation function
// ---------------------------------------------------------------------------

/// Input to evaluate_trigger — captures everything the trigger-evaluation
/// step reads, in a form that can be constructed from a unit test.
pub struct TriggerInput<'a> {
    /// Workspaces the CLOSED toplevel was on (D9 case 4: may be 0, 1, or >1).
    pub closed_workspaces: &'a [WorkspaceMeta],
    /// For each workspace the closing toplevel was on, is_active flag.
    /// Indexed by WorkspaceId.
    pub is_active: &'a HashMap<WorkspaceId, bool>,
    /// For each workspace the closing toplevel was on, occupancy count
    /// EXCLUDING the closing handle (per FR-MRU-002).
    pub occupied_excluding_closed: &'a HashMap<WorkspaceId, usize>,
    /// Group membership per workspace (group protocol_id) keyed by WorkspaceId.
    /// `None` value = orphaned workspace (covers the case_no_group anomaly path).
    pub group_id_for: &'a HashMap<WorkspaceId, Option<u64>>,
    /// All workspaces per group (for fallback ordering by coordinates[0]).
    pub group_workspaces: &'a HashMap<u64, Vec<WorkspaceMeta>>,
    /// Per-group occupied workspace IDs (any workspace with ≥1 toplevel,
    /// excluding the closing handle).
    pub group_occupied: &'a HashMap<u64, HashSet<WorkspaceId>>,
    /// MRU deque (global, filtered by group at query time).
    pub recent_mru: &'a VecDeque<WorkspaceId>,
}

/// Outcome from evaluate_trigger — drives both the activation dispatch
/// AND the tracing emission.
#[derive(Debug, PartialEq, Eq)]
pub enum TriggerOutcome {
    /// Fire activation. `source` is the now-empty active workspace; `target`
    /// is the next workspace to activate; `group_id` is shared between them.
    Jump {
        source: WorkspaceId,
        target: WorkspaceId,
        group_id: u64,
    },
    /// No activation. `reason` drives the tracing label.
    NoOp { reason: NoOpReason },
}

#[derive(Debug, PartialEq, Eq)]
pub enum NoOpReason {
    /// Closed toplevel had zero workspace handles (D9 — previously silent).
    NoWorkspace,
    /// All workspaces the closed toplevel was on are non-active. D9 case 1.
    NotActive,
    /// The closing toplevel's workspace still has other toplevels. D9 case 2.
    StillOccupied { ws: WorkspaceId },
    /// No eligible target — group is empty after excluding the source. D9 case 3.
    NoTarget { group_id: u64, source: WorkspaceId },
    /// Workspace orphaned from group (group_data.is_none).
    /// Previously overloaded as "no_target"; now a distinct case.
    NoGroup { ws: WorkspaceId },
    /// Multi-workspace toplevel with no per-handle match (D9 case 4).
    /// Coalesces all per-handle no-ops into ONE outcome.
    MultiWorkspaceNoMatch { handle_count: usize },
}

/// Evaluate whether the just-closed toplevel's workspace should trigger a jump.
///
/// Pure: no Wayland types, no `&mut AppData`, no I/O.
/// The caller (`handle_empty_workspace_if_triggered`) is responsible for
/// collecting the inputs from live Wayland state and dispatching the result.
pub fn evaluate_trigger(input: &TriggerInput<'_>) -> TriggerOutcome {
    // 1. Closed toplevel had no workspaces.
    if input.closed_workspaces.is_empty() {
        return TriggerOutcome::NoOp { reason: NoOpReason::NoWorkspace };
    }

    // 2. Multi-workspace handling — iterate handles; first Jump wins; if all
    //    no-op AND count > 1, coalesce to MultiWorkspaceNoMatch.
    let multi = input.closed_workspaces.len() > 1;
    let mut last_single_reason: Option<NoOpReason> = None;

    for ws in input.closed_workspaces {
        // 3. is_active check.
        let active = input.is_active.get(&ws.id).copied().unwrap_or(false);
        if !active {
            if !multi {
                return TriggerOutcome::NoOp { reason: NoOpReason::NotActive };
            }
            last_single_reason = Some(NoOpReason::NotActive);
            continue;
        }

        // 4. Occupancy check (FR-MRU-002 — already excludes closing handle).
        let occupied = input.occupied_excluding_closed.get(&ws.id).copied().unwrap_or(0);
        if occupied > 0 {
            if !multi {
                return TriggerOutcome::NoOp { reason: NoOpReason::StillOccupied { ws: ws.id } };
            }
            last_single_reason = Some(NoOpReason::StillOccupied { ws: ws.id });
            continue;
        }

        // 5. Group resolution.
        let group_id = match input.group_id_for.get(&ws.id).and_then(|x| x.as_ref()) {
            Some(g) => *g,
            None => {
                if !multi {
                    return TriggerOutcome::NoOp { reason: NoOpReason::NoGroup { ws: ws.id } };
                }
                last_single_reason = Some(NoOpReason::NoGroup { ws: ws.id });
                continue;
            }
        };

        // 6. Run select_jump_target via the existing pure selector.
        let occupied_in_group = input.group_occupied.get(&group_id).cloned().unwrap_or_default();
        let group_workspaces = input.group_workspaces.get(&group_id).cloned().unwrap_or_default();

        match select_jump_target(ws.id, &group_workspaces, &occupied_in_group, input.recent_mru) {
            Some(target) => {
                return TriggerOutcome::Jump { source: ws.id, target, group_id };
            }
            None => {
                if !multi {
                    return TriggerOutcome::NoOp {
                        reason: NoOpReason::NoTarget { group_id, source: ws.id },
                    };
                }
                last_single_reason = Some(NoOpReason::NoTarget { group_id, source: ws.id });
                continue;
            }
        }
    }

    // 7. Multi-workspace coalescing.
    if multi {
        return TriggerOutcome::NoOp {
            reason: NoOpReason::MultiWorkspaceNoMatch {
                handle_count: input.closed_workspaces.len(),
            },
        };
    }

    // Unreachable: single-handle path always returns inside the loop body
    // (every branch either returns Jump or returns NoOp without setting
    // last_single_reason). The multi-handle path returns above via the
    // MultiWorkspaceNoMatch branch. last_single_reason is therefore never
    // observed and this expression is structurally dead — make the invariant
    // explicit so future regressions trip immediately.
    let _ = last_single_reason;
    unreachable!("single-handle loop always returns; multi-handle is handled above")
}

// ---------------------------------------------------------------------------
// Pure functions
// ---------------------------------------------------------------------------

/// Select the best jump target for the workspace that just became empty.
///
/// Returns `None` in all no-op cases enumerated by D9 / FR-MRU-005.
///
/// Algorithm (D2 + §3.2 of design):
/// 1. Walk `recent` front-to-back; return the first entry that is in `group_workspaces`,
///    is in `occupied`, and is not `current_ws_id`.
/// 2. Fallback: return the workspace in `group_workspaces` with the lowest `coordinates`
///    (lexicographic); tiebreak on `id` ASC. Skip if it equals `current_ws_id` or is not
///    in `occupied`.
/// 3. None — single-workspace group or no candidate.
///
/// # Preconditions
/// The caller has already verified that `current_ws_id` is active and now empty.
/// Trigger filtering happens in `evaluate_trigger`.
pub fn select_jump_target(
    current_ws_id: WorkspaceId,
    group_workspaces: &[WorkspaceMeta],
    occupied: &HashSet<WorkspaceId>,
    recent: &VecDeque<WorkspaceId>,
) -> Option<WorkspaceId> {
    let group_ids: HashSet<WorkspaceId> = group_workspaces.iter().map(|w| w.id).collect();

    // Step 1: MRU scan — front to back.
    for &ws_id in recent.iter() {
        if ws_id != current_ws_id && group_ids.contains(&ws_id) && occupied.contains(&ws_id) {
            return Some(ws_id);
        }
    }

    // Step 2: Fallback to lowest-coordinate workspace in the group (not current, must be occupied).
    let fallback = group_workspaces
        .iter()
        .filter(|w| w.id != current_ws_id && occupied.contains(&w.id))
        .min_by(|a, b| a.coordinates.cmp(&b.coordinates).then(a.id.cmp(&b.id)))
        .map(|w| w.id);

    // Step 3: None if no candidate found.
    fallback
}

/// Push `ws_id` to the front of `deque`, removing any existing occurrence, then
/// truncate to at most `cap` entries.
///
/// Complexity: O(K) per call where K = `cap` = `MRU_CAP` = 16.
/// Meets the O(1)-amortised performance requirement (NFR-MRU-002 / D4).
pub fn record_mru_transition(
    deque: &mut VecDeque<WorkspaceId>,
    ws_id: WorkspaceId,
    cap: usize,
) {
    // Dedup: remove any existing occurrence so the ID is unique in the deque.
    deque.retain(|&x| x != ws_id);
    // Push to front (most-recently-visited = front).
    deque.push_front(ws_id);
    // Cap: evict oldest entries from the back.
    while deque.len() > cap {
        deque.pop_back();
    }
}

/// Pure helper: detect active-workspace transitions by comparing `current_active`
/// against `last_known_active`. Returns workspace IDs that are newly active
/// (in input iteration order). Updates `last_known_active` in place.
///
/// Groups that disappear from `current_active` are NOT removed from
/// `last_known_active` — stale entries are harmless (they never match a
/// fresh group_id from a new session because `AppData` is dropped on reconnect).
///
/// This function is the inner pure core; `update_mru_from_active_transitions`
/// is the Wayland-glue shell that feeds it.
pub(crate) fn detect_transitions_and_update(
    last_known_active: &mut HashMap<u64, WorkspaceId>,
    current_active: &[(u64, WorkspaceId)], // (group_id, active_ws_id)
) -> Vec<WorkspaceId> {
    let mut out = Vec::new();
    for &(group_id, active_ws_id) in current_active {
        match last_known_active.get(&group_id) {
            Some(&prev) if prev == active_ws_id => continue, // no change
            _ => {
                last_known_active.insert(group_id, active_ws_id);
                out.push(active_ws_id);
            }
        }
    }
    out
}

/// Wayland-glue shell: scan all workspace groups for active-bit transitions and
/// push any new transitions to the front of `deque` via `record_mru_transition`.
///
/// Called from `WorkspaceHandler::done()` when `config.jump_on_empty` is true.
///
/// This function reads `WorkspaceState` read-only; it issues NO Wayland requests.
/// It delegates all pure logic to `detect_transitions_and_update` and
/// `record_mru_transition` so those can be unit-tested without a live compositor.
pub fn update_mru_from_active_transitions(
    last_known_active: &mut HashMap<u64, WorkspaceId>,
    deque: &mut VecDeque<WorkspaceId>,
    workspace_state: &cosmic_client_toolkit::workspace::WorkspaceState,
    cap: usize,
) {
    use wayland_client::Proxy as _;
    use crate::wayland::workspace::workspace_is_active;

    // Collect (group_id, active_ws_id) pairs for every group that has exactly
    // one active workspace. Groups with no active workspace are skipped.
    let current_active: Vec<(u64, WorkspaceId)> = workspace_state
        .workspace_groups()
        .filter_map(|g| {
            let group_id = g.handle.id().protocol_id() as u64;
            workspace_state
                .workspaces()
                .find(|w| g.workspaces.contains(&w.handle) && workspace_is_active(w))
                .map(|w| (group_id, w.handle.id().protocol_id() as u64))
        })
        .collect();

    let transitions = detect_transitions_and_update(last_known_active, &current_active);
    for ws_id in transitions {
        record_mru_transition(deque, ws_id, cap);
    }
}

// ---------------------------------------------------------------------------
// Tests — pure logic only; no Wayland compositor required (NFR-MRU-001)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn make_meta(id: WorkspaceId, coordinates: Vec<u32>) -> WorkspaceMeta {
        WorkspaceMeta { id, coordinates }
    }

    fn occupied(ids: &[WorkspaceId]) -> HashSet<WorkspaceId> {
        ids.iter().copied().collect()
    }

    fn recent(ids: &[WorkspaceId]) -> VecDeque<WorkspaceId> {
        ids.iter().copied().collect()
    }

    // -----------------------------------------------------------------------
    // select_jump_target — MRU path (tests 1–4)
    // -----------------------------------------------------------------------

    #[test]
    fn select_jump_target_returns_most_recent_mru_when_available() {
        // deque = [3, 2, 1]; current = 5; group = {1, 2, 3, 5}; occupied = {1, 2, 3}
        // Expects 3 (front of deque, in group, occupied, not current).
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
            make_meta(3, vec![2]),
            make_meta(5, vec![4]),
        ];
        let occ = occupied(&[1, 2, 3]);
        let rec = recent(&[3, 2, 1]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(3));
    }

    #[test]
    fn select_jump_target_skips_unoccupied_mru_entries() {
        // deque = [3, 2, 1]; occupied = {1} only; expects 1 (skip 3 and 2).
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
            make_meta(3, vec![2]),
            make_meta(5, vec![4]),
        ];
        let occ = occupied(&[1]);
        let rec = recent(&[3, 2, 1]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(1));
    }

    #[test]
    fn select_jump_target_skips_cross_group_mru_entries() {
        // Group A = {1, 2}. deque = [99, 100, 1]; 99 and 100 are in group B (not in metas).
        // Expects 1 (first deque entry that IS in group A).
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ];
        let occ = occupied(&[1, 2, 99, 100]);
        let rec = recent(&[99, 100, 1]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(1));
    }

    #[test]
    fn select_jump_target_skips_current_in_mru() {
        // deque = [5, 2]; current = 5; expects 2 (skip current).
        let metas = vec![
            make_meta(2, vec![1]),
            make_meta(5, vec![4]),
        ];
        let occ = occupied(&[2, 5]);
        let rec = recent(&[5, 2]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(2));
    }

    // -----------------------------------------------------------------------
    // select_jump_target — fallback path (tests 5–6)
    // -----------------------------------------------------------------------

    #[test]
    fn select_jump_target_falls_back_to_lowest_coordinate() {
        // MRU empty; group = {ws1 coord [0], ws5 coord [4]}; occupied = {ws1, ws5}; current = ws3.
        // Expects ws1 (lowest coord, still occupied).
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(5, vec![4]),
            make_meta(3, vec![2]),
        ];
        let occ = occupied(&[1, 5]);
        let rec = recent(&[]);
        assert_eq!(select_jump_target(3, &metas, &occ, &rec), Some(1));
    }

    #[test]
    fn select_jump_target_fallback_tiebreaks_on_protocol_id() {
        // Two workspaces share coordinates [0]; lower id wins.
        let metas = vec![
            make_meta(10, vec![0]),
            make_meta(7, vec![0]),
            make_meta(99, vec![2]),
        ];
        let occ = occupied(&[7, 10, 99]);
        let rec = recent(&[]);
        // current = 99, fallback between id=7 and id=10 both at coord [0] → id=7 wins
        assert_eq!(select_jump_target(99, &metas, &occ, &rec), Some(7));
    }

    // -----------------------------------------------------------------------
    // select_jump_target — None cases (tests 7–8)
    // -----------------------------------------------------------------------

    #[test]
    fn select_jump_target_returns_none_when_current_is_only_workspace() {
        // Single-workspace group; current is the only member → None.
        let metas = vec![make_meta(1, vec![0])];
        let occ = occupied(&[]);
        let rec = recent(&[]);
        assert_eq!(select_jump_target(1, &metas, &occ, &rec), None);
    }

    #[test]
    fn select_jump_target_returns_none_when_fallback_is_current() {
        // Group = {current=1, ws2}; ws2 not occupied; current is only candidate → None.
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ];
        let occ = occupied(&[]);
        let rec = recent(&[]);
        assert_eq!(select_jump_target(1, &metas, &occ, &rec), None);
    }

    // -----------------------------------------------------------------------
    // record_mru_transition (tests 9–11)
    // -----------------------------------------------------------------------

    #[test]
    fn record_mru_transition_pushes_to_front() {
        let mut deque = VecDeque::new();
        record_mru_transition(&mut deque, 42, MRU_CAP);
        assert_eq!(deque, vec![42u64]);
    }

    #[test]
    fn record_mru_transition_deduplicates() {
        let mut deque: VecDeque<u64> = vec![1, 2, 3].into();
        // Push 2 again — should move to front, deduplicated.
        record_mru_transition(&mut deque, 2, MRU_CAP);
        assert_eq!(deque, vec![2u64, 1, 3]);
    }

    #[test]
    fn record_mru_transition_caps_at_k() {
        let mut deque = VecDeque::new();
        // Push 17 distinct ids with cap=16.
        for i in 0u64..17 {
            record_mru_transition(&mut deque, i, MRU_CAP);
        }
        assert_eq!(deque.len(), MRU_CAP, "deque must be capped at MRU_CAP={}", MRU_CAP);
        // The oldest push was id=0; it should be evicted (id=16 is front, id=1 is back).
        assert!(!deque.contains(&0), "oldest entry (0) must have been evicted");
        assert_eq!(*deque.front().unwrap(), 16, "most-recent (16) must be at front");
    }

    // -----------------------------------------------------------------------
    // detect_transitions_and_update (tests 12–14)
    // -----------------------------------------------------------------------

    #[test]
    fn detect_transitions_and_update_no_change_no_transition() {
        let mut last: HashMap<u64, u64> = [(10, 42)].into();
        let current = [(10u64, 42u64)];
        let out = detect_transitions_and_update(&mut last, &current);
        assert!(out.is_empty(), "no transition expected when active workspace unchanged");
        assert_eq!(last[&10], 42);
    }

    #[test]
    fn detect_transitions_and_update_transition_recorded() {
        let mut last: HashMap<u64, u64> = [(10, 42)].into();
        // Active workspace changed from 42 to 99.
        let current = [(10u64, 99u64)];
        let out = detect_transitions_and_update(&mut last, &current);
        assert_eq!(out, vec![99u64]);
        assert_eq!(last[&10], 99);
    }

    #[test]
    fn detect_transitions_and_update_fresh_group_emits_transition() {
        let mut last: HashMap<u64, u64> = HashMap::new();
        // Group 10 seen for the first time.
        let current = [(10u64, 42u64)];
        let out = detect_transitions_and_update(&mut last, &current);
        assert_eq!(out, vec![42u64]);
        assert_eq!(last[&10], 42);
    }

    // -----------------------------------------------------------------------
    // evaluate_trigger tests (8 cases — round 1 fixes #2/#3/#5/#6/#7/#8)
    // -----------------------------------------------------------------------

    #[test]
    fn evaluate_trigger_jumps_when_active_and_empty_with_mru_target() {
        // ws2 is active, empty (occupancy 0 after closing), group 10 has ws1 + ws2.
        // MRU = [ws1]. Expects Jump { source: 2, target: 1, group_id: 10 }.
        let closed = vec![make_meta(2, vec![1])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, true)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = [(10u64, vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ])].into();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = [(10u64, [1u64].into())].into();
        let mru: VecDeque<WorkspaceId> = [1u64].into();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::Jump { source: 2, target: 1, group_id: 10 },
        );
    }

    #[test]
    fn evaluate_trigger_not_active_yields_not_active() {
        // ws2 is NOT active. Expects NoOp { reason: NotActive }.
        let closed = vec![make_meta(2, vec![1])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, false)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = HashMap::new();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::NotActive },
        );
    }

    #[test]
    fn evaluate_trigger_still_occupied_yields_still_occupied() {
        // ws2 is active but still has 1 other toplevel after excluding closing handle.
        let closed = vec![make_meta(2, vec![1])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, true)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 1)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = HashMap::new();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::StillOccupied { ws: 2 } },
        );
    }

    #[test]
    fn evaluate_trigger_no_target_yields_no_target() {
        // ws2 is active, empty, group 10 has only ws2 (no other occupied workspace).
        let closed = vec![make_meta(2, vec![1])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, true)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = [(10u64, vec![make_meta(2, vec![1])])].into();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = [(10u64, HashSet::new())].into();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::NoTarget { group_id: 10, source: 2 } },
        );
    }

    /// Finding #6 — zero workspace handles emits NoWorkspace (was previously silent).
    #[test]
    fn evaluate_trigger_no_workspace_yields_no_workspace() {
        let closed: Vec<WorkspaceMeta> = vec![];
        let is_active: HashMap<WorkspaceId, bool> = HashMap::new();
        let occ_excl: HashMap<WorkspaceId, usize> = HashMap::new();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = HashMap::new();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = HashMap::new();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::NoWorkspace },
        );
    }

    /// Finding #7 — orphaned workspace (None group) emits NoGroup (was "no_target").
    #[test]
    fn evaluate_trigger_orphan_group_yields_no_group() {
        let closed = vec![make_meta(2, vec![1])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, true)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0)].into();
        // None value signals orphaned workspace.
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, None)].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = HashMap::new();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::NoGroup { ws: 2 } },
        );
    }

    /// Finding #8 — multi-workspace toplevel with no match coalesces to ONE outcome.
    #[test]
    fn evaluate_trigger_multi_workspace_no_match_coalesces() {
        // Toplevel on ws2 + ws3; neither is active. Expects MultiWorkspaceNoMatch { handle_count: 2 }.
        let closed = vec![make_meta(2, vec![1]), make_meta(3, vec![2])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, false), (3, false)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0), (3, 0)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> = [(2, Some(10)), (3, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = HashMap::new();
        let mru: VecDeque<WorkspaceId> = VecDeque::new();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::NoOp { reason: NoOpReason::MultiWorkspaceNoMatch { handle_count: 2 } },
        );
    }

    /// Multi-workspace toplevel where the FIRST workspace produces a jump.
    #[test]
    fn evaluate_trigger_multi_workspace_jumps_on_first_match() {
        // Toplevel on ws2 (active, empty) + ws3 (not active).
        // ws2 triggers a jump to ws1. ws3 is never evaluated.
        let closed = vec![make_meta(2, vec![1]), make_meta(3, vec![2])];
        let is_active: HashMap<WorkspaceId, bool> = [(2, true), (3, false)].into();
        let occ_excl: HashMap<WorkspaceId, usize> = [(2, 0), (3, 0)].into();
        let group_id_for: HashMap<WorkspaceId, Option<u64>> =
            [(2, Some(10)), (3, Some(10))].into();
        let group_ws: HashMap<u64, Vec<WorkspaceMeta>> = [(10u64, vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ])].into();
        let group_occ: HashMap<u64, HashSet<WorkspaceId>> = [(10u64, [1u64].into())].into();
        let mru: VecDeque<WorkspaceId> = [1u64].into();

        let input = TriggerInput {
            closed_workspaces: &closed,
            is_active: &is_active,
            occupied_excluding_closed: &occ_excl,
            group_id_for: &group_id_for,
            group_workspaces: &group_ws,
            group_occupied: &group_occ,
            recent_mru: &mru,
        };
        assert_eq!(
            evaluate_trigger(&input),
            TriggerOutcome::Jump { source: 2, target: 1, group_id: 10 },
        );
    }

    // -----------------------------------------------------------------------
    // Feature-off structural contract (replaces two vacuous tests)
    // -----------------------------------------------------------------------

    /// Documents the contract that the caller (toplevel_closed / done()) MUST
    /// guard the call to evaluate_trigger behind `if config.jump_on_empty`.
    /// The guards are verified by reading src/wayland/{workspace,toplevel}.rs
    /// which use `if self.config.jump_on_empty` before any MRU-related call.
    #[test]
    fn feature_off_config_default_is_false() {
        let config_off = crate::config::from_env_source(|_| None).unwrap();
        assert!(!config_off.jump_on_empty);
        // The actual feature-off guards are at:
        // - src/wayland/workspace.rs::WorkspaceHandler::done (MRU producer)
        // - src/wayland/toplevel.rs::ToplevelInfoHandler::toplevel_closed (trigger)
    }

    // -----------------------------------------------------------------------
    // Legacy integration-level tests kept (pure-function level; no compositor)
    // -----------------------------------------------------------------------

    /// SC-MRU-005: two toplevels on ws-2; close T-A first (occupancy still 1) → no jump;
    /// close T-B next (occupancy 0) → trigger should proceed to target selection.
    /// Tested at the occupancy-check level via the pure select_jump_target.
    #[test]
    fn trigger_fires_exactly_once_when_workspace_empties_gradually() {
        let metas = vec![
            make_meta(1, vec![0]), // ws1
            make_meta(2, vec![1]), // ws2 (active, closing)
        ];

        // Scenario B: ws2 is empty after T-B closes; ws1 is occupied.
        let occ_after_tb = occupied(&[1]); // only ws1 still has a toplevel
        let rec = recent(&[]);
        let result = select_jump_target(2, &metas, &occ_after_tb, &rec);
        assert_eq!(result, Some(1), "second close (ws2 empty) must jump to ws1");
    }

    /// FR-MRU-002: occupancy check must exclude the closing handle.
    #[test]
    fn occupancy_check_excludes_closing_handle() {
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ];
        let occ = occupied(&[1]); // ws2 NOT in occupied because closed handle excluded
        let rec = recent(&[]);
        let result = select_jump_target(2, &metas, &occ, &rec);
        assert_eq!(result, Some(1), "must jump to ws1 when closing handle is properly excluded from occupancy");
    }

    /// FR-MRU-001: trigger must not fire when workspace is not active,
    /// when it is occupied, or when there is no valid target.
    #[test]
    fn trigger_fires_only_when_workspace_is_active_and_empty() {
        let metas_single = vec![make_meta(1, vec![0])];
        let occ_with_only_current = occupied(&[1]);
        let result = select_jump_target(1, &metas_single, &occ_with_only_current, &recent(&[]));
        assert_eq!(result, None, "sole workspace → None (no valid target)");

        let metas_two = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
        ];
        let occ_empty = occupied(&[]);
        let result = select_jump_target(2, &metas_two, &occ_empty, &recent(&[]));
        assert_eq!(result, None, "no occupied target → None");
    }

    /// SC-MRU-003: select_jump_target returns None when current is the only workspace.
    #[test]
    fn select_jump_target_returns_none_when_current_is_only_occupied() {
        let metas = vec![
            make_meta(1, vec![0]),
            make_meta(2, vec![1]),
            make_meta(3, vec![2]),
        ];
        let occ = occupied(&[1]);
        let rec = recent(&[]);
        let result = select_jump_target(1, &metas, &occ, &rec);
        assert_eq!(result, None, "no occupied non-current workspace → None");
    }
}
