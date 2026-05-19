// app — connect_and_run: one Wayland session from connect to disconnect.
// SPDX-License-Identifier: GPL-3.0-only
//
// `connect_and_run` is the inner loop body for the reconnect supervisor defined
// in `reconnect::Supervisor`. Each call owns:
//   - one `wayland_client::Connection`
//   - one `calloop::EventLoop<'static, AppData>`
//   - one `AppData` constructed from scratch (FR-021: no stale state survives)
//
// The function returns:
//   - `Ok(ExitReason::Signal)` when SIGTERM/SIGINT is received (FR-022).
//   - `Err(RunError::BackendDisconnect)` on compositor disconnect (FR-021).
//   - `Err(RunError::StartupFailure(_))` for non-recoverable init errors (FR-002).
//
// D8 fail-fast: ToplevelInfoState::try_new returning None → StartupFailure.
// D8 also requires cosmic_toplevel_info field to be Some (zcosmic_toplevel_info_v1
// must be advertised by the compositor, not just ext_foreign_toplevel_list_v1).
// The caller (Supervisor::run) does NOT retry on StartupFailure.
//
// Implemented in T-020 (reconnect loop) / T-021 (entrypoint wiring).

use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use calloop::signals::{Signal, Signals};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use cosmic_client_toolkit::toplevel_info::ToplevelInfoState;
use cosmic_client_toolkit::toplevel_management::ToplevelManagerState;
use cosmic_client_toolkit::workspace::WorkspaceState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::seat::SeatState;
use wayland_client::globals::registry_queue_init;
use wayland_client::Connection;

use crate::config::Config;
use crate::reconnect::{BackoffState, ExitReason, RunError};
use crate::state::AppData;
use crate::verify::VerifierState;

// ---------------------------------------------------------------------------
// D8 error messages (design §8.1 — exact pinned strings)
// ---------------------------------------------------------------------------

pub(crate) const D8_MISSING_COSMIC_TOPLEVEL_INFO: &str =
    "cosmic toplevel-info extension (zcosmic_toplevel_info_v1) not advertised by the \
compositor. This daemon requires a COSMIC compositor that exposes the cosmic \
toplevel-info / toplevel-management / workspace protocols. Confirm you are running \
the COSMIC desktop (System76 cosmic-comp >= 1.0.0-alpha.4 or equivalent) and that \
this process is started via `systemctl --user start cosmic-ext-window-daemon`. \
Run `systemctl --user status cosmic-ext-window-daemon` to inspect the failure. \
Exiting with code 1.";

pub(crate) const D8_MISSING_EXT_FOREIGN_TOPLEVEL: &str =
    "ext-foreign-toplevel-list-v1 not advertised by the compositor; \
ToplevelInfoState::try_new failed. Ensure a COSMIC compositor is running.";

pub(crate) const D8_MISSING_WORKSPACE_MANAGER: &str =
    "ext_workspace_manager_v1 not advertised by the compositor; \
workspace placement is impossible. Ensure a COSMIC compositor (cosmic-comp >= 1.0.0-alpha.4) \
is running and the ext-workspace protocol is available.";

// ---------------------------------------------------------------------------
// Required-globals guard (A17 / R2.2)
// ---------------------------------------------------------------------------

/// Validate that both Wayland extension globals are advertised by the compositor.
///
/// Extracted as a pure helper so the behavior can be tested without a live compositor.
/// Called from `connect_and_run` after constructing `ToplevelInfoState` and
/// `WorkspaceState`. Returns `Err(RunError::StartupFailure(_))` on the FIRST
/// missing global (D8 fail-fast order: cosmic_toplevel_info before workspace_manager).
pub(crate) fn check_required_globals(
    has_cosmic_toplevel_info: bool,
    has_workspace_manager: bool,
) -> Result<(), RunError> {
    if !has_cosmic_toplevel_info {
        return Err(RunError::StartupFailure(anyhow::anyhow!(
            "{}",
            D8_MISSING_COSMIC_TOPLEVEL_INFO
        )));
    }
    if !has_workspace_manager {
        return Err(RunError::StartupFailure(anyhow::anyhow!(
            "{}",
            D8_MISSING_WORKSPACE_MANAGER
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// connect_and_run — one full Wayland session
// ---------------------------------------------------------------------------

/// Attempt to connect to the Wayland compositor, build all state, and run the
/// event loop until either a signal is received or the backend disconnects.
///
/// `backoff` is passed in so the function can call `backoff.reset()` after a
/// successful connect+roundtrip, as specified in design §6.5.
pub fn connect_and_run(
    config: &Arc<Config>,
    backoff: &mut BackoffState,
) -> Result<ExitReason, RunError> {
    // FR-001 — connect to the Wayland compositor.
    let conn = Connection::connect_to_env().map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("failed to connect to Wayland compositor: {}", e))
    })?;

    // Initial registry roundtrip + global inspection.
    let (globals, event_queue) = registry_queue_init::<AppData>(&conn).map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("registry_queue_init failed: {}", e))
    })?;
    let qh = event_queue.handle();

    // Build the calloop event loop.
    let mut event_loop: EventLoop<'static, AppData> =
        EventLoop::try_new().map_err(|e| {
            RunError::StartupFailure(anyhow::anyhow!("calloop EventLoop::try_new failed: {}", e))
        })?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    // Smithay toolkit state.
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);

    // FR-002 / D8: fail-fast if ext-foreign-toplevel-list-v1 is absent.
    let toplevel_info_state = ToplevelInfoState::try_new(&registry_state, &qh)
        .ok_or_else(|| {
            let e = anyhow::anyhow!("{}", D8_MISSING_EXT_FOREIGN_TOPLEVEL);
            tracing::error!(error = %e);
            RunError::StartupFailure(e)
        })?;

    // COSMIC toplevel manager — also required.
    let toplevel_manager_state = ToplevelManagerState::try_new(&registry_state, &qh)
        .ok_or_else(|| {
            let e = anyhow::anyhow!(
                "zcosmic_toplevel_manager_v1 not advertised by the compositor; \
                 ensure COSMIC desktop is running"
            );
            tracing::error!(error = %e);
            RunError::StartupFailure(e)
        })?;

    // Workspace state — WorkspaceState::new is infallible (uses GlobalProxy::from),
    // so we must check the required global explicitly after construction.
    // Verified: cosmic-client-toolkit-0.2.0/src/workspace.rs:105-118.
    let workspace_state = WorkspaceState::new(&registry_state, &qh);

    // D8 fail-fast: check both zcosmic_toplevel_info_v1 and ext_workspace_manager_v1.
    // cosmic_toplevel_info is bound with .ok() inside ToplevelInfoState::try_new, so a
    // compositor exposing only ext-foreign-toplevel-list-v1 (not zcosmic_toplevel_info_v1)
    // passes try_new but silently routes every toplevel to NoCosmicToplevel.
    // Verified: cosmic-client-toolkit-0.2.0/src/toplevel_info.rs:88-124.
    if let Err(e) = check_required_globals(
        toplevel_info_state.cosmic_toplevel_info.is_some(),
        workspace_state.workspace_manager().get().is_ok(),
    ) {
        tracing::error!(error = %e);
        return Err(e);
    }

    // All fallible inserts: wire Wayland source and signal source into the loop
    // BEFORE constructing AppData and before resetting backoff (A16 / issues #5, #12).
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| {
            RunError::StartupFailure(anyhow::anyhow!("WaylandSource::insert failed: {}", e))
        })?;

    // FR-022: SIGTERM / SIGINT → stop the loop, exit 0.
    // `LoopSignal::stop()` breaks the `event_loop.run()` call cleanly.
    let loop_signal_for_handler = loop_signal.clone();
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT]).map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("calloop Signals::new failed: {}", e))
    })?;
    loop_handle.insert_source(signals, move |_event, _, _app: &mut AppData| {
        loop_signal_for_handler.stop();
    }).map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("failed to insert signal source: {}", e))
    })?;

    // Successful connect + roundtrip, all sources wired — reset backoff (design §6.5).
    // Placed here (after all fallible inserts) so a WaylandSource or Signals failure
    // on reconnect does NOT reset the backoff counter prematurely (issue #5).
    backoff.reset();

    tracing::info!("connected to Wayland compositor; starting event loop");

    // Build AppData — only after all fallible inserts succeed (issue #12: loop_handle
    // here always points to a live, fully-wired event loop).
    let verifier = VerifierState::new(3); // N=3 per design §5.4 / Q8
    let mut app_data = AppData {
        registry_state,
        output_state,
        seat_state,
        toplevel_info_state,
        toplevel_manager_state,
        workspace_state,
        config: config.clone(),
        handled: HashSet::new(),
        verifier,
        pending_tokens: HashMap::new(),
        warn_workspace_output_fallback: AtomicBool::new(false),
        warn_new_each_unsupported: AtomicBool::new(false),
        warn_create_workspace_not_honored: AtomicBool::new(false),
        loop_handle: loop_handle.clone(),
        qh,
        seat: None,
        new_each_counter: 0,
        pending_placements: Vec::new(),
    };

    // Run the event loop — blocks until loop_signal.stop() or a dispatch error.
    //
    // calloop 0.14: run() returns Err(calloop::Error::...) on dispatch failure;
    // we discriminate the error variant to distinguish backend disconnects from
    // internal loop errors and Wayland protocol violations (A16).
    //
    // timeout=None → block forever until a source requests stop (our signal handler).
    match event_loop.run(None, &mut app_data, |_app| {}) {
        Ok(()) => {
            // event_loop.run returned Ok — the signal handler called stop().
            Ok(ExitReason::Signal)
        }
        Err(e) => map_calloop_error(e),
    }
}

/// Map a `calloop::Error` to a `RunError`.
///
/// - `IoError` with `raw_os_error() == EPROTO` or `EBADMSG`: Wayland protocol
///   violation (calloop-wayland-source 0.4.1 surfaces both as EPROTO). Reconnecting
///   would immediately reproduce the same violation, so this routes to `InternalError`
///   rather than `BackendDisconnect`. See calloop-wayland-source 0.4.1 src/lib.rs:252-256.
/// - All other `IoError`: compositor disconnected normally → `BackendDisconnect` for retry.
/// - `InvalidToken` / `OtherError`: internal logic fault → `InternalError`, not retried.
pub(crate) fn map_calloop_error(e: calloop::Error) -> Result<ExitReason, RunError> {
    match e {
        calloop::Error::IoError(ref io_err) => {
            let raw = io_err.raw_os_error();
            // Defensive: calloop-wayland-source 0.4.1 only emits EPROTO; EBADMSG is included
            // in case upstream behavior changes in a future version.
            if raw == Some(nix::errno::Errno::EPROTO as i32)
                || raw == Some(nix::errno::Errno::EBADMSG as i32)
            {
                tracing::error!("wayland protocol violation: {}", io_err);
                Err(RunError::InternalError(anyhow::anyhow!(
                    "Wayland protocol violation (EPROTO/EBADMSG): {}",
                    io_err
                )))
            } else {
                tracing::warn!(error = %io_err, "Wayland backend I/O error; attempting reconnect");
                Err(RunError::BackendDisconnect)
            }
        }
        other => {
            let ae = anyhow::anyhow!("calloop internal error: {}", other);
            tracing::error!(error = %ae, "non-recoverable event loop error");
            Err(RunError::InternalError(ae))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // R2.1 — map_calloop_error: EPROTO/EBADMSG must route to InternalError
    // -----------------------------------------------------------------------

    #[test]
    fn map_calloop_error_eproto_routes_to_internal_error() {
        let io_err = std::io::Error::from_raw_os_error(nix::errno::Errno::EPROTO as i32);
        let calloop_err = calloop::Error::IoError(io_err);
        let result = map_calloop_error(calloop_err);
        assert!(
            matches!(result, Err(RunError::InternalError(_))),
            "EPROTO must map to InternalError, not BackendDisconnect"
        );
    }

    #[test]
    fn map_calloop_error_ebadmsg_routes_to_internal_error() {
        let io_err = std::io::Error::from_raw_os_error(nix::errno::Errno::EBADMSG as i32);
        let calloop_err = calloop::Error::IoError(io_err);
        let result = map_calloop_error(calloop_err);
        assert!(
            matches!(result, Err(RunError::InternalError(_))),
            "EBADMSG must map to InternalError, not BackendDisconnect"
        );
    }

    #[test]
    fn map_calloop_error_generic_io_routes_to_backend_disconnect() {
        // A plain I/O error (e.g. ECONNRESET) must still map to BackendDisconnect.
        let io_err = std::io::Error::from_raw_os_error(nix::errno::Errno::ECONNRESET as i32);
        let calloop_err = calloop::Error::IoError(io_err);
        let result = map_calloop_error(calloop_err);
        assert!(
            matches!(result, Err(RunError::BackendDisconnect)),
            "non-EPROTO IoError must map to BackendDisconnect"
        );
    }

    // -----------------------------------------------------------------------
    // R2.2 — check_required_globals: load-bearing tests for D8 guards
    // -----------------------------------------------------------------------

    #[test]
    fn check_required_globals_fails_d8_when_cosmic_toplevel_info_missing() {
        let result = check_required_globals(false, true);
        match result {
            Err(RunError::StartupFailure(e)) => {
                assert!(
                    e.to_string().contains("zcosmic_toplevel_info_v1"),
                    "error must name the missing protocol, got: {}",
                    e
                );
            }
            other => panic!("expected StartupFailure, got {:?}", other),
        }
    }

    #[test]
    fn check_required_globals_fails_when_workspace_manager_missing() {
        let result = check_required_globals(true, false);
        match result {
            Err(RunError::StartupFailure(e)) => {
                assert!(
                    e.to_string().contains("ext_workspace_manager_v1"),
                    "error must name the missing protocol, got: {}",
                    e
                );
            }
            other => panic!("expected StartupFailure, got {:?}", other),
        }
    }

    #[test]
    fn check_required_globals_reports_cosmic_toplevel_first_when_both_missing() {
        // D8 fail-fast: cosmic_toplevel_info takes priority over workspace_manager.
        let result = check_required_globals(false, false);
        match result {
            Err(RunError::StartupFailure(e)) => {
                assert!(
                    e.to_string().contains("zcosmic_toplevel_info_v1"),
                    "when both missing, must report D8 (zcosmic_toplevel_info_v1) first, got: {}",
                    e
                );
            }
            other => panic!("expected StartupFailure, got {:?}", other),
        }
    }

    // These tests verify the D8 and workspace-manager error message constants
    // are present and non-empty (strategy: source-of-truth constants are tested
    // directly; field-presence is enforced at runtime in connect_and_run above).
    // Stronger mocking of ToplevelInfoState/WorkspaceState is not possible without
    // private-field access into cosmic-client-toolkit structs.

    #[test]
    fn d8_cosmic_toplevel_info_error_message_contains_protocol_name() {
        assert!(
            D8_MISSING_COSMIC_TOPLEVEL_INFO.contains("zcosmic_toplevel_info_v1"),
            "D8 error message must name the missing protocol"
        );
    }

    #[test]
    fn d8_cosmic_toplevel_info_error_message_mentions_cosmic_compositor() {
        assert!(
            D8_MISSING_COSMIC_TOPLEVEL_INFO.contains("COSMIC"),
            "D8 error message must mention COSMIC compositor"
        );
    }

    #[test]
    fn d8_workspace_manager_error_message_contains_protocol_name() {
        assert!(
            D8_MISSING_WORKSPACE_MANAGER.contains("ext_workspace_manager_v1"),
            "workspace manager error message must name the missing protocol"
        );
    }

    #[test]
    fn d8_ext_foreign_toplevel_error_message_names_the_protocol() {
        assert!(
            D8_MISSING_EXT_FOREIGN_TOPLEVEL.contains("ext-foreign-toplevel-list-v1"),
            "D8 ext-foreign-toplevel error message must name the missing protocol \
             (ext-foreign-toplevel-list-v1); got: {}",
            D8_MISSING_EXT_FOREIGN_TOPLEVEL
        );
    }
}
