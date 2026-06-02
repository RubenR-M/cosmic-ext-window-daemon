// runtime — Wayland-to-pure-logic integration layer.
// SPDX-License-Identifier: GPL-3.0-only
//
// This module is the BRIDGE between the real Wayland world
// (cosmic_client_toolkit::*, wayland_client::*, wayland_protocols::*) and
// the pure-logic policy engine in `crate::placement`. It exists so that
// `placement.rs` can remain Wayland-free — the stub types
// (ToplevelInfoStub, WorkspaceStateStub, etc.) are the pure-logic contract,
// and this module owns the conversion + execution side.
//
// Phase 3 / T-019.
//
// Constraint E (D15): all workspace state mutations go through
// `crate::wayland::workspace::WorkspaceManager::transaction(|tx| ...)`.
//
// Constraint F (W1): PlacementDecision.warns are consumed here and emitted
// as WARN-once-per-process via AppData's AtomicBool guards.

#![allow(dead_code)]

use std::collections::HashSet;

use wayland_client::Proxy as _;

use crate::placement::{
    decide, PlacementAction, PlacementWarn, PostPlaceActions, SkipReason, ToplevelInfoStub,
    WorkspaceGroupStub, WorkspaceStateStub, WorkspaceStub, WorkspaceTarget,
};
use crate::wayland::workspace::WorkspaceManager;

// ---------------------------------------------------------------------------
// FR-014 — Pending placements queue
//
// WORKSPACE_MODE=new-each issues create_workspace + commit; the new workspace
// becomes visible on the NEXT WorkspaceHandler::done() dispatch, not synchronously.
// Placing synchronously on a fallback workspace silently violates FR-014's
// "wait one dispatch cycle" semantics. Instead, push to this queue, scan on
// done() events, and either land on the new workspace (success) or degrade
// with a WARN-once-per-process (compositor did not honor the request).
// ---------------------------------------------------------------------------

/// A placement deferred until the compositor reports the new workspace (or
/// until we've waited long enough to declare the create_workspace request
/// unanswered).
pub struct PendingPlacement {
    pub cosmic_toplevel: cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    /// ObjectId of the foreign toplevel handle this placement targets. Used by
    /// ToplevelInfoHandler::toplevel_closed to evict pendings whose toplevel
    /// closed during the window between push and the next done() scan — if we
    /// failed to evict, scan_pending_placements would issue move_to_ext_workspace
    /// against a zombie proxy (silent no-op at the wire layer) AND log
    /// "deferred placement completed" — exactly the lying-in-the-journal
    /// failure mode this project is built to prevent.
    pub foreign_toplevel_id: wayland_client::backend::ObjectId,
    pub group_handle: wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    pub output: wayland_client::protocol::wl_output::WlOutput,
    /// Snapshot of workspace handle ObjectIds in the group BEFORE create_workspace.
    /// Used to detect the appearance of a new handle.
    pub workspace_ids_before: HashSet<wayland_client::backend::ObjectId>,
    pub then: PostPlaceActions,
    pub app_id: String,
    pub cycles_waited: u32,
}

/// Pure-logic outcome of evaluating a single pending placement against the
/// current world state. Decoupled from Wayland types so it can be unit-tested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDecision {
    /// A new workspace appeared in the group; the caller looks up its handle
    /// by the given index into the current-group-workspaces list.
    UseNew { idx: usize },
    /// One dispatch cycle elapsed without a new workspace appearing.
    /// Degrade to next-free at the given index (or None if the group has no
    /// empty workspaces — caller skips with a WARN).
    DegradeAndWarn { next_free_idx: Option<usize> },
    /// First scan — still on cycle 0. Leave in the queue.
    KeepWaiting,
}

/// Pure-logic decision: given the snapshot and the current state of the
/// pending placement's group, decide what to do.
///
/// Inputs:
/// - `workspace_ids_before`: snapshot from PendingPlacement.
/// - `current_workspace_ids_in_group`: workspace handle IDs currently in the
///   group, in stable order.
/// - `current_workspace_occupied`: parallel array — true if the workspace
///   has any toplevel referencing it.
/// - `cycles_waited`: how many WorkspaceHandler::done() events have fired
///   since the create_workspace was issued.
pub fn evaluate_pending<Id: Eq + std::hash::Hash + Clone>(
    workspace_ids_before: &HashSet<Id>,
    current_workspace_ids_in_group: &[Id],
    current_workspace_occupied: &[bool],
    cycles_waited: u32,
) -> PendingDecision {
    debug_assert_eq!(
        current_workspace_ids_in_group.len(),
        current_workspace_occupied.len(),
        "evaluate_pending: workspace and occupancy slices must have equal length",
    );

    // Look for a workspace handle that wasn't in the snapshot.
    let new_idx = current_workspace_ids_in_group
        .iter()
        .position(|id| !workspace_ids_before.contains(id));

    if let Some(idx) = new_idx {
        return PendingDecision::UseNew { idx };
    }

    // No new workspace appeared. If we've already waited one cycle, degrade.
    // FR-014: "After one event-loop dispatch cycle, if no new workspace is
    // visible in WorkspaceState, the daemon MUST degrade to next-free."
    if cycles_waited >= 1 {
        let next_free_idx = current_workspace_occupied.iter().position(|occ| !*occ);
        return PendingDecision::DegradeAndWarn { next_free_idx };
    }

    PendingDecision::KeepWaiting
}

// ---------------------------------------------------------------------------
// Stub builders — convert real Wayland types to pure-logic stubs.
// ---------------------------------------------------------------------------

/// Convert real Wayland workspace state to the pure-logic stub type.
///
/// Output IDs use the WlOutput proxy's protocol_id() so they are stable within
/// a single compositor session (matching what WORKSPACE_OUTPUT stores).
fn build_world_stub(
    ws_state: &cosmic_client_toolkit::workspace::WorkspaceState,
    info_state: &cosmic_client_toolkit::toplevel_info::ToplevelInfoState,
) -> WorkspaceStateStub {
    let groups = ws_state
        .workspace_groups()
        .map(|g| {
            let output_ids: Vec<u64> =
                g.outputs.iter().map(|o| o.id().protocol_id() as u64).collect();

            // GroupCapabilities bit for create_workspace
            // (ext-workspace-v1.xml line 144).
            let can_create_workspace = g.capabilities.contains(
                wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::GroupCapabilities::CreateWorkspace,
            );

            // Workspaces in this group, with occupancy from ToplevelInfoState.
            let occupied_workspace_ids: HashSet<wayland_client::backend::ObjectId> = info_state
                .toplevels()
                .flat_map(|t| t.workspace.iter().map(|w| w.id()))
                .collect();

            let workspaces: Vec<WorkspaceStub> = ws_state
                .workspaces()
                .filter(|w| g.workspaces.contains(&w.handle))
                .map(|w| {
                    let ws_id = w.handle.id().protocol_id() as u64;
                    let is_occupied = occupied_workspace_ids.contains(&w.handle.id());
                    WorkspaceStub {
                        id: ws_id,
                        toplevel_ids: if is_occupied { vec![1] } else { vec![] },
                    }
                })
                .collect();

            let group_id = g.handle.id().protocol_id() as u64;
            WorkspaceGroupStub { id: group_id, output_ids, workspaces, can_create_workspace }
        })
        .collect();

    WorkspaceStateStub { groups }
}

/// Convert a real ToplevelInfo to the pure-logic stub.
fn build_info_stub(info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo) -> ToplevelInfoStub {
    ToplevelInfoStub {
        app_id: info.app_id.clone(),
        title: info.title.clone(),
        output_ids: info.output.iter().map(|o| o.id().protocol_id() as u64).collect(),
        cosmic_toplevel_present: info.cosmic_toplevel.is_some(),
    }
}

/// The toplevel_id we use for handled-set lookups in the stub layer.
/// Uses the foreign_toplevel handle's protocol_id() as a stable u64 key.
fn toplevel_stub_id(info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo) -> u64 {
    info.foreign_toplevel.id().protocol_id() as u64
}

/// Build the handled HashSet<u64> from the real ObjectId-keyed set.
fn build_handled_stub(
    handled: &HashSet<wayland_client::backend::ObjectId>,
) -> HashSet<u64> {
    handled.iter().map(|id| id.protocol_id() as u64).collect()
}

// ---------------------------------------------------------------------------
// Main integration handler
// ---------------------------------------------------------------------------

/// Integrate the placement pipeline with Wayland: decide + execute.
///
/// Called from `ToplevelInfoHandler::new_toplevel` for each new toplevel window.
/// Converts real Wayland types to pure-logic stubs, calls `decide()`, then
/// executes the resulting action sequence through the appropriate Wayland calls.
pub fn handle_new_toplevel(
    app: &mut crate::state::AppData,
    info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo,
) -> anyhow::Result<()> {
    // 1. Build pure-logic stubs from the real world state.
    let info_stub = build_info_stub(info);
    let ws_stub = build_world_stub(&app.workspace_state, &app.toplevel_info_state);
    let handled_stub = build_handled_stub(&app.handled);
    let toplevel_id = toplevel_stub_id(info);

    // 2. Run the pure decision function.
    let decision = decide(&app.config.clone(), &info_stub, &ws_stub, &handled_stub, toplevel_id);

    // 3. Consume warns (Constraint F): WARN-once-per-process via AtomicBool guards.
    for warn in &decision.warns {
        match warn {
            PlacementWarn::WorkspaceOutputFallback => app.warn_once_workspace_output_fallback(),
            PlacementWarn::NewEachUnsupported => app.warn_once_new_each_unsupported(),
        }
    }

    // 4. Execute the action.
    let action = decision.action;

    match action {
        PlacementAction::Skip { reason } => {
            log_skip(reason, info);
            Ok(())
        }
        PlacementAction::Place { workspace: target, then } => execute_place(app, info, target, then),
    }
}

fn log_skip(reason: SkipReason, info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo) {
    match reason {
        SkipReason::AlreadyHandled => {}
        SkipReason::ExcludedByAppId => {
            tracing::debug!(app_id = %info.app_id, "toplevel excluded by app_id; skipping placement");
        }
        SkipReason::ExcludedByTitle => {
            tracing::debug!(title = %info.title, "toplevel excluded by title regex; skipping placement");
        }
        SkipReason::NoCosmicToplevel => {
            tracing::warn!(app_id = %info.app_id, "toplevel has no cosmic_toplevel handle (FR-009); skipping");
        }
        SkipReason::NoOutputs => {
            tracing::warn!(app_id = %info.app_id, "toplevel has no outputs; cannot determine workspace group (FR-010)");
        }
        SkipReason::NoMatchingGroup => {
            tracing::warn!(app_id = %info.app_id, "no workspace group matches toplevel outputs; skipping placement");
        }
        SkipReason::WorkspaceModeSame => {
            tracing::debug!(app_id = %info.app_id, "WORKSPACE_MODE=same; not moving toplevel");
        }
    }
}

fn execute_place(
    app: &mut crate::state::AppData,
    info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo,
    target: WorkspaceTarget,
    then: crate::placement::PostPlaceActions,
) -> anyhow::Result<()> {
    // FR-009 guard at execution time.
    let cosmic_handle = match &info.cosmic_toplevel {
        Some(h) => h.clone(),
        None => {
            tracing::warn!(app_id = %info.app_id, "cosmic_toplevel is None at execution time; skipping");
            return Ok(());
        }
    };

    // Pick an output for the move request.
    let output = match info.output.iter().next().cloned() {
        Some(o) => o,
        None => {
            tracing::warn!(app_id = %info.app_id, "no output on toplevel at execution time; skipping");
            return Ok(());
        }
    };

    // Resolve the target workspace handle.
    let target_ws_handle = match target {
        WorkspaceTarget::Existing(ws_id) => {
            match app
                .workspace_state
                .workspaces()
                .find(|w| w.handle.id().protocol_id() as u64 == ws_id)
                .map(|w| w.handle.clone())
            {
                Some(h) => h,
                None => {
                    tracing::warn!(ws_id, app_id = %info.app_id, "target workspace no longer exists; skipping");
                    return Ok(());
                }
            }
        }
        WorkspaceTarget::Create => {
            // FR-014: create_workspace + commit on the target group, then DEFER
            // placement until the compositor reports the new workspace (or until
            // one dispatch cycle has elapsed without it appearing — degrade).
            //
            // The placement does NOT happen synchronously here — it is pushed to
            // app.pending_placements and resolved by scan_pending_placements()
            // on the next WorkspaceHandler::done() event.
            let target_output_id = output.id().protocol_id() as u64;
            let group_handle = match app
                .workspace_state
                .workspace_groups()
                .find(|g| g.outputs.iter().any(|o| o.id().protocol_id() as u64 == target_output_id))
                .map(|g| g.handle.clone())
            {
                Some(h) => h,
                None => {
                    tracing::warn!(app_id = %info.app_id, "no workspace group for output; skipping create_workspace");
                    return Ok(());
                }
            };

            // Workspace name policy (Q1): app_id, fallback auto-N.
            // wrapping_add is hygienic; u64 saturation is unreachable in practice
            // (~5×10^11 years at 1 toplevel/μs) but defensive against UB-on-overflow.
            let ws_name = if info.app_id.is_empty() {
                let name = format!("auto-{}", app.new_each_counter);
                app.new_each_counter = app.new_each_counter.wrapping_add(1);
                name
            } else {
                info.app_id.clone()
            };

            // Snapshot the current workspace handle IDs in the target group so
            // scan_pending_placements can detect the NEW handle when it appears.
            let group_for_snapshot = app
                .workspace_state
                .workspace_groups()
                .find(|g| g.handle == group_handle)
                .cloned();

            let workspace_ids_before: HashSet<wayland_client::backend::ObjectId> =
                match &group_for_snapshot {
                    Some(g) => app
                        .workspace_state
                        .workspaces()
                        .filter(|w| g.workspaces.contains(&w.handle))
                        .map(|w| w.handle.id())
                        .collect(),
                    None => HashSet::new(),
                };

            // Issue create_workspace + commit via WorkspaceManager::transaction (Constraint E).
            let manager = WorkspaceManager::from_state(&app.workspace_state)
                .map_err(|e| anyhow::anyhow!("workspace manager unavailable: {}", e))?;

            manager
                .transaction(|tx| {
                    tx.create_workspace(&group_handle, ws_name);
                    Ok(())
                })
                .map_err(|e| anyhow::anyhow!("create_workspace failed: {}", e))?;

            // Push pending placement. scan_pending_placements() on the next
            // WorkspaceHandler::done() will land it on the new workspace if one
            // appears, or degrade to next-free with WARN-once if not.
            //
            // foreign_toplevel_id is captured so toplevel_closed can evict the
            // pending if the toplevel dies in the push→done window (W1 fix).
            app.pending_placements.push(PendingPlacement {
                cosmic_toplevel: cosmic_handle,
                foreign_toplevel_id: info.foreign_toplevel.id(),
                group_handle,
                output,
                workspace_ids_before,
                then,
                app_id: info.app_id.clone(),
                cycles_waited: 0,
            });

            tracing::debug!(
                app_id = %info.app_id,
                "pending placement queued for WORKSPACE_MODE=new-each; awaiting WorkspaceHandler::done"
            );
            return Ok(());
        }
    };

    // FR-016: move request.
    crate::wayland::management::move_toplevel(app, &cosmic_handle, &target_ws_handle, &output);

    // FR-017 + Constraint G: activate + commit + verification timer.
    if then.switch_to {
        register_activate_with_verification(app, &target_ws_handle)?;
    }

    // FR-020: maximize.
    if then.maximize {
        crate::wayland::management::set_maximized(app, &cosmic_handle);
    }

    tracing::info!(
        app_id = %info.app_id,
        workspace_id = target_ws_handle.id().protocol_id(),
        switch = then.switch_to,
        maximize = then.maximize,
        "toplevel placed on workspace"
    );
    Ok(())
}

/// Issue activate + commit via WorkspaceManager::transaction, then register the
/// D9 verification timer (FR-018 / Constraint G).
fn register_activate_with_verification(
    app: &mut crate::state::AppData,
    target_ws_handle: &wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
) -> anyhow::Result<()> {
    let manager = WorkspaceManager::from_state(&app.workspace_state)
        .map_err(|e| anyhow::anyhow!("workspace manager unavailable for activate: {}", e))?;

    manager
        .transaction(|tx| {
            tx.activate(target_ws_handle);
            Ok(())
        })
        .map_err(|e| anyhow::anyhow!("activate failed: {}", e))?;

    let handle_id = target_ws_handle.id().protocol_id() as u64;

    // FR-019: timeout=None disables verification — just record_attempt and return.
    let Some(timeout) = app.config.switch_verify_timeout else {
        app.verifier.record_attempt(handle_id, None);
        return Ok(());
    };

    // Timer callback: on expiry, record_timeout and emit INFO/WARN per VerifyEvent.
    let token = app.loop_handle.insert_source(
        calloop::timer::Timer::from_duration(timeout),
        move |_, _, app_data: &mut crate::state::AppData| {
            let event = app_data.verifier.record_timeout(handle_id);
            app_data.pending_tokens.remove(&handle_id);
            emit_verify_event(event, handle_id, timeout, &app_data.verifier);
            calloop::timer::TimeoutAction::Drop
        },
    );

    match token {
        Ok(registration_token) => {
            // Use handle_id as the TimerId (TimerId is a u64 alias in pure-logic).
            app.verifier.record_attempt(handle_id, Some(handle_id));
            app.pending_tokens.insert(handle_id, registration_token);
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to register verification timer; proceeding without timeout verification");
            app.verifier.record_attempt(handle_id, None);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// MRU jump on empty workspace (T-MRU-006 / FR-MRU-001..012)
// ---------------------------------------------------------------------------

/// Evaluate whether the just-closed toplevel's workspace should trigger a jump
/// to the most-recently-visited workspace in the same group.
///
/// Called from `ToplevelInfoHandler::toplevel_closed` when `config.jump_on_empty`
/// is true. The closed toplevel's `ToplevelInfo` is still accessible at call time
/// (cosmic-client-toolkit-0.2.0/src/toplevel_info.rs:361-370).
///
/// # Borrow choreography (design §4.2)
/// Mirrors `execute_place`: collect owned data (IDs, occupancy, target_id) from
/// immutable borrows first, release them, then resolve `target_id → &ExtWorkspaceHandleV1`
/// for the final `register_activate_with_verification` call.
///
/// # RUST_LOG=info behavior
/// A successful jump emits `tracing::info!` with source/target/group/cause fields.
/// No-op cases emit `tracing::debug!` — journals stay clean at the default info level.
pub(crate) fn handle_empty_workspace_if_triggered(
    app: &mut crate::state::AppData,
    closed_id: &wayland_client::backend::ObjectId,
) {
    use crate::wayland::workspace::workspace_is_active;
    use crate::mru_jump::{WorkspaceMeta, TriggerInput, TriggerOutcome, NoOpReason, evaluate_trigger};
    use std::collections::HashMap;

    // Step 1 — pull closed toplevel's workspace handle set.
    // ToplevelInfo is still present at call time per toolkit source §361-370:
    // `toplevel_closed` is invoked BEFORE the toolkit removes ToplevelData.
    let closed_ws_handles: Vec<wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1> = {
        let info = app
            .toplevel_info_state
            .toplevels()
            .find(|t| t.foreign_toplevel.id() == *closed_id);
        match info {
            Some(i) => i.workspace.iter().cloned().collect(),
            None => {
                // Should not happen per toolkit guarantee, but handle gracefully.
                // Distinct from the D9 NoWorkspace case (ToplevelData found but its
                // workspace set is empty): this path means ToplevelData itself was
                // missing at handler call time.
                tracing::debug!(
                    closed_toplevel_id = %closed_id,
                    case = "toplevel_not_found",
                    "jump trigger no-op: ToplevelInfo not found at toplevel_closed time"
                );
                return;
            }
        }
    };

    // Step 2 — collect owned data from immutable borrows to build TriggerInput.

    // WorkspaceMeta slice: one entry per workspace the closing toplevel was on.
    let closed_workspaces: Vec<WorkspaceMeta> = closed_ws_handles
        .iter()
        .map(|h| WorkspaceMeta {
            id: h.id().protocol_id() as u64,
            coordinates: app
                .workspace_state
                .workspaces()
                .find(|w| w.handle.id() == h.id())
                .map(|w| w.coordinates.clone())
                .unwrap_or_default(),
        })
        .collect();

    // is_active per workspace.
    let is_active: HashMap<crate::ids::WorkspaceId, bool> = closed_ws_handles
        .iter()
        .map(|h| {
            let active = app
                .workspace_state
                .workspaces()
                .find(|w| w.handle.id() == h.id())
                .map(workspace_is_active)
                .unwrap_or(false);
            (h.id().protocol_id() as u64, active)
        })
        .collect();

    // Occupied-excluding-closed count per workspace.
    let occupied_excluding_closed: HashMap<crate::ids::WorkspaceId, usize> = closed_ws_handles
        .iter()
        .map(|h| {
            let count = app
                .toplevel_info_state
                .toplevels()
                .filter(|t| {
                    t.foreign_toplevel.id() != *closed_id
                        && t.workspace.iter().any(|w| w.id() == h.id())
                })
                .count();
            (h.id().protocol_id() as u64, count)
        })
        .collect();

    // Group id per workspace (None if orphaned).
    let group_id_for: HashMap<crate::ids::WorkspaceId, Option<u64>> = closed_ws_handles
        .iter()
        .map(|h| {
            let gid = app
                .workspace_state
                .workspace_groups()
                .find(|g| g.workspaces.iter().any(|wh| wh.id() == h.id()))
                .map(|g| g.handle.id().protocol_id() as u64);
            (h.id().protocol_id() as u64, gid)
        })
        .collect();

    // All workspaces per group + per-group occupied set (excluding closing handle).
    let occupied_ws_ids: std::collections::HashSet<wayland_client::backend::ObjectId> = app
        .toplevel_info_state
        .toplevels()
        .filter(|t| t.foreign_toplevel.id() != *closed_id)
        .flat_map(|t| t.workspace.iter().map(|w| w.id()))
        .collect();

    let mut group_workspaces: HashMap<u64, Vec<WorkspaceMeta>> = HashMap::new();
    let mut group_occupied: HashMap<u64, std::collections::HashSet<crate::ids::WorkspaceId>> =
        HashMap::new();

    for gid_opt in group_id_for.values().flatten() {
        let gid = *gid_opt;
        if group_workspaces.contains_key(&gid) {
            continue;
        }
        let g = app
            .workspace_state
            .workspace_groups()
            .find(|g| g.handle.id().protocol_id() as u64 == gid);
        if let Some(g) = g {
            let metas: Vec<WorkspaceMeta> = app
                .workspace_state
                .workspaces()
                .filter(|w| g.workspaces.contains(&w.handle))
                .map(|w| WorkspaceMeta {
                    id: w.handle.id().protocol_id() as u64,
                    coordinates: w.coordinates.clone(),
                })
                .collect();
            let occ: std::collections::HashSet<crate::ids::WorkspaceId> = metas
                .iter()
                .filter(|m| {
                    app.workspace_state
                        .workspaces()
                        .find(|w| w.handle.id().protocol_id() as u64 == m.id)
                        .map(|w| occupied_ws_ids.contains(&w.handle.id()))
                        .unwrap_or(false)
                })
                .map(|m| m.id)
                .collect();
            group_occupied.insert(gid, occ);
            group_workspaces.insert(gid, metas);
        }
    }

    // Step 3 — call the pure evaluator.
    let input = TriggerInput {
        closed_workspaces: &closed_workspaces,
        is_active: &is_active,
        occupied_excluding_closed: &occupied_excluding_closed,
        group_id_for: &group_id_for,
        group_workspaces: &group_workspaces,
        group_occupied: &group_occupied,
        recent_mru: &app.recent_workspaces,
    };

    let outcome = evaluate_trigger(&input);

    // Step 4 — dispatch outcome. All immutable borrows released before activation.
    match outcome {
        TriggerOutcome::Jump { source, target, group_id } => {
            // Resolve target WorkspaceId → &ExtWorkspaceHandleV1.
            let target_ws_handle = app
                .workspace_state
                .workspaces()
                .find(|w| w.handle.id().protocol_id() as u64 == target)
                .map(|w| w.handle.clone());

            match target_ws_handle {
                Some(h) => match register_activate_with_verification(app, &h) {
                    Ok(()) => {
                        tracing::info!(
                            source_workspace_id = source,
                            target_workspace_id = target,
                            group_id,
                            cause = "last_toplevel_closed",
                            "jumped to MRU/fallback workspace"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            source_workspace_id = source,
                            target_workspace_id = target,
                            "jump activation failed"
                        );
                    }
                },
                None => {
                    tracing::warn!(
                        workspace_id = target,
                        "jump target workspace handle not found; skipping activation"
                    );
                }
            }
        }
        TriggerOutcome::NoOp { reason: NoOpReason::NoWorkspace } => {
            tracing::debug!(
                closed_toplevel_id = %closed_id,
                case = "no_workspace",
                "jump trigger no-op"
            );
        }
        TriggerOutcome::NoOp { reason: NoOpReason::NotActive } => {
            tracing::debug!(
                closed_toplevel_id = %closed_id,
                case = "not_active",
                "jump trigger no-op"
            );
        }
        TriggerOutcome::NoOp { reason: NoOpReason::StillOccupied { ws } } => {
            tracing::debug!(
                closed_toplevel_id = %closed_id,
                workspace_id = ws,
                case = "still_occupied",
                "jump trigger no-op"
            );
        }
        TriggerOutcome::NoOp { reason: NoOpReason::NoTarget { group_id, source } } => {
            tracing::debug!(
                closed_toplevel_id = %closed_id,
                group_id,
                source_workspace_id = source,
                case = "no_target",
                "jump trigger no-op"
            );
        }
        TriggerOutcome::NoOp { reason: NoOpReason::NoGroup { ws } } => {
            tracing::warn!(
                closed_toplevel_id = %closed_id,
                workspace_id = ws,
                case = "no_group",
                "jump trigger no-op: workspace orphaned from group (anomalous)"
            );
        }
        TriggerOutcome::NoOp { reason: NoOpReason::MultiWorkspaceNoMatch { handle_count } } => {
            tracing::debug!(
                closed_toplevel_id = %closed_id,
                handle_count,
                case = "multi_workspace_no_match",
                "jump trigger no-op"
            );
        }
    }
}

fn emit_verify_event(
    event: crate::verify::VerifyEvent,
    handle_id: u64,
    timeout: std::time::Duration,
    verifier: &crate::verify::VerifierState,
) {
    match event {
        crate::verify::VerifyEvent::None => {}
        crate::verify::VerifyEvent::InfoOnceForHandle => {
            tracing::info!(
                workspace_handle_id = handle_id,
                timeout_ms = timeout.as_millis(),
                "workspace activation not confirmed within timeout; compositor may not have honored the request"
            );
        }
        crate::verify::VerifyEvent::WarnOnceForProcess => {
            tracing::warn!(
                workspace_handle_id = handle_id,
                attempted = verifier.attempted_distinct_count(),
                "compositor does not appear to be honoring workspace activation at all — distinct activations attempted, none confirmed"
            );
        }
        crate::verify::VerifyEvent::InfoAndWarn => {
            tracing::info!(
                workspace_handle_id = handle_id,
                timeout_ms = timeout.as_millis(),
                "workspace activation not confirmed within timeout; compositor may not have honored the request"
            );
            tracing::warn!(
                workspace_handle_id = handle_id,
                attempted = verifier.attempted_distinct_count(),
                "compositor does not appear to be honoring workspace activation at all — distinct activations attempted, none confirmed"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// FR-014 scan loop: drain pending placements on WorkspaceHandler::done
// ---------------------------------------------------------------------------

/// Walk `app.pending_placements`, evaluate each, and either complete the
/// placement on a new workspace (success) or degrade to next-free with
/// WARN-once-per-process (compositor did not honor `create_workspace`).
///
/// Called from `WorkspaceHandler::done` (the toolkit's batch-completion event).
/// Each invocation counts as one dispatch cycle of waiting.
pub fn scan_pending_placements(app: &mut crate::state::AppData) {
    if app.pending_placements.is_empty() {
        return;
    }

    let mut i = 0;
    while i < app.pending_placements.len() {
        // Increment cycle counter FIRST: this scan call counts as one cycle.
        app.pending_placements[i].cycles_waited += 1;

        // Snapshot the group's current workspaces.
        let group_info = app
            .workspace_state
            .workspace_groups()
            .find(|g| g.handle == app.pending_placements[i].group_handle)
            .cloned();

        let group = match group_info {
            Some(g) => g,
            None => {
                tracing::warn!(
                    app_id = %app.pending_placements[i].app_id,
                    "workspace group disappeared while pending placement was queued; dropping"
                );
                app.pending_placements.swap_remove(i);
                continue;
            }
        };

        let current_ids: Vec<wayland_client::backend::ObjectId> = app
            .workspace_state
            .workspaces()
            .filter(|w| group.workspaces.contains(&w.handle))
            .map(|w| w.handle.id())
            .collect();

        let occupied_set: HashSet<wayland_client::backend::ObjectId> = app
            .toplevel_info_state
            .toplevels()
            .flat_map(|t| t.workspace.iter().map(|w| w.id()))
            .collect();

        let current_occupied: Vec<bool> =
            current_ids.iter().map(|id| occupied_set.contains(id)).collect();

        let decision = evaluate_pending(
            &app.pending_placements[i].workspace_ids_before,
            &current_ids,
            &current_occupied,
            app.pending_placements[i].cycles_waited,
        );

        match decision {
            PendingDecision::KeepWaiting => {
                i += 1;
            }
            PendingDecision::UseNew { idx } => {
                let target_id = current_ids[idx].clone();
                let target_handle = app
                    .workspace_state
                    .workspaces()
                    .find(|w| w.handle.id() == target_id)
                    .map(|w| w.handle.clone());

                let pending = app.pending_placements.swap_remove(i);

                match target_handle {
                    Some(h) => execute_deferred_place(
                        app,
                        &pending.cosmic_toplevel,
                        &h,
                        &pending.output,
                        &pending.then,
                        &pending.app_id,
                    ),
                    None => {
                        tracing::warn!(
                            app_id = %pending.app_id,
                            "new workspace handle vanished between scan and place; dropping"
                        );
                    }
                }
                // Do NOT increment i — swap_remove brought a new pending into [i].
            }
            PendingDecision::DegradeAndWarn { next_free_idx } => {
                app.warn_once_create_workspace_not_honored();

                let target_handle = next_free_idx.and_then(|idx| {
                    let id = current_ids[idx].clone();
                    app.workspace_state
                        .workspaces()
                        .find(|w| w.handle.id() == id)
                        .map(|w| w.handle.clone())
                });

                let pending = app.pending_placements.swap_remove(i);

                match target_handle {
                    Some(h) => execute_deferred_place(
                        app,
                        &pending.cosmic_toplevel,
                        &h,
                        &pending.output,
                        &pending.then,
                        &pending.app_id,
                    ),
                    None => {
                        tracing::warn!(
                            app_id = %pending.app_id,
                            "create_workspace did not produce a new workspace and group has no empty fallback; skipping"
                        );
                    }
                }
                // Do NOT increment i — swap_remove brought a new pending into [i].
            }
        }
    }
}

/// Execute the move + activate + maximize sequence on a target workspace handle
/// that was resolved from a pending placement (FR-014 deferred path).
///
/// Same shape as the synchronous Existing-target path in `execute_place`, but
/// extracted so both call sites can share it.
fn execute_deferred_place(
    app: &mut crate::state::AppData,
    cosmic_handle: &cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    target_ws: &wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    output: &wayland_client::protocol::wl_output::WlOutput,
    then: &PostPlaceActions,
    app_id: &str,
) {
    crate::wayland::management::move_toplevel(app, cosmic_handle, target_ws, output);

    if then.switch_to {
        if let Err(e) = register_activate_with_verification(app, target_ws) {
            tracing::warn!(error = %e, "deferred activate failed");
        }
    }

    if then.maximize {
        crate::wayland::management::set_maximized(app, cosmic_handle);
    }

    tracing::info!(
        app_id = %app_id,
        workspace_id = target_ws.id().protocol_id(),
        switch = then.switch_to,
        maximize = then.maximize,
        "deferred placement on workspace completed"
    );
}

// ---------------------------------------------------------------------------
// Tests — pure-logic evaluation of pending-placement decision
// (the scan_pending_placements + execute_deferred_place glue is tested
// only by the type system + manual live-compositor verification per FR-OOS-008)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ids(values: &[u32]) -> Vec<u32> {
        values.to_vec()
    }

    fn snapshot(values: &[u32]) -> HashSet<u32> {
        values.iter().copied().collect()
    }

    #[test]
    fn evaluate_pending_keeps_waiting_on_cycle_zero_with_no_new_workspace() {
        // Just after push: cycles_waited=0, no new workspace yet → KeepWaiting.
        let before = snapshot(&[10, 20]);
        let current = ids(&[10, 20]);
        let occupied = vec![true, true];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 0),
            PendingDecision::KeepWaiting,
        );
    }

    #[test]
    fn evaluate_pending_finds_new_workspace_on_first_cycle() {
        // After first done(): cycles_waited=1, a new workspace (30) appeared.
        // UseNew with the index of the new workspace.
        let before = snapshot(&[10, 20]);
        let current = ids(&[10, 20, 30]);
        let occupied = vec![true, true, false];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 1),
            PendingDecision::UseNew { idx: 2 },
        );
    }

    #[test]
    fn evaluate_pending_finds_new_workspace_even_on_cycle_zero() {
        // If the compositor delivered the new workspace IMMEDIATELY (cycles_waited=0
        // because the scan was triggered before the increment), still use it.
        // Note: in production scan_pending_placements increments cycles_waited
        // FIRST, so cycles_waited >= 1 at evaluate time. This test pins behavior
        // for the boundary case where a caller happens to evaluate at cycle 0.
        let before = snapshot(&[10, 20]);
        let current = ids(&[10, 20, 30]);
        let occupied = vec![true, true, false];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 0),
            PendingDecision::UseNew { idx: 2 },
        );
    }

    #[test]
    fn evaluate_pending_degrades_after_one_cycle_with_no_new_workspace() {
        // After first done(): cycles_waited=1, no new workspace → degrade.
        // next-free is index 0 (the only empty workspace).
        let before = snapshot(&[10, 20, 30]);
        let current = ids(&[10, 20, 30]);
        let occupied = vec![false, true, true];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 1),
            PendingDecision::DegradeAndWarn { next_free_idx: Some(0) },
        );
    }

    #[test]
    fn evaluate_pending_degrades_with_no_fallback_when_group_fully_occupied() {
        // No new workspace, no empty fallback either — caller will skip with a WARN.
        let before = snapshot(&[10, 20]);
        let current = ids(&[10, 20]);
        let occupied = vec![true, true];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 1),
            PendingDecision::DegradeAndWarn { next_free_idx: None },
        );
    }

    #[test]
    fn evaluate_pending_picks_first_new_when_multiple_appeared() {
        // If the compositor created multiple workspaces in one cycle, use the
        // first one in iteration order (deterministic per the workspace_state
        // iterator's order).
        let before = snapshot(&[10]);
        let current = ids(&[10, 30, 31]);
        let occupied = vec![true, false, false];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 1),
            PendingDecision::UseNew { idx: 1 }, // first new at index 1
        );
    }

    #[test]
    fn evaluate_pending_treats_re_use_of_old_handle_as_no_new_workspace() {
        // Edge case: a workspace from the snapshot was removed AND a new one
        // appeared but with an ID we'd already seen — shouldn't happen in
        // practice (handles are not reused within a session) but defensive.
        let before = snapshot(&[10, 20, 30]);
        let current = ids(&[10, 20]); // 30 disappeared, no new ones
        let occupied = vec![true, true];
        assert_eq!(
            evaluate_pending(&before, &current, &occupied, 1),
            PendingDecision::DegradeAndWarn { next_free_idx: None },
        );
    }
}
