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
    decide, PlacementAction, PlacementWarn, SkipReason, ToplevelInfoStub,
    WorkspaceGroupStub, WorkspaceStateStub, WorkspaceStub, WorkspaceTarget,
};
use crate::wayland::workspace::{first_empty_workspace_in_group, WorkspaceManager};

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
            // FR-014: create_workspace + commit on the target group, then find
            // the new workspace. The compositor's new-workspace event arrives
            // asynchronously — handle that in a follow-up commit
            // (currently degrades immediately to next-free).
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
            let ws_name = if info.app_id.is_empty() {
                let name = format!("auto-{}", app.new_each_counter);
                app.new_each_counter += 1;
                name
            } else {
                info.app_id.clone()
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

            // Immediate degrade to first-empty-in-group as the placement target for
            // THIS event. The newly-created workspace becomes visible on the next
            // dispatch cycle; honoring FR-014's "wait one cycle" semantics is the
            // pending-placement-queue work landing in a follow-up commit.
            let group_info = app
                .workspace_state
                .workspace_groups()
                .find(|g| g.handle == group_handle)
                .cloned();

            let fallback = group_info
                .as_ref()
                .and_then(|g| {
                    first_empty_workspace_in_group(&app.workspace_state, &app.toplevel_info_state, g)
                })
                .map(|w| w.handle.clone());

            match fallback {
                Some(h) => h,
                None => {
                    tracing::warn!(app_id = %info.app_id, "create_workspace did not produce a new workspace; no fallback available; skipping");
                    return Ok(());
                }
            }
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
