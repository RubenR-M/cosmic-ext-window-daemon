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

#![allow(dead_code)]

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
    /// Identity of the group this workspace belongs to:
    /// `group.handle.id().protocol_id() as u64`.
    pub group_id: u64,
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
/// Trigger filtering happens in `runtime::handle_empty_workspace_if_triggered`.
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

    fn make_meta(id: WorkspaceId, coordinates: Vec<u32>, group_id: u64) -> WorkspaceMeta {
        WorkspaceMeta { id, coordinates, group_id }
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
            make_meta(1, vec![0], 10),
            make_meta(2, vec![1], 10),
            make_meta(3, vec![2], 10),
            make_meta(5, vec![4], 10),
        ];
        let occ = occupied(&[1, 2, 3]);
        let rec = recent(&[3, 2, 1]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(3));
    }

    #[test]
    fn select_jump_target_skips_unoccupied_mru_entries() {
        // deque = [3, 2, 1]; occupied = {1} only; expects 1 (skip 3 and 2).
        let metas = vec![
            make_meta(1, vec![0], 10),
            make_meta(2, vec![1], 10),
            make_meta(3, vec![2], 10),
            make_meta(5, vec![4], 10),
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
            make_meta(1, vec![0], 10),
            make_meta(2, vec![1], 10),
        ];
        let occ = occupied(&[1, 2, 99, 100]);
        let rec = recent(&[99, 100, 1]);
        assert_eq!(select_jump_target(5, &metas, &occ, &rec), Some(1));
    }

    #[test]
    fn select_jump_target_skips_current_in_mru() {
        // deque = [5, 2]; current = 5; expects 2 (skip current).
        let metas = vec![
            make_meta(2, vec![1], 10),
            make_meta(5, vec![4], 10),
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
        // Expects ws1 (lowest coord).
        let metas = vec![
            make_meta(1, vec![0], 10),
            make_meta(5, vec![4], 10),
            make_meta(3, vec![2], 10),
        ];
        let occ = occupied(&[1, 5]);
        let rec = recent(&[]);
        assert_eq!(select_jump_target(3, &metas, &occ, &rec), Some(1));
    }

    #[test]
    fn select_jump_target_fallback_tiebreaks_on_protocol_id() {
        // Two workspaces share coordinates [0]; lower id wins.
        let metas = vec![
            make_meta(10, vec![0], 20),
            make_meta(7, vec![0], 20),
            make_meta(99, vec![2], 20),
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
        let metas = vec![make_meta(1, vec![0], 10)];
        let occ = occupied(&[]);
        let rec = recent(&[]);
        assert_eq!(select_jump_target(1, &metas, &occ, &rec), None);
    }

    #[test]
    fn select_jump_target_returns_none_when_fallback_is_current() {
        // Group = {current=1, ws2}; ws2 not occupied; current is only candidate → None.
        let metas = vec![
            make_meta(1, vec![0], 10),
            make_meta(2, vec![1], 10),
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
}
