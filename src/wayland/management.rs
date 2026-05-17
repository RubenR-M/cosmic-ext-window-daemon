// wayland::management — ToplevelManagerHandler impl + move/maximize helpers.
// SPDX-License-Identifier: GPL-3.0-only
//
// Houses move_toplevel() and set_maximized() calls against the COSMIC toplevel
// manager. Implements ToplevelManagerHandler for AppData.
//
// Note: move_to_ext_workspace does NOT require workspace-manager commit() —
// it is a toplevel-manager request. D15 only applies to workspace-manager
// state requests (activate, create_workspace, commit).
//
// Implemented in T-016 (Phase 3).

#![allow(dead_code, unused_imports)]

use cosmic_client_toolkit::toplevel_management::{ToplevelManagerHandler, ToplevelManagerState};
use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::{
    self, ZcosmicToplevelManagerV1,
};
use wayland_client::{Connection, QueueHandle, WEnum, protocol::wl_output::WlOutput};

use crate::state::AppData;

// ---------------------------------------------------------------------------
// ToplevelManagerHandler for AppData
// ---------------------------------------------------------------------------

impl ToplevelManagerHandler for AppData {
    fn toplevel_manager_state(&mut self) -> &mut ToplevelManagerState {
        &mut self.toplevel_manager_state
    }

    fn capabilities(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
        _capabilities: Vec<WEnum<zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1>>,
    ) {
        // No-op in v0 — capabilities are read at the point of use.
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Issue a move_to_ext_workspace request via the COSMIC toplevel manager.
///
/// FR-016: calls `ZcosmicToplevelManagerV1::move_to_ext_workspace(
///     &cosmic_toplevel_handle, &ext_workspace_handle, &wl_output)`.
///
/// Does NOT require a workspace-manager commit() — the toplevel manager
/// handles its own commit semantics (design §2.5 note).
pub fn move_toplevel(
    app: &AppData,
    toplevel: &cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
    workspace: &wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    output: &WlOutput,
) {
    app.toplevel_manager_state
        .manager
        .move_to_ext_workspace(toplevel, workspace, output);
}

/// Issue a set_maximized request via the COSMIC toplevel manager.
///
/// FR-020: MAXIMIZE=1 → call set_maximized after the move (and after
/// activate+commit if SWITCH_TO_WORKSPACE=1).
pub fn set_maximized(
    app: &AppData,
    toplevel: &cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::ZcosmicToplevelHandleV1,
) {
    app.toplevel_manager_state.manager.set_maximized(toplevel);
}
