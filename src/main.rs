// cosmic-ext-window-daemon — binary entry point.
// SPDX-License-Identifier: GPL-3.0-only
//
// D15 Layer 2 (T-010): denies direct calls to the three workspace-state-mutating
// protocol methods (ExtWorkspaceHandleV1::activate,
// ExtWorkspaceGroupHandleV1::create_workspace, ExtWorkspaceManagerV1::commit)
// listed in clippy.toml. The sole authorized call site is wayland::workspace in
// the library crate, which carries #![allow(clippy::disallowed_methods)].
//
// The CI gate (T-011) enforces this deny as a required-status-check on merge to
// main. SC-018 (T-012) demonstrates the gate blocking a bypass attempt.
#![deny(clippy::disallowed_methods)]

use cosmic_ext_window_daemon::{
    config,
    reconnect::Supervisor,
    app::connect_and_run,
};

fn main() {
    // T-021 — initialize tracing-subscriber with EnvFilter from RUST_LOG.
    // Default level: info (NFR-002). All output goes to stdout.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stdout)
        .init();

    // FR-003 / FR-004 — parse config from environment variables.
    // On error: log ERROR and exit with code 1 (NFR-007).
    let config = match config::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = ?e, "configuration error; exiting");
            std::process::exit(1);
        }
    };

    tracing::info!("cosmic-ext-window-daemon starting");

    // T-020 — build the reconnect supervisor and run the outer loop.
    // `connect_and_run` is the inner loop body; on StartupFailure it propagates
    // the error without retry; on BackendDisconnect the supervisor backs off and
    // retries; on Signal it exits cleanly.
    let mut supervisor = Supervisor::with_connect_fn(config, |cfg, backoff| {
        connect_and_run(cfg, backoff)
    });

    // FR-022 / NFR-007: exit 0 on clean shutdown, 1 on startup failure.
    match supervisor.run() {
        Ok(()) => {
            tracing::info!("daemon exiting cleanly");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!(error = ?e, "daemon startup failure; exiting");
            std::process::exit(1);
        }
    }
}
