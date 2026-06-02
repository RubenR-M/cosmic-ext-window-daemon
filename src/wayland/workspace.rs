// wayland::workspace — THE D15 module.
// SPDX-License-Identifier: GPL-3.0-only
//
// Implements WorkspaceManager, the ONLY authorized site for raw
// ExtWorkspaceHandleV1::activate(), ExtWorkspaceGroupHandleV1::create_workspace(),
// and ExtWorkspaceManagerV1::commit() calls. All other modules MUST go through
// WorkspaceManager::transaction(|tx| { ... }).
//
// The clippy::disallowed_methods lint (configured in clippy.toml and denied at the
// crate root in T-010) is allowed here and ONLY here. `wayland::workspace` is the
// sole authorized call site for the three disallowed protocol methods.
//
// D15 Layer 1 invariant (SC-017 / NFR-004):
//   Constructing a workspace-state mutation without a corresponding commit() is
//   impossible to express in the Rust type system within this module because:
//   1. WorkspaceTx<'tx> has no public constructor — it can only be created by
//      WorkspaceManager::transaction(), which owns the commit() call.
//   2. WorkspaceTx<'tx> carries PhantomData<fn(&'tx ()) -> &'tx ()>, making 'tx
//      genuinely invariant (a `fn` parameter is contravariant + a `fn` return is
//      covariant — combined they pin the lifetime to exactly 'tx with no
//      coercions in either direction). Combined with the `for<'tx>` HRTB on the
//      closure, the transaction handle cannot outlive the closure or be stored
//      outside it.
//   3. The closure receives `&mut WorkspaceTx<'tx>`. `&mut T` is invariant in T,
//      which independently prevents the handle from being smuggled out via the
//      mutable reference. Both invariance mechanisms are load-bearing: if a
//      future maintainer changes the closure to take `WorkspaceTx<'tx>` by-value,
//      the &mut-invariance disappears and the PhantomData becomes the sole
//      remaining guard.
//   4. The only path that compiles for a caller is:
//        manager.transaction(|tx| { tx.activate(handle); Ok(()) })
//      which unconditionally calls commit() after the closure returns Ok(_).
//
// T-013 — Constraint C closure:
//   WorkspaceState::workspace_manager() at
//   cosmic-client-toolkit-0.2.0/src/workspace.rs lines 120-124 exposes:
//     pub fn workspace_manager(&self) -> &GlobalProxy<ExtWorkspaceManagerV1>
//   Preferred path (1) is available. The parallel-registry-bind fallback from
//   design §3 path (2) is NOT used. WorkspaceManager::from_state() below calls
//   workspace_state.workspace_manager().get() directly.
//
// T-014 — Constraint D closure:
//   Active bitflag value identified at:
//   wayland-protocols-0.32.12/protocols/staging/ext-workspace/ext-workspace-v1.xml
//   line 313: <entry name="active" value="1" summary="the workspace is active"/>
//   Rust constant: ext_workspace_handle_v1::State::ACTIVE (bits = 1u32).
//   workspace_is_active() below checks workspace.state.contains(State::ACTIVE).

#![allow(dead_code, unused_imports, clippy::disallowed_methods)]

use std::marker::PhantomData;

use cosmic_client_toolkit::{
    toplevel_info::ToplevelInfoState,
    workspace::{Workspace, WorkspaceGroup, WorkspaceHandler, WorkspaceState},
};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};

use crate::state::AppData;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that may arise from a workspace transaction.
#[derive(Debug)]
pub enum WorkspaceTxError {
    /// The ext_workspace_manager_v1 global is not yet bound (extension absent or
    /// registry roundtrip not complete). The transaction was not started.
    ManagerUnavailable,
    /// The caller's closure returned an error. commit() was NOT called.
    Other(anyhow::Error),
}

impl std::fmt::Display for WorkspaceTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceTxError::ManagerUnavailable => {
                write!(f, "ext_workspace_manager_v1 not available")
            }
            WorkspaceTxError::Other(e) => write!(f, "workspace transaction error: {}", e),
        }
    }
}

impl std::error::Error for WorkspaceTxError {}

// ---------------------------------------------------------------------------
// WorkspaceManager — the public entry-point
// ---------------------------------------------------------------------------

/// RAII wrapper around `ExtWorkspaceManagerV1` that enforces D15:
/// every workspace-state mutation (activate, create_workspace) is paired with
/// exactly one commit(), issued unconditionally after the closure returns Ok(_).
///
/// Construct via `WorkspaceManager::from_state(state)`.
/// Issue mutations via `WorkspaceManager::transaction(|tx| { ... })`.
pub struct WorkspaceManager<'a> {
    manager: &'a ExtWorkspaceManagerV1,
}

impl<'a> WorkspaceManager<'a> {
    /// Wrap a reference to an `ExtWorkspaceManagerV1` obtained from the registry.
    /// Used by tests and direct construction.
    pub fn new(manager: &'a ExtWorkspaceManagerV1) -> Self {
        Self { manager }
    }

    /// Construct from `WorkspaceState` using the toolkit's own accessor.
    ///
    /// Constraint C path (1): `WorkspaceState::workspace_manager()` exposes
    /// the toolkit's internally-bound `GlobalProxy<ExtWorkspaceManagerV1>`.
    /// Calling `.get()` on the proxy yields `&ExtWorkspaceManagerV1` if bound.
    /// Returns `Err(ManagerUnavailable)` if the extension is not yet bound.
    pub fn from_state(state: &'a WorkspaceState) -> Result<Self, WorkspaceTxError> {
        let manager = state
            .workspace_manager()
            .get()
            .map_err(|_| WorkspaceTxError::ManagerUnavailable)?;
        Ok(Self { manager })
    }

    /// The ONLY supported way to issue workspace-mutating requests.
    ///
    /// `commit()` is called unconditionally after the closure returns `Ok(_)`.
    /// If the closure returns `Err(_)`, `commit()` is NOT called — the partial
    /// mutations are left uncommitted (atomic rollback semantics at the compositor).
    ///
    /// # D15 compile-time guarantee
    ///
    /// The closure receives a `&mut WorkspaceTx<'tx>` where `'tx` is bound by a
    /// higher-ranked trait bound (`for<'tx>`). This means:
    /// - The caller cannot name `'tx` and therefore cannot store a reference to
    ///   `WorkspaceTx` outside the closure.
    /// - `WorkspaceTx` has no public constructor, so the only way to obtain one
    ///   is by entering this method.
    /// - `PhantomData<fn(&'tx ()) -> &'tx ()>`: `'tx` is genuinely invariant.
    pub fn transaction<R, F>(&self, f: F) -> Result<R, WorkspaceTxError>
    where
        F: for<'tx> FnOnce(&mut WorkspaceTx<'tx>) -> Result<R, WorkspaceTxError>,
    {
        let mut tx = WorkspaceTx {
            manager: self.manager,
            _marker: PhantomData,
        };
        match f(&mut tx) {
            Ok(value) => {
                // THE only commit() call site in the entire codebase.
                // clippy::disallowed_methods allowed at module scope.
                self.manager.commit();
                Ok(value)
            }
            Err(e) => Err(e), // closure errored → no commit (rollback)
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceTx — the transaction handle
// ---------------------------------------------------------------------------

/// A workspace transaction handle. Handed to the closure inside
/// `WorkspaceManager::transaction`; cannot be constructed or stored outside it.
///
/// Invariants:
/// - No public constructor: constructible only from `WorkspaceManager::transaction`.
/// - `PhantomData<fn(&'tx ()) -> &'tx ()>`: `'tx` is genuinely invariant.
pub struct WorkspaceTx<'tx> {
    manager: &'tx ExtWorkspaceManagerV1,
    _marker: PhantomData<fn(&'tx ()) -> &'tx ()>,
}

impl<'tx> WorkspaceTx<'tx> {
    /// Call `ExtWorkspaceHandleV1::activate()` on the given workspace handle.
    pub fn activate(&mut self, handle: &ExtWorkspaceHandleV1) -> &mut Self {
        handle.activate();
        self
    }

    /// Call `ExtWorkspaceGroupHandleV1::create_workspace()` on the given group.
    pub fn create_workspace(
        &mut self,
        group: &ExtWorkspaceGroupHandleV1,
        name: String,
    ) -> &mut Self {
        group.create_workspace(name);
        self
    }
}

// ---------------------------------------------------------------------------
// WorkspaceHandler impl for AppData (T-017 verifier integration)
// ---------------------------------------------------------------------------

impl WorkspaceHandler for AppData {
    fn workspace_state(&mut self) -> &mut WorkspaceState {
        &mut self.workspace_state
    }

    /// Called by the toolkit after every workspace state Done batch.
    ///
    /// Constraint G: walk all pending verification handles. For each one where
    /// the workspace's active bit is now set, call record_confirm() and cancel
    /// the calloop timer.
    fn done(&mut self) {
        use wayland_client::Proxy as _;

        // MRU producer (D3 / FR-MRU-006). Gated on Config.jump_on_empty (D5):
        // zero overhead when the feature is off (function is not called).
        // Placed FIRST in done() per OQ-4 resolution (design §5.1 + §11.1):
        // records all active-bit transitions visible in the current batch before
        // the verifier confirm walk or scan_pending_placements may issue new activates.
        if self.config.jump_on_empty {
            crate::mru_jump::update_mru_from_active_transitions(
                &mut self.last_known_active,
                &mut self.recent_workspaces,
                &self.workspace_state,
                crate::mru_jump::MRU_CAP,
            );
        }

        // Collect (handle_id, is_active) pairs from the settled workspace state.
        // We must collect because we borrow self.workspace_state immutably while
        // self.verifier and self.pending_tokens need mutable access afterward.
        let pending_handle_ids: Vec<crate::ids::WorkspaceId> = self
            .pending_tokens
            .keys()
            .copied()
            .collect();

        let mut confirmed: Vec<crate::ids::WorkspaceId> = Vec::new();
        for handle_id in &pending_handle_ids {
            let is_active = self
                .workspace_state
                .workspaces()
                .find(|w| w.handle.id().protocol_id() == *handle_id as u32)
                .map(workspace_is_active)
                .unwrap_or(false);

            if is_active {
                confirmed.push(*handle_id);
            }
        }

        for handle_id in confirmed {
            if let Some(token) = self.pending_tokens.remove(&handle_id) {
                self.loop_handle.remove(token);
            }
            self.verifier.record_confirm(handle_id);
        }

        // FR-014: drain pending placements queued by WORKSPACE_MODE=new-each.
        // Each done() event counts as one dispatch cycle of waiting. The first
        // scan after push either lands on the newly-created workspace or
        // degrades to next-free with a WARN-once-per-process.
        crate::runtime::scan_pending_placements(self);
    }
}

// ---------------------------------------------------------------------------
// Pure read-only queries (no D15 risk — these are reads, not mutations)
// ---------------------------------------------------------------------------

/// Select the workspace group whose `outputs` contains the given `WlOutput`.
///
/// Among multiple matching groups, picks the one with the lowest protocol object
/// ID for determinism (FR-011).
pub fn select_group_for_output<'a>(
    state: &'a WorkspaceState,
    output: &WlOutput,
) -> Option<&'a WorkspaceGroup> {
    use wayland_client::Proxy as _;

    let output_id = output.id();
    let mut candidates: Vec<&WorkspaceGroup> = state
        .workspace_groups()
        .filter(|g| g.outputs.iter().any(|o| o.id() == output_id))
        .collect();

    candidates.sort_by_key(|g| {
        use wayland_client::Proxy as _;
        g.handle.id().protocol_id()
    });
    candidates.into_iter().next()
}

/// Select the first workspace in the group with no toplevels on it (FR-013).
///
/// "Empty" = not referenced by any ToplevelInfo.workspace set.
/// Sorted by protocol_id for deterministic ordering.
pub fn first_empty_workspace_in_group<'a>(
    ws_state: &'a WorkspaceState,
    info_state: &ToplevelInfoState,
    group: &WorkspaceGroup,
) -> Option<&'a Workspace> {
    use wayland_client::Proxy as _;

    let occupied: std::collections::HashSet<wayland_client::backend::ObjectId> = info_state
        .toplevels()
        .flat_map(|t| t.workspace.iter().map(|w| w.id()))
        .collect();

    let mut group_workspaces: Vec<&Workspace> = ws_state
        .workspaces()
        .filter(|w| group.workspaces.contains(&w.handle))
        .collect();

    group_workspaces.sort_by_key(|w| w.handle.id().protocol_id());

    group_workspaces
        .into_iter()
        .find(|w| !occupied.contains(&w.handle.id()))
}

/// T-014 — check if a workspace's active bitflag is set.
///
/// Source: wayland-protocols-0.32.12/protocols/staging/ext-workspace/ext-workspace-v1.xml
/// line 313: `<entry name="active" value="1" summary="the workspace is active"/>` — bits = 1u32.
/// Rust constant: `ext_workspace_handle_v1::State::Active` (PascalCase per wayland-rs
/// code generation convention for bitflag entries).
pub fn workspace_is_active(workspace: &Workspace) -> bool {
    workspace.state.contains(ext_workspace_handle_v1::State::Active)
}
