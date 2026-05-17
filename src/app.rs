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
// D8 error message (design §8.1 — exact pinned string)
// ---------------------------------------------------------------------------

fn d8_missing_extension_error() -> anyhow::Error {
    anyhow::anyhow!(
        "cosmic toplevel-info extension (zcosmic_toplevel_info_v1) not advertised by the \
compositor. This daemon requires a COSMIC compositor that exposes the cosmic \
toplevel-info / toplevel-management / workspace protocols. Confirm you are running \
the COSMIC desktop (System76 cosmic-comp >= 1.0.0-alpha.4 or equivalent) and that \
this process is started via `systemctl --user start cosmic-ext-window-daemon`. \
Run `systemctl --user status cosmic-ext-window-daemon` to inspect the failure. \
Exiting with code 1."
    )
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
    // Clone loop_signal: one goes to AppData (for future use), one to the signal handler.
    let loop_signal = event_loop.get_signal();
    let loop_signal_for_handler = loop_signal.clone();

    // Smithay toolkit state.
    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);

    // FR-002 / D8: fail-fast if the COSMIC toplevel-info extension is absent.
    let toplevel_info_state = ToplevelInfoState::try_new(&registry_state, &qh);
    if toplevel_info_state.is_none() {
        let e = d8_missing_extension_error();
        tracing::error!(error = %e);
        return Err(RunError::StartupFailure(e));
    }
    let toplevel_info_state = toplevel_info_state.unwrap();

    // COSMIC toplevel manager — also required.
    let toplevel_manager_state = ToplevelManagerState::try_new(&registry_state, &qh);
    if toplevel_manager_state.is_none() {
        let e = anyhow::anyhow!(
            "zcosmic_toplevel_manager_v1 not advertised by the compositor; \
             ensure COSMIC desktop is running"
        );
        tracing::error!(error = %e);
        return Err(RunError::StartupFailure(e));
    }
    let toplevel_manager_state = toplevel_manager_state.unwrap();

    // Workspace state.
    let workspace_state = WorkspaceState::new(&registry_state, &qh);

    // Successful connect + roundtrip — reset the backoff sequence (design §6.5).
    backoff.reset();

    tracing::info!("connected to Wayland compositor; starting event loop");

    // Build AppData with all owned state.
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
        loop_signal: loop_signal.clone(),
    };

    // Insert the WaylandSource into the event loop.
    WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .map_err(|e| {
            RunError::StartupFailure(anyhow::anyhow!("WaylandSource::insert failed: {}", e))
        })?;

    // FR-022: SIGTERM / SIGINT → stop the loop, exit 0.
    // `LoopSignal::stop()` breaks the `event_loop.run()` call cleanly.
    let signals = Signals::new(&[Signal::SIGTERM, Signal::SIGINT]).map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("calloop Signals::new failed: {}", e))
    })?;
    loop_handle.insert_source(signals, move |_event, _, _app: &mut AppData| {
        loop_signal_for_handler.stop();
    }).map_err(|e| {
        RunError::StartupFailure(anyhow::anyhow!("failed to insert signal source: {}", e))
    })?;

    // Run the event loop — blocks until loop_signal.stop() or a dispatch error.
    //
    // calloop 0.14: run() returns Err(calloop::Error::...) on dispatch failure;
    // we map calloop_wayland_source disconnect errors to RunError::BackendDisconnect.
    //
    // timeout=None → block forever until a source requests stop (our signal handler).
    match event_loop.run(None, &mut app_data, |_app| {}) {
        Ok(()) => {
            // event_loop.run returned Ok — the signal handler called stop().
            Ok(ExitReason::Signal)
        }
        Err(e) => {
            // Treat any event loop error as a backend disconnect; the supervisor
            // will decide whether to retry based on RunError variant.
            //
            // calloop surfaces I/O errors from WaylandSource as calloop::Error;
            // we map all of them to BackendDisconnect (design §6.1, FR-021).
            tracing::warn!(error = %e, "event loop error; attempting reconnect");
            Err(RunError::BackendDisconnect)
        }
    }
}
