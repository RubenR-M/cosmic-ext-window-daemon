// wayland::toplevel — ToplevelInfoHandler impl + delegate_toplevel_info macro.
// SPDX-License-Identifier: GPL-3.0-only
//
// Routes new_toplevel / update_toplevel / toplevel_closed events to the
// placement coordinator. Implements ToplevelInfoHandler for AppData.
//
// Implemented in T-015 (Phase 3).

#![allow(dead_code, unused_imports)]

use cosmic_client_toolkit::toplevel_info::{ToplevelInfoHandler, ToplevelInfoState};
use wayland_client::{Connection, Proxy as _, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;

use crate::state::AppData;

// ---------------------------------------------------------------------------
// ToplevelInfoHandler for AppData
// ---------------------------------------------------------------------------

impl ToplevelInfoHandler for AppData {
    fn toplevel_info_state(&mut self) -> &mut ToplevelInfoState {
        &mut self.toplevel_info_state
    }

    /// Called by the toolkit after a new toplevel's initial Done event fires.
    ///
    /// FR-023: no grace period — placement logic runs directly here, without
    /// sleeping or deferring to a timer.
    ///
    /// FR-005: the handled-set check happens inside placement::handle_new_toplevel.
    fn new_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
        handle: &ExtForeignToplevelHandleV1,
    ) {
        // Look up the fully-populated ToplevelInfo for this handle.
        // The toolkit guarantees ToplevelInfo is complete at this call site
        // (post-Done guarantee per design D10.1 / cosmic-client-toolkit-0.2.0
        // src/toplevel_info.rs lines 372-385).
        let info = match self.toplevel_info_state.info(handle) {
            Some(i) => i.clone(),
            None => {
                // Should not happen post-Done; log WARN and skip.
                tracing::warn!(
                    handle_id = ?handle.id(),
                    "new_toplevel fired but ToplevelInfo is unavailable; skipping"
                );
                return;
            }
        };

        // Delegate to the placement coordinator (T-019).
        if let Err(e) = crate::placement::handle_new_toplevel(self, &info) {
            tracing::warn!(error = %e, "placement error for new toplevel; skipping");
        }

        // FR-005: add to handled-set after processing (whether placement occurred
        // or the toplevel was excluded / skipped). This prevents double-processing
        // on any subsequent update_toplevel events for the same handle.
        self.handled.insert(handle.id());
    }

    /// Called by the toolkit when an existing toplevel's state is updated.
    ///
    /// FR-005: D3 idempotency — the handle is already in the handled-set after
    /// new_toplevel, so this is a no-op in v0. We do not re-place toplevels on
    /// update events.
    fn update_toplevel(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
        _handle: &ExtForeignToplevelHandleV1,
    ) {
        // No-op in v0 — idempotency via handled-set. The handle is already in
        // self.handled after new_toplevel, so placement::handle_new_toplevel
        // would return Skip(AlreadyHandled) anyway.
    }

    /// Called by the toolkit when a toplevel is closed.
    ///
    /// FR-006: remove the ObjectId from the handled-set so a potential future
    /// toplevel that reuses the same protocol object slot starts fresh.
    fn toplevel_closed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<AppData>,
        handle: &ExtForeignToplevelHandleV1,
    ) {
        self.handled.remove(&handle.id());
    }

    // info_done and finished use the default no-op impls from the trait.
}
