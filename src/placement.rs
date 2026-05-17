// placement — policy engine: (Config, ToplevelInfoStub, WorkspaceStateStub) -> PlacementAction.
// SPDX-License-Identifier: GPL-3.0-only
//
// Pure decision function; no Wayland I/O or side effects.
// Stand-in types capture only the fields the policy needs.
// Real types are wired in Phase 3 (T-017 area) via thin From conversions.
//
// Implemented in T-006 (Phase 1).

#![allow(dead_code)]

use std::collections::HashSet;

use crate::config::{Config, WorkspaceMode};
use crate::ids::WorkspaceId;

// ---------------------------------------------------------------------------
// Public stand-in types (replaces real Wayland types for pure-logic tests)
// ---------------------------------------------------------------------------

// WorkspaceId is hoisted to `crate::ids` so Phase 3 can swap its underlying
// representation in one place. See src/ids.rs.

/// Minimal snapshot of a toplevel window — fields the policy uses.
#[derive(Debug, Clone)]
pub struct ToplevelInfoStub {
    pub app_id: String,
    pub title: String,
    /// Output IDs the toplevel is currently on (empty → FR-010 skip).
    pub output_ids: Vec<u64>,
    /// Whether the COSMIC-specific toplevel handle is present (FR-009 guard).
    pub cosmic_toplevel_present: bool,
}

/// Minimal snapshot of workspace state — fields the policy uses.
#[derive(Debug, Clone)]
pub struct WorkspaceStateStub {
    /// Groups, each mapping to its output IDs and its workspace IDs (with occupancy).
    pub groups: Vec<WorkspaceGroupStub>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceGroupStub {
    pub id: u64,
    /// Output IDs associated with this group.
    pub output_ids: Vec<u64>,
    /// Workspaces in this group.
    pub workspaces: Vec<WorkspaceStub>,
    /// Whether create_workspace capability is available.
    pub can_create_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceStub {
    pub id: WorkspaceId,
    /// Toplevel IDs currently on this workspace.
    pub toplevel_ids: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Action types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementAction {
    Skip { reason: SkipReason },
    Place { workspace: WorkspaceTarget, then: PostPlaceActions },
}

/// Where to place a toplevel within the selected workspace group.
///
/// `Existing` carries the WorkspaceId of an extant workspace. `Create` means
/// the caller (Phase 3) must invoke `create_workspace` on the group manager.
/// The enum makes the two cases distinct in the type system so a consumer
/// CANNOT silently treat "create a new workspace" as if it were an existing
/// workspace handle. Avoids the D9/D15-class brittleness of an integer sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceTarget {
    Existing(WorkspaceId),
    Create,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostPlaceActions {
    pub switch_to: bool,
    pub maximize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    AlreadyHandled,
    ExcludedByAppId,
    ExcludedByTitle,
    NoCosmicToplevel,
    NoOutputs,
    NoMatchingGroup,
    WorkspaceModeSame,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Pure decision function: given config and world state, return a placement action.
///
/// `handled` is the daemon's in-memory set of toplevel IDs already processed (FR-005).
pub fn decide(
    config: &Config,
    info: &ToplevelInfoStub,
    workspaces: &WorkspaceStateStub,
    handled: &HashSet<u64>,
    toplevel_id: u64,
) -> PlacementAction {
    // FR-005 — idempotency
    if handled.contains(&toplevel_id) {
        return PlacementAction::Skip { reason: SkipReason::AlreadyHandled };
    }

    // FR-007 — exclusion by app_id
    if config.excluded_app_ids.iter().any(|id| id == &info.app_id) {
        return PlacementAction::Skip { reason: SkipReason::ExcludedByAppId };
    }

    // FR-008 — exclusion by title regex
    if let Some(re) = &config.excluded_title_regex {
        if re.is_match(&info.title) {
            return PlacementAction::Skip { reason: SkipReason::ExcludedByTitle };
        }
    }

    // FR-009 — cosmic_toplevel=None guard
    if !info.cosmic_toplevel_present {
        return PlacementAction::Skip { reason: SkipReason::NoCosmicToplevel };
    }

    // FR-010 — empty outputs guard
    if info.output_ids.is_empty() {
        return PlacementAction::Skip { reason: SkipReason::NoOutputs };
    }

    // FR-015 — WORKSPACE_MODE=same: no move
    if config.workspace_mode == WorkspaceMode::Same {
        return PlacementAction::Skip { reason: SkipReason::WorkspaceModeSame };
    }

    // FR-011 / FR-012 — select target group
    let target_group = select_group(config, info, workspaces);
    let group = match target_group {
        Some(g) => g,
        None => return PlacementAction::Skip { reason: SkipReason::NoMatchingGroup },
    };

    // FR-013 / FR-014 — select target workspace within group
    let target = match select_workspace(config, group) {
        Some(t) => t,
        None => return PlacementAction::Skip { reason: SkipReason::NoMatchingGroup },
    };

    PlacementAction::Place {
        workspace: target,
        then: PostPlaceActions {
            switch_to: config.switch_to_workspace,
            maximize: config.maximize,
        },
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Select the workspace group based on config (WORKSPACE_OUTPUT override or per-toplevel).
fn select_group<'a>(
    config: &Config,
    info: &ToplevelInfoStub,
    workspaces: &'a WorkspaceStateStub,
) -> Option<&'a WorkspaceGroupStub> {
    if let Some(ref output_name) = config.workspace_output {
        // FR-012: WORKSPACE_OUTPUT override. In pure-logic layer we compare by name stored
        // as a string. The stub doesn't carry output names, so we model the override
        // as an output ID stored as u64 parsed from the string (or a special sentinel).
        // For the pure-logic layer, we treat workspace_output as an output ID if it parses
        // as u64; otherwise fall back to per-toplevel. This matches what Phase 3 wiring
        // will do via the named-output lookup. Tests set WORKSPACE_OUTPUT to a parseable ID.
        if let Ok(override_output_id) = output_name.parse::<u64>() {
            let found = workspaces
                .groups
                .iter()
                .find(|g| g.output_ids.contains(&override_output_id));
            if found.is_some() {
                return found;
            }
            // Fall through to per-toplevel selection (the caller will WARN-once; that
            // is state kept in AppData.one_shot, not in this pure function).
        }
    }

    // FR-011: per-toplevel output selection — pick the group that contains any of the
    // toplevel's birth outputs; among multiple matches, pick deterministically by group id.
    let mut candidates: Vec<&WorkspaceGroupStub> = workspaces
        .groups
        .iter()
        .filter(|g| g.output_ids.iter().any(|oid| info.output_ids.contains(oid)))
        .collect();

    candidates.sort_by_key(|g| g.id);
    candidates.into_iter().next()
}

/// Select a workspace within a group per WorkspaceMode.
fn select_workspace(config: &Config, group: &WorkspaceGroupStub) -> Option<WorkspaceTarget> {
    match config.workspace_mode {
        WorkspaceMode::Same => unreachable!("Same mode is handled before group selection"),
        WorkspaceMode::NextFree => {
            // FR-013: first workspace with no toplevels
            group
                .workspaces
                .iter()
                .find(|w| w.toplevel_ids.is_empty())
                .map(|w| WorkspaceTarget::Existing(w.id))
        }
        WorkspaceMode::NewEach => {
            // FR-014: WorkspaceTarget::Create signals the caller (Phase 3) to invoke
            // create_workspace on the group manager. If can_create_workspace is false,
            // fall back to NextFree semantics — the WARN-once is emitted by the caller,
            // not by this pure function.
            if group.can_create_workspace {
                Some(WorkspaceTarget::Create)
            } else {
                // Degradation: fall back to next-free
                group
                    .workspaces
                    .iter()
                    .find(|w| w.toplevel_ids.is_empty())
                    .map(|w| WorkspaceTarget::Existing(w.id))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Build a Config with defaults plus any overrides.
    fn default_config() -> Config {
        crate::config::from_env_source(|_| None).unwrap()
    }

    fn config_with_mode(mode: &str) -> Config {
        let map: HashMap<&str, &str> = [("WORKSPACE_MODE", mode)].into_iter().collect();
        crate::config::from_env_source(|key| map.get(key).map(|v| v.to_string())).unwrap()
    }

    fn config_with(pairs: &[(&str, &str)]) -> Config {
        let map: HashMap<&str, &str> = pairs.iter().copied().collect();
        crate::config::from_env_source(|key| map.get(key).map(|v| v.to_string())).unwrap()
    }

    /// A vanilla ToplevelInfoStub that won't be excluded.
    fn valid_info() -> ToplevelInfoStub {
        ToplevelInfoStub {
            app_id: "org.example.App".to_string(),
            title: "My Window".to_string(),
            output_ids: vec![1],
            cosmic_toplevel_present: true,
        }
    }

    /// A simple workspace state: one group (output 1), two workspaces (one empty, one occupied).
    fn simple_workspaces() -> WorkspaceStateStub {
        WorkspaceStateStub {
            groups: vec![WorkspaceGroupStub {
                id: 10,
                output_ids: vec![1],
                can_create_workspace: true,
                workspaces: vec![
                    WorkspaceStub { id: 100, toplevel_ids: vec![42] }, // occupied
                    WorkspaceStub { id: 101, toplevel_ids: vec![] },   // empty
                ],
            }],
        }
    }

    fn no_handled() -> HashSet<u64> {
        HashSet::new()
    }

    // --- FR-005: AlreadyHandled ---

    #[test]
    fn decide_returns_skip_already_handled_when_toplevel_in_handled_set() {
        let mut handled = HashSet::new();
        handled.insert(99u64);
        let result = decide(&default_config(), &valid_info(), &simple_workspaces(), &handled, 99);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::AlreadyHandled });
    }

    // --- FR-007: ExcludedByAppId ---

    #[test]
    fn decide_returns_skip_excluded_when_app_id_matches() {
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "org.example.App,foot")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::ExcludedByAppId });
    }

    #[test]
    fn decide_does_not_exclude_when_app_id_does_not_match() {
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "foot")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result, PlacementAction::Place { .. }));
    }

    #[test]
    fn decide_exclusion_by_app_id_is_case_sensitive() {
        // "Org.Example.App" should NOT match "org.example.App"
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "Org.Example.App")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result, PlacementAction::Place { .. }));
    }

    // --- FR-008: ExcludedByTitle ---

    #[test]
    fn decide_returns_skip_excluded_when_title_matches_regex() {
        let cfg = config_with(&[("EXCLUDED_TITLE_REGEX", "^Picture-in-Picture")]);
        let mut info = valid_info();
        info.title = "Picture-in-Picture — YouTube".to_string();
        let result = decide(&cfg, &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::ExcludedByTitle });
    }

    #[test]
    fn decide_does_not_exclude_when_title_does_not_match_regex() {
        let cfg = config_with(&[("EXCLUDED_TITLE_REGEX", "^Picture-in-Picture")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result, PlacementAction::Place { .. }));
    }

    // --- FR-009: NoCosmicToplevel ---

    #[test]
    fn decide_returns_skip_no_cosmic_toplevel_when_cosmic_handle_missing() {
        let mut info = valid_info();
        info.cosmic_toplevel_present = false;
        let result = decide(&default_config(), &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::NoCosmicToplevel });
    }

    // --- FR-010: NoOutputs ---

    #[test]
    fn decide_returns_skip_no_outputs_when_toplevel_has_no_outputs() {
        let mut info = valid_info();
        info.output_ids = vec![];
        let result = decide(&default_config(), &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::NoOutputs });
    }

    // --- FR-015: WorkspaceModeSame ---

    #[test]
    fn decide_returns_skip_workspace_mode_same_when_mode_is_same() {
        let cfg = config_with_mode("same");
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::WorkspaceModeSame });
    }

    // --- FR-011: per-toplevel group selection ---

    #[test]
    fn decide_places_on_correct_group_for_output() {
        // Two groups on different outputs; toplevel is on output 2.
        let workspaces = WorkspaceStateStub {
            groups: vec![
                WorkspaceGroupStub {
                    id: 10,
                    output_ids: vec![1],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 100, toplevel_ids: vec![] }],
                },
                WorkspaceGroupStub {
                    id: 20,
                    output_ids: vec![2],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 200, toplevel_ids: vec![] }],
                },
            ],
        };
        let mut info = valid_info();
        info.output_ids = vec![2];
        let result = decide(&default_config(), &info, &workspaces, &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(200),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    #[test]
    fn decide_returns_skip_no_matching_group_when_no_group_for_output() {
        // Group is on output 1, toplevel is on output 99.
        let mut info = valid_info();
        info.output_ids = vec![99];
        let result = decide(&default_config(), &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::NoMatchingGroup });
    }

    // --- FR-013: NextFree ---

    #[test]
    fn decide_places_on_first_empty_workspace_in_next_free_mode() {
        // simple_workspaces: workspace 100 is occupied, 101 is empty → should pick 101.
        let result = decide(&default_config(), &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    #[test]
    fn decide_returns_skip_no_matching_group_when_all_workspaces_occupied_next_free() {
        let workspaces = WorkspaceStateStub {
            groups: vec![WorkspaceGroupStub {
                id: 10,
                output_ids: vec![1],
                can_create_workspace: false,
                workspaces: vec![
                    WorkspaceStub { id: 100, toplevel_ids: vec![1] },
                    WorkspaceStub { id: 101, toplevel_ids: vec![2] },
                ],
            }],
        };
        let result = decide(&default_config(), &valid_info(), &workspaces, &no_handled(), 1);
        assert_eq!(result, PlacementAction::Skip { reason: SkipReason::NoMatchingGroup });
    }

    // --- FR-014: NewEach ---

    #[test]
    fn decide_returns_create_target_in_new_each_mode() {
        let cfg = config_with_mode("new-each");
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        // WorkspaceTarget::Create instructs Phase 3 to invoke create_workspace.
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Create,
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    #[test]
    fn decide_degrades_to_next_free_in_new_each_mode_when_create_workspace_not_supported() {
        let cfg = config_with_mode("new-each");
        let workspaces = WorkspaceStateStub {
            groups: vec![WorkspaceGroupStub {
                id: 10,
                output_ids: vec![1],
                can_create_workspace: false, // capability not set
                workspaces: vec![
                    WorkspaceStub { id: 100, toplevel_ids: vec![1] },
                    WorkspaceStub { id: 101, toplevel_ids: vec![] }, // empty
                ],
            }],
        };
        let result = decide(&cfg, &valid_info(), &workspaces, &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    // --- PostPlaceActions: switch_to and maximize ---

    #[test]
    fn decide_place_carries_switch_to_true_when_configured() {
        let cfg = config_with(&[("SWITCH_TO_WORKSPACE", "1")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: true, maximize: false },
            }
        );
    }

    #[test]
    fn decide_place_carries_maximize_true_when_configured() {
        let cfg = config_with(&[("MAXIMIZE", "1")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: true },
            }
        );
    }

    // --- FR-012: WORKSPACE_OUTPUT override ---

    #[test]
    fn decide_uses_workspace_output_override_when_output_present() {
        // Two groups. Override points to group with output 2.
        let workspaces = WorkspaceStateStub {
            groups: vec![
                WorkspaceGroupStub {
                    id: 10,
                    output_ids: vec![1],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 100, toplevel_ids: vec![] }],
                },
                WorkspaceGroupStub {
                    id: 20,
                    output_ids: vec![2],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 200, toplevel_ids: vec![] }],
                },
            ],
        };
        // Toplevel is on output 1, but WORKSPACE_OUTPUT overrides to output 2.
        let cfg = config_with(&[("WORKSPACE_OUTPUT", "2")]);
        let result = decide(&cfg, &valid_info(), &workspaces, &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(200),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    #[test]
    fn decide_falls_back_to_per_toplevel_when_workspace_output_absent() {
        // Override points to output 99 (not in any group).
        // Toplevel is on output 1 → should fall back to group 10.
        let cfg = config_with(&[("WORKSPACE_OUTPUT", "99")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    // --- group selection determinism ---

    #[test]
    fn decide_selects_group_with_lowest_id_when_multiple_groups_match() {
        // Both groups contain output 1.
        let workspaces = WorkspaceStateStub {
            groups: vec![
                WorkspaceGroupStub {
                    id: 20, // higher id
                    output_ids: vec![1],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 200, toplevel_ids: vec![] }],
                },
                WorkspaceGroupStub {
                    id: 10, // lower id — should be selected
                    output_ids: vec![1],
                    can_create_workspace: true,
                    workspaces: vec![WorkspaceStub { id: 100, toplevel_ids: vec![] }],
                },
            ],
        };
        let result = decide(&default_config(), &valid_info(), &workspaces, &no_handled(), 1);
        assert_eq!(
            result,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(100),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }
}
