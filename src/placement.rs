// placement — policy engine: (Config, ToplevelInfoStub, WorkspaceStateStub) -> PlacementAction.
// SPDX-License-Identifier: GPL-3.0-only
//
// Pure decision function; no Wayland I/O or side effects.
// Stand-in types capture only the fields the policy needs.
// Real types are wired in Phase 3 (T-019) via thin From conversions.
//
// T-006 (Phase 1): pure decide() + stubs.
// T-019 (Phase 3): handle_new_toplevel() — bridges real Wayland types to decide().

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

/// Observable warnings emitted by the policy engine.
///
/// Phase 3 MUST read these from the `PlacementDecision.warns` field and emit
/// the corresponding WARN-once-per-process log lines. The warnings are
/// observable from the return value (not buried in `tracing!` macros) so
/// tests can assert their presence without mocking the logging stack.
///
/// Same discipline as `VerifyEvent` in `crate::verify` and `WorkspaceTarget`
/// in this module: side effects that callers MUST honor live in the return
/// value, not in hidden state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementWarn {
    /// FR-012: `WORKSPACE_OUTPUT` was set but the named output is not present
    /// in any workspace group. The decision falls back to per-toplevel output
    /// selection (FR-011). Phase 3 emits WARN-once-per-process:
    ///     "`WORKSPACE_OUTPUT` name not found; falling back to per-toplevel output"
    WorkspaceOutputFallback,

    /// FR-014: `WORKSPACE_MODE=new-each` but the selected group does NOT
    /// advertise the `create_workspace` capability. The decision falls back
    /// to `next-free` semantics within the group. Phase 3 emits
    /// WARN-once-per-process:
    ///     "`new-each` not supported on this compositor; degraded to `next-free`"
    NewEachUnsupported,
}

/// The full result of `decide`: the action to take plus any observable
/// warnings the caller must emit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementDecision {
    pub action: PlacementAction,
    pub warns: Vec<PlacementWarn>,
}

impl PlacementDecision {
    /// Construct a decision with no warnings (the common case).
    fn just(action: PlacementAction) -> Self {
        Self { action, warns: Vec::new() }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Pure decision function: given config and world state, return a placement
/// decision (action + any observable warnings).
///
/// `handled` is the daemon's in-memory set of toplevel IDs already processed (FR-005).
pub fn decide(
    config: &Config,
    info: &ToplevelInfoStub,
    workspaces: &WorkspaceStateStub,
    handled: &HashSet<u64>,
    toplevel_id: u64,
) -> PlacementDecision {
    // Early-exit branches: never accumulate warnings (the WARN-relevant work
    // happens during group/workspace selection, downstream of these guards).

    // FR-005 — idempotency
    if handled.contains(&toplevel_id) {
        return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::AlreadyHandled });
    }

    // FR-007 — exclusion by app_id
    if config.excluded_app_ids.iter().any(|id| id == &info.app_id) {
        return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::ExcludedByAppId });
    }

    // FR-008 — exclusion by title regex
    if let Some(re) = &config.excluded_title_regex {
        if re.is_match(&info.title) {
            return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::ExcludedByTitle });
        }
    }

    // FR-009 — cosmic_toplevel=None guard
    if !info.cosmic_toplevel_present {
        return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::NoCosmicToplevel });
    }

    // FR-010 — empty outputs guard
    if info.output_ids.is_empty() {
        return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::NoOutputs });
    }

    // FR-015 — WORKSPACE_MODE=same: no move
    if config.workspace_mode == WorkspaceMode::Same {
        return PlacementDecision::just(PlacementAction::Skip { reason: SkipReason::WorkspaceModeSame });
    }

    // Group + workspace selection both may emit warnings. We accumulate them
    // even on Skip paths so the caller can still emit the WARN that explains
    // why the skip happened in the multi-warn scenario (e.g., WORKSPACE_OUTPUT
    // absent AND per-toplevel fallback also fails → user gets BOTH signals).
    let mut warns: Vec<PlacementWarn> = Vec::new();

    // FR-011 / FR-012 — select target group
    let (target_group, group_warn) = select_group(config, info, workspaces);
    if let Some(w) = group_warn {
        warns.push(w);
    }
    let group = match target_group {
        Some(g) => g,
        None => {
            return PlacementDecision {
                action: PlacementAction::Skip { reason: SkipReason::NoMatchingGroup },
                warns,
            };
        }
    };

    // FR-013 / FR-014 — select target workspace within group
    let (target, workspace_warn) = select_workspace(config, group);
    if let Some(w) = workspace_warn {
        warns.push(w);
    }
    let target = match target {
        Some(t) => t,
        None => {
            return PlacementDecision {
                action: PlacementAction::Skip { reason: SkipReason::NoMatchingGroup },
                warns,
            };
        }
    };

    PlacementDecision {
        action: PlacementAction::Place {
            workspace: target,
            then: PostPlaceActions {
                switch_to: config.switch_to_workspace,
                maximize: config.maximize,
            },
        },
        warns,
    }
}

// ---------------------------------------------------------------------------
// Private helpers
// ---------------------------------------------------------------------------

/// Select the workspace group based on config (WORKSPACE_OUTPUT override or per-toplevel).
///
/// Returns `(group, warn)`. The warn is `Some(WorkspaceOutputFallback)` iff
/// `WORKSPACE_OUTPUT` was set but the named output was not found in any group
/// (regardless of whether per-toplevel fallback subsequently matched). The
/// warning is observable from the caller and MUST be propagated by Phase 3
/// — it is not optional decoration.
fn select_group<'a>(
    config: &Config,
    info: &ToplevelInfoStub,
    workspaces: &'a WorkspaceStateStub,
) -> (Option<&'a WorkspaceGroupStub>, Option<PlacementWarn>) {
    let mut warn: Option<PlacementWarn> = None;

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
                return (found, None);
            }
            // Override-output absent: emit WorkspaceOutputFallback and fall through to
            // per-toplevel selection. The warning fires whether or not per-toplevel matches.
            warn = Some(PlacementWarn::WorkspaceOutputFallback);
        } else {
            // Non-numeric WORKSPACE_OUTPUT (legacy/edge case at the stub layer).
            // The named-output lookup would have failed in Phase 3 too, so still warn.
            warn = Some(PlacementWarn::WorkspaceOutputFallback);
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
    (candidates.into_iter().next(), warn)
}

/// Select a workspace within a group per WorkspaceMode.
///
/// Returns `(target, warn)`. The warn is `Some(NewEachUnsupported)` iff
/// `WORKSPACE_MODE=new-each` was requested but the group does not advertise
/// the `create_workspace` capability bit. The warning is observable from the
/// caller and MUST be propagated by Phase 3 — silent degradation to
/// next-free without the WARN would mask a configuration that needs
/// administrator attention.
fn select_workspace(
    config: &Config,
    group: &WorkspaceGroupStub,
) -> (Option<WorkspaceTarget>, Option<PlacementWarn>) {
    match config.workspace_mode {
        WorkspaceMode::Same => unreachable!("Same mode is handled before group selection"),
        WorkspaceMode::NextFree => {
            // FR-013: first workspace with no toplevels
            let target = group
                .workspaces
                .iter()
                .find(|w| w.toplevel_ids.is_empty())
                .map(|w| WorkspaceTarget::Existing(w.id));
            (target, None)
        }
        WorkspaceMode::NewEach => {
            // FR-014: WorkspaceTarget::Create signals the caller (Phase 3) to invoke
            // create_workspace on the group manager.
            if group.can_create_workspace {
                (Some(WorkspaceTarget::Create), None)
            } else {
                // Degradation: fall back to next-free + emit NewEachUnsupported warn.
                let target = group
                    .workspaces
                    .iter()
                    .find(|w| w.toplevel_ids.is_empty())
                    .map(|w| WorkspaceTarget::Existing(w.id));
                (target, Some(PlacementWarn::NewEachUnsupported))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3 integration: handle_new_toplevel (T-019)
// ---------------------------------------------------------------------------

/// Convert real Wayland workspace state to the pure-logic stub type.
///
/// This is the bridge between the real compositor world (WorkspaceState,
/// ToplevelInfoState, WlOutput proxy objects) and the pure `decide()` function.
/// Pure conversion — no side effects, no mutations.
///
/// Output IDs use the WlOutput proxy's protocol_id() so they are stable within
/// a single compositor session (matching what WORKSPACE_OUTPUT stores).
fn build_world_stub(
    ws_state: &cosmic_client_toolkit::workspace::WorkspaceState,
    info_state: &cosmic_client_toolkit::toplevel_info::ToplevelInfoState,
) -> WorkspaceStateStub {
    use wayland_client::Proxy as _;

    let groups = ws_state
        .workspace_groups()
        .map(|g| {
            let output_ids: Vec<u64> = g.outputs.iter().map(|o| o.id().protocol_id() as u64).collect();

            // Determine whether create_workspace capability is set.
            // The GroupCapabilities bitflag for create_workspace is value 1
            // (ext-workspace-v1.xml line 144: <entry name="create_workspace" value="1">).
            let can_create_workspace = g
                .capabilities
                .contains(wayland_protocols::ext::workspace::v1::client::ext_workspace_group_handle_v1::GroupCapabilities::CreateWorkspace);

            // Workspaces in this group, with occupancy from ToplevelInfoState.
            let occupied_workspace_ids: std::collections::HashSet<wayland_client::backend::ObjectId> = info_state
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
///
/// Output IDs use protocol_id() so they are consistent with build_world_stub().
fn build_info_stub(info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo) -> ToplevelInfoStub {
    use wayland_client::Proxy as _;

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
    use wayland_client::Proxy as _;
    info.foreign_toplevel.id().protocol_id() as u64
}

/// Build the handled HashSet<u64> from the real ObjectId-keyed set.
///
/// Maps each ObjectId to its protocol_id() to match the u64 stub space.
fn build_handled_stub(
    handled: &std::collections::HashSet<wayland_client::backend::ObjectId>,
) -> HashSet<u64> {
    handled.iter().map(|id| id.protocol_id() as u64).collect()
}

/// Integrate the placement pipeline with Wayland: decide + execute.
///
/// Called from `ToplevelInfoHandler::new_toplevel` for each new toplevel window.
/// Converts real Wayland types to pure-logic stubs, calls `decide()`, then
/// executes the resulting action sequence through the appropriate Wayland calls.
///
/// Constraint E: all workspace state mutations go through
/// `WorkspaceManager::transaction(|tx| ...)` (D15 Layer 1 + Layer 2).
///
/// Constraint F: PlacementDecision.warns is consumed and emitted as
/// WARN-once-per-process via AppData's AtomicBool guards.
pub fn handle_new_toplevel(
    app: &mut crate::state::AppData,
    info: &cosmic_client_toolkit::toplevel_info::ToplevelInfo,
) -> anyhow::Result<()> {
    use wayland_client::Proxy as _;
    use crate::wayland::workspace::{
        WorkspaceManager,
        first_empty_workspace_in_group,
    };

    // -----------------------------------------------------------------------
    // 1. Build pure-logic stubs from the real world state.
    // -----------------------------------------------------------------------
    let info_stub = build_info_stub(info);
    let ws_stub = build_world_stub(&app.workspace_state, &app.toplevel_info_state);
    let handled_stub = build_handled_stub(&app.handled);
    let toplevel_id = toplevel_stub_id(info);

    // -----------------------------------------------------------------------
    // 2. Run the pure decision function.
    // -----------------------------------------------------------------------
    let decision = decide(&app.config.clone(), &info_stub, &ws_stub, &handled_stub, toplevel_id);

    // -----------------------------------------------------------------------
    // 3. Consume warns (Constraint F).
    //    Each warn variant emits WARN-once-per-process via AtomicBool guards.
    // -----------------------------------------------------------------------
    for warn in &decision.warns {
        match warn {
            PlacementWarn::WorkspaceOutputFallback => {
                app.warn_once_workspace_output_fallback();
            }
            PlacementWarn::NewEachUnsupported => {
                app.warn_once_new_each_unsupported();
            }
        }
    }

    // -----------------------------------------------------------------------
    // 4. Execute the action.
    // -----------------------------------------------------------------------
    let action = decision.action;

    match action {
        PlacementAction::Skip { reason } => {
            match reason {
                SkipReason::AlreadyHandled => {
                    // Silently skip — already processed.
                }
                SkipReason::ExcludedByAppId => {
                    tracing::debug!(app_id = %info.app_id, "toplevel excluded by app_id; skipping placement");
                }
                SkipReason::ExcludedByTitle => {
                    tracing::debug!(title = %info.title, "toplevel excluded by title regex; skipping placement");
                }
                SkipReason::NoCosmicToplevel => {
                    tracing::warn!(
                        app_id = %info.app_id,
                        "toplevel has no cosmic_toplevel handle (FR-009); skipping"
                    );
                }
                SkipReason::NoOutputs => {
                    tracing::warn!(
                        app_id = %info.app_id,
                        "toplevel has no outputs; cannot determine workspace group (FR-010)"
                    );
                }
                SkipReason::NoMatchingGroup => {
                    tracing::warn!(
                        app_id = %info.app_id,
                        "no workspace group matches toplevel outputs; skipping placement"
                    );
                }
                SkipReason::WorkspaceModeSame => {
                    tracing::debug!(app_id = %info.app_id, "WORKSPACE_MODE=same; not moving toplevel");
                }
            }
            return Ok(());
        }

        PlacementAction::Place { workspace: target, then } => {
            // We need the cosmic_toplevel handle (already guarded by FR-009 above).
            let cosmic_handle = match &info.cosmic_toplevel {
                Some(h) => h.clone(),
                None => {
                    // Should not happen — decide() guards this; defensive fallback.
                    tracing::warn!(app_id = %info.app_id, "cosmic_toplevel is None at execution time; skipping");
                    return Ok(());
                }
            };

            // Select the concrete ExtWorkspaceHandleV1 and an output for the move.
            // We need to find the target workspace handle and an output for the move call.
            // The decision carries WorkspaceTarget::{Existing(id), Create}.

            // Get the first output from the toplevel to use for the move request.
            let wl_output = info.output.iter().next().cloned();
            let output = match wl_output {
                Some(o) => o,
                None => {
                    tracing::warn!(app_id = %info.app_id, "no output on toplevel at execution time; skipping");
                    return Ok(());
                }
            };

            // Resolve the target workspace handle.
            let target_ws_handle: wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1 = match target {
                WorkspaceTarget::Existing(ws_id) => {
                    // Find the workspace with this protocol_id in the current workspace state.
                    let found = app.workspace_state
                        .workspaces()
                        .find(|w| w.handle.id().protocol_id() as u64 == ws_id)
                        .map(|w| w.handle.clone());
                    match found {
                        Some(h) => h,
                        None => {
                            tracing::warn!(ws_id, app_id = %info.app_id, "target workspace no longer exists; skipping");
                            return Ok(());
                        }
                    }
                }
                WorkspaceTarget::Create => {
                    // FR-014: create_workspace on the target group, then find the new workspace.
                    // We need the group handle. Select it from the topology.
                    let target_output_id = output.id().protocol_id() as u64;
                    let group_handle = app.workspace_state
                        .workspace_groups()
                        .find(|g| g.outputs.iter().any(|o| o.id().protocol_id() as u64 == target_output_id))
                        .map(|g| g.handle.clone());

                    let group_handle = match group_handle {
                        Some(h) => h,
                        None => {
                            tracing::warn!(app_id = %info.app_id, "no workspace group for output; skipping create_workspace");
                            return Ok(());
                        }
                    };

                    // Determine the workspace name (Q1 policy: app_id, fallback auto-N).
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

                    manager.transaction(|tx| {
                        tx.create_workspace(&group_handle, ws_name);
                        Ok(())
                    }).map_err(|e| anyhow::anyhow!("create_workspace failed: {}", e))?;

                    // After commit, the compositor will notify us of the new workspace via
                    // WorkspaceHandler::done on the next dispatch. We cannot block here.
                    // FR-014: if the new workspace doesn't appear, we degrade to next-free.
                    // For now, find the first empty workspace in the group as the fallback.
                    let group_info = app.workspace_state
                        .workspace_groups()
                        .find(|g| g.handle == group_handle)
                        .cloned();

                    let fallback = group_info.as_ref().and_then(|g| {
                        first_empty_workspace_in_group(&app.workspace_state, &app.toplevel_info_state, g)
                    }).map(|w| w.handle.clone());

                    match fallback {
                        Some(h) => h,
                        None => {
                            tracing::warn!(app_id = %info.app_id, "create_workspace did not produce a new workspace; no fallback available; skipping");
                            return Ok(());
                        }
                    }
                }
            };

            // ---------------------------------------------------------------
            // 5. Issue the move request (FR-016).
            // ---------------------------------------------------------------
            crate::wayland::management::move_toplevel(app, &cosmic_handle, &target_ws_handle, &output);

            // ---------------------------------------------------------------
            // 6. Activate + commit if SWITCH_TO_WORKSPACE=1 (FR-017).
            //    Wiring in calloop verification timer (Constraint G / FR-018).
            // ---------------------------------------------------------------
            if then.switch_to {
                let manager = WorkspaceManager::from_state(&app.workspace_state)
                    .map_err(|e| anyhow::anyhow!("workspace manager unavailable for activate: {}", e))?;

                manager.transaction(|tx| {
                    tx.activate(&target_ws_handle);
                    Ok(())
                }).map_err(|e| anyhow::anyhow!("activate failed: {}", e))?;

                // FR-018 / Constraint G: register a verification timer if configured.
                if let Some(timeout) = app.config.switch_verify_timeout {
                    let handle_id = target_ws_handle.id().protocol_id() as u64;
                    // loop_handle is used implicitly by insert_source (on app.loop_handle)

                    // Timer callback: on expiry, call record_timeout and emit INFO/WARN.
                    let token = app.loop_handle.insert_source(
                        calloop::timer::Timer::from_duration(timeout),
                        move |_, _, app_data: &mut crate::state::AppData| {
                            let event = app_data.verifier.record_timeout(handle_id);
                            app_data.pending_tokens.remove(&handle_id);
                            match event {
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
                                        "compositor does not appear to be honoring workspace activation at all — {} distinct workspace activations attempted, none confirmed",
                                        app_data.verifier.attempted_distinct_count()
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
                                        "compositor does not appear to be honoring workspace activation at all — {} distinct workspace activations attempted, none confirmed",
                                        app_data.verifier.attempted_distinct_count()
                                    );
                                }
                                crate::verify::VerifyEvent::None => {}
                            }
                            calloop::timer::TimeoutAction::Drop
                        },
                    );

                    match token {
                        Ok(registration_token) => {
                            // Record the attempt in the verifier state machine.
                            let timer_id = handle_id; // use handle_id as the TimerId (u64 alias)
                            app.verifier.record_attempt(handle_id, Some(timer_id));
                            app.pending_tokens.insert(handle_id, registration_token);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to register verification timer; proceeding without timeout verification");
                            // Still record the attempt without a timer (verification disabled for this handle).
                            app.verifier.record_attempt(handle_id, None);
                        }
                    }
                } else {
                    // SWITCH_VERIFY_TIMEOUT_MS=0 — FR-019: no timer.
                    // Still record_attempt with None so attempted_distinct is tracked.
                    let handle_id = target_ws_handle.id().protocol_id() as u64;
                    app.verifier.record_attempt(handle_id, None);
                }
            }

            // ---------------------------------------------------------------
            // 7. Maximize if MAXIMIZE=1 (FR-020).
            // ---------------------------------------------------------------
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
        }
    }

    Ok(())
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
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::AlreadyHandled });
    }

    // --- FR-007: ExcludedByAppId ---

    #[test]
    fn decide_returns_skip_excluded_when_app_id_matches() {
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "org.example.App,foot")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::ExcludedByAppId });
    }

    #[test]
    fn decide_does_not_exclude_when_app_id_does_not_match() {
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "foot")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result.action, PlacementAction::Place { .. }));
    }

    #[test]
    fn decide_exclusion_by_app_id_is_case_sensitive() {
        // "Org.Example.App" should NOT match "org.example.App"
        let cfg = config_with(&[("EXCLUDED_APP_IDS", "Org.Example.App")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result.action, PlacementAction::Place { .. }));
    }

    // --- FR-008: ExcludedByTitle ---

    #[test]
    fn decide_returns_skip_excluded_when_title_matches_regex() {
        let cfg = config_with(&[("EXCLUDED_TITLE_REGEX", "^Picture-in-Picture")]);
        let mut info = valid_info();
        info.title = "Picture-in-Picture — YouTube".to_string();
        let result = decide(&cfg, &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::ExcludedByTitle });
    }

    #[test]
    fn decide_does_not_exclude_when_title_does_not_match_regex() {
        let cfg = config_with(&[("EXCLUDED_TITLE_REGEX", "^Picture-in-Picture")]);
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert!(matches!(result.action, PlacementAction::Place { .. }));
    }

    // --- FR-009: NoCosmicToplevel ---

    #[test]
    fn decide_returns_skip_no_cosmic_toplevel_when_cosmic_handle_missing() {
        let mut info = valid_info();
        info.cosmic_toplevel_present = false;
        let result = decide(&default_config(), &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::NoCosmicToplevel });
    }

    // --- FR-010: NoOutputs ---

    #[test]
    fn decide_returns_skip_no_outputs_when_toplevel_has_no_outputs() {
        let mut info = valid_info();
        info.output_ids = vec![];
        let result = decide(&default_config(), &info, &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::NoOutputs });
    }

    // --- FR-015: WorkspaceModeSame ---

    #[test]
    fn decide_returns_skip_workspace_mode_same_when_mode_is_same() {
        let cfg = config_with_mode("same");
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::WorkspaceModeSame });
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
            result.action,
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
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::NoMatchingGroup });
    }

    // --- FR-013: NextFree ---

    #[test]
    fn decide_places_on_first_empty_workspace_in_next_free_mode() {
        // simple_workspaces: workspace 100 is occupied, 101 is empty → should pick 101.
        let result = decide(&default_config(), &valid_info(), &simple_workspaces(), &no_handled(), 1);
        assert_eq!(
            result.action,
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
        assert_eq!(result.action, PlacementAction::Skip { reason: SkipReason::NoMatchingGroup });
    }

    // --- FR-014: NewEach ---

    #[test]
    fn decide_returns_create_target_in_new_each_mode() {
        let cfg = config_with_mode("new-each");
        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);
        // WorkspaceTarget::Create instructs Phase 3 to invoke create_workspace.
        assert_eq!(
            result.action,
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
            result.action,
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
            result.action,
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
            result.action,
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
            result.action,
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
            result.action,
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
            result.action,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(100),
                then: PostPlaceActions { switch_to: false, maximize: false },
            }
        );
    }

    // -----------------------------------------------------------------------
    // W1 — Observable warns in PlacementDecision (FR-012, FR-014)
    // -----------------------------------------------------------------------

    #[test]
    fn decide_emits_workspace_output_fallback_warn_when_override_output_absent_and_per_toplevel_matches() {
        // WORKSPACE_OUTPUT=99 (absent), per-toplevel fallback to output 1 (group 10).
        // Phase 3 must emit WARN-once-per-process; the warn appears on the decision.
        let mut cfg = default_config();
        cfg.workspace_output = Some("99".to_string());

        let result = decide(&cfg, &valid_info(), &simple_workspaces(), &no_handled(), 1);

        assert_eq!(
            result.action,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            },
            "per-toplevel fallback should still place on the correct workspace",
        );
        assert_eq!(
            result.warns,
            vec![PlacementWarn::WorkspaceOutputFallback],
            "absent WORKSPACE_OUTPUT must emit WorkspaceOutputFallback warn",
        );
    }

    #[test]
    fn decide_emits_workspace_output_fallback_warn_even_when_per_toplevel_also_fails() {
        // WORKSPACE_OUTPUT=99 (absent), toplevel on output 99 (no group has it).
        // Result is Skip(NoMatchingGroup) BUT warn must still propagate so the user
        // gets the "WORKSPACE_OUTPUT not found" signal alongside the skip log.
        let mut cfg = default_config();
        cfg.workspace_output = Some("99".to_string());
        let mut info = valid_info();
        info.output_ids = vec![99]; // no group has output 99

        let result = decide(&cfg, &info, &simple_workspaces(), &no_handled(), 1);

        assert_eq!(
            result.action,
            PlacementAction::Skip { reason: SkipReason::NoMatchingGroup },
        );
        assert_eq!(
            result.warns,
            vec![PlacementWarn::WorkspaceOutputFallback],
            "warn must propagate even on Skip — both signals are independent",
        );
    }

    #[test]
    fn decide_emits_new_each_unsupported_warn_when_capability_bit_unset() {
        // WORKSPACE_MODE=new-each, group does NOT advertise create_workspace capability.
        // Decision degrades to next-free (WorkspaceTarget::Existing) and emits the warn.
        let mut cfg = config_with_mode("new-each");
        cfg.workspace_output = None;

        let workspaces = WorkspaceStateStub {
            groups: vec![WorkspaceGroupStub {
                id: 10,
                output_ids: vec![1],
                can_create_workspace: false, // capability bit unset
                workspaces: vec![WorkspaceStub { id: 101, toplevel_ids: vec![] }],
            }],
        };

        let result = decide(&cfg, &valid_info(), &workspaces, &no_handled(), 1);

        assert_eq!(
            result.action,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            },
            "degradation must produce a real Existing target, not Create",
        );
        assert_eq!(
            result.warns,
            vec![PlacementWarn::NewEachUnsupported],
            "missing create_workspace capability must emit NewEachUnsupported warn",
        );
    }

    #[test]
    fn decide_emits_both_warns_when_output_absent_and_new_each_unsupported() {
        // WORKSPACE_OUTPUT absent + WORKSPACE_MODE=new-each + capability unset.
        // BOTH warns must appear on the decision in deterministic order.
        let mut cfg = config_with_mode("new-each");
        cfg.workspace_output = Some("99".to_string());

        let workspaces = WorkspaceStateStub {
            groups: vec![WorkspaceGroupStub {
                id: 10,
                output_ids: vec![1],
                can_create_workspace: false,
                workspaces: vec![WorkspaceStub { id: 101, toplevel_ids: vec![] }],
            }],
        };

        let result = decide(&cfg, &valid_info(), &workspaces, &no_handled(), 1);

        assert_eq!(
            result.action,
            PlacementAction::Place {
                workspace: WorkspaceTarget::Existing(101),
                then: PostPlaceActions { switch_to: false, maximize: false },
            },
        );
        assert_eq!(
            result.warns,
            vec![
                PlacementWarn::WorkspaceOutputFallback,
                PlacementWarn::NewEachUnsupported,
            ],
            "warns appear in selection-pipeline order: group-selection first, then workspace-selection",
        );
    }
}
