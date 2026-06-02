// state — AppData struct: owns all toolkit state and daemon state.
// SPDX-License-Identifier: GPL-3.0-only
//
// AppData is the calloop callback data type — it is passed as `&mut AppData`
// to every Wayland event handler and calloop source callback.
//
// Implemented in T-018 (Phase 3).

#![allow(dead_code, unused_imports)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use calloop::{LoopHandle, RegistrationToken};
use cosmic_client_toolkit::{
    toplevel_info::ToplevelInfoState,
    toplevel_management::ToplevelManagerState,
    workspace::WorkspaceState,
};
use smithay_client_toolkit::{
    output::OutputState,
    registry::RegistryState,
    seat::SeatState,
};
use wayland_client::{
    QueueHandle,
    backend::ObjectId,
    protocol::wl_seat::WlSeat,
};

use crate::{
    config::Config,
    ids::WorkspaceId,
    verify::VerifierState,
};

// ---------------------------------------------------------------------------
// AppData — the single calloop dispatch data type
// ---------------------------------------------------------------------------

/// Calloop callback data. Holds all toolkit state and all daemon state.
///
/// `AppData` is constructed fresh on each `connect_and_run` iteration; it is
/// dropped wholesale on `DispatchError::Backend` (reconnect path). No stale
/// ObjectIds survive across reconnects (FR-021 / design §6.2).
pub struct AppData {
    // -----------------------------------------------------------------------
    // Toolkit state (owned by AppData; all proxies are valid for this session)
    // -----------------------------------------------------------------------
    pub registry_state: RegistryState,
    pub output_state: OutputState,
    pub seat_state: SeatState,
    pub toplevel_info_state: ToplevelInfoState,
    pub toplevel_manager_state: ToplevelManagerState,
    pub workspace_state: WorkspaceState,

    // -----------------------------------------------------------------------
    // Daemon config (immutable after startup; cloned from Supervisor)
    // -----------------------------------------------------------------------
    pub config: std::sync::Arc<Config>,

    // -----------------------------------------------------------------------
    // Handled-set (FR-005 idempotency / FR-006 cleanup on close)
    //
    // Keyed on ExtForeignToplevelHandleV1.id() — the ObjectId of the foreign
    // toplevel handle. Cleared wholesale on reconnect (design §6.2 rationale).
    // -----------------------------------------------------------------------
    pub handled: HashSet<ObjectId>,

    // -----------------------------------------------------------------------
    // D9 two-tier verifier
    //
    // VerifierState tracks the pure-logic state (attempt counts, fired flags).
    // pending_tokens maps WorkspaceId (u64 via protocol_id()) to the calloop
    // RegistrationToken for the associated verification timer, so that
    // on confirmation (WorkspaceHandler::done sees active bit) we can cancel
    // the timer with loop_handle.remove(token).
    // -----------------------------------------------------------------------
    pub verifier: VerifierState,
    /// Maps workspace handle protocol_id (WorkspaceId = u64) to the calloop
    /// timer token registered for D9 bounded-timeout verification.
    pub pending_tokens: HashMap<WorkspaceId, RegistrationToken>,

    // -----------------------------------------------------------------------
    // WARN-once-per-process guards (D6, D7 / Constraint F)
    //
    // Atomic so they are Sync (even though calloop is single-threaded, the
    // guards live inside AppData which may be behind Arc in some patterns;
    // AtomicBool costs nothing measurable and removes any future-proofing risk).
    // -----------------------------------------------------------------------
    /// FR-012: WORKSPACE_OUTPUT name not found; falling back to per-toplevel output.
    pub warn_workspace_output_fallback: AtomicBool,
    /// FR-014: new-each not supported on this compositor; degraded to next-free.
    pub warn_new_each_unsupported: AtomicBool,
    /// FR-014 (second variant): create_workspace was issued but the compositor
    /// did not produce a new workspace within one dispatch cycle; degraded.
    pub warn_create_workspace_not_honored: AtomicBool,

    // -----------------------------------------------------------------------
    // Calloop loop infrastructure
    //
    // loop_handle: used by handlers to register new timer sources (D9 verifier).
    // qh: QueueHandle for issuing outgoing Wayland requests.
    // -----------------------------------------------------------------------
    pub loop_handle: LoopHandle<'static, AppData>,
    pub qh: QueueHandle<AppData>,

    // -----------------------------------------------------------------------
    // Miscellaneous
    // -----------------------------------------------------------------------
    /// First seat from SeatState; used for COSMIC toplevel management calls.
    /// NFR-008: single-seat assumption.
    pub seat: Option<WlSeat>,

    /// Monotonic counter for NewEach workspace name fallback when app_id is empty (Q1).
    pub new_each_counter: u64,

    /// FR-014 pending-placement queue: NewEach mode pushes a PendingPlacement
    /// after create_workspace + commit, then waits up to one
    /// WorkspaceHandler::done() cycle for the new workspace to appear before
    /// degrading to next-free placement.
    pub pending_placements: Vec<crate::runtime::PendingPlacement>,

    // -----------------------------------------------------------------------
    // MRU-jump-on-empty (T-MRU-003 / D3 + D4)
    //
    // Gated on Config.jump_on_empty (D5): when false, neither field is mutated
    // after construction. Both dropped wholesale on reconnect (initial-impl D11 / D7).
    // -----------------------------------------------------------------------
    /// Most-recently-visited workspace IDs, front = most recent.
    /// Global deque (not per-group); intra-group filtering happens at query time
    /// in `mru_jump::select_jump_target` per D8. Capped at `MRU_CAP` = 16.
    pub recent_workspaces: std::collections::VecDeque<crate::ids::WorkspaceId>,
    /// Last observed active workspace per group.
    /// Key: `group.handle.id().protocol_id() as u64`
    /// Value: `workspace.handle.id().protocol_id() as u64`
    pub last_known_active: std::collections::HashMap<u64, crate::ids::WorkspaceId>,
}

impl AppData {
    /// Emit WARN-once-per-process for `WorkspaceOutputFallback`.
    ///
    /// Uses an AtomicBool swap: only the first caller emits.
    /// Subsequent calls are no-ops regardless of caller frequency (NFR-003).
    pub fn warn_once_workspace_output_fallback(&self) {
        if !self.warn_workspace_output_fallback.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "`WORKSPACE_OUTPUT` name not found; falling back to per-toplevel output"
            );
        }
    }

    /// Emit WARN-once-per-process for `NewEachUnsupported`.
    pub fn warn_once_new_each_unsupported(&self) {
        if !self.warn_new_each_unsupported.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "`new-each` not supported on this compositor; degraded to `next-free`"
            );
        }
    }

    /// Emit WARN-once-per-process for the FR-014 second variant: the compositor
    /// did NOT produce a new workspace within one dispatch cycle after
    /// create_workspace + commit. The placement was degraded to next-free.
    pub fn warn_once_create_workspace_not_honored(&self) {
        if !self.warn_create_workspace_not_honored.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "`create_workspace` did not produce a new workspace; degraded to `next-free`"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Smithay-client-toolkit delegation macros
//
// These wire the Dispatch impls from toolkit types to AppData, using the
// delegate_* macros from smithay-client-toolkit.
// ---------------------------------------------------------------------------

smithay_client_toolkit::delegate_registry!(AppData);
smithay_client_toolkit::delegate_output!(AppData);
smithay_client_toolkit::delegate_seat!(AppData);

// ---------------------------------------------------------------------------
// SeatHandler impl
// ---------------------------------------------------------------------------

impl smithay_client_toolkit::seat::SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &wayland_client::Connection, _qh: &QueueHandle<AppData>, seat: WlSeat) {
        // NFR-008: single-seat assumption — store only the first seat.
        if self.seat.is_none() {
            self.seat = Some(seat);
        }
    }

    fn new_capability(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<AppData>,
        _seat: WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
    }

    fn remove_capability(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<AppData>,
        _seat: WlSeat,
        _capability: smithay_client_toolkit::seat::Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &wayland_client::Connection, _qh: &QueueHandle<AppData>, seat: WlSeat) {
        if self.seat.as_ref().map(|s| s == &seat).unwrap_or(false) {
            self.seat = None;
        }
    }
}

// ---------------------------------------------------------------------------
// OutputHandler impl
// ---------------------------------------------------------------------------

impl smithay_client_toolkit::output::OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<AppData>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<AppData>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &wayland_client::Connection,
        _qh: &QueueHandle<AppData>,
        _output: wayland_client::protocol::wl_output::WlOutput,
    ) {
    }
}

// ---------------------------------------------------------------------------
// RegistryHandler impl
// ---------------------------------------------------------------------------

impl smithay_client_toolkit::registry::ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    smithay_client_toolkit::registry_handlers![
        OutputState,
        SeatState,
    ];
}

// ---------------------------------------------------------------------------
// Workspace delegation
// ---------------------------------------------------------------------------

cosmic_client_toolkit::delegate_workspace!(AppData);

// ---------------------------------------------------------------------------
// Toplevel delegation
// ---------------------------------------------------------------------------

cosmic_client_toolkit::delegate_toplevel_info!(AppData);
cosmic_client_toolkit::delegate_toplevel_manager!(AppData);
