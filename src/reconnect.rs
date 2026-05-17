// reconnect — bounded exponential backoff state machine + reconnect supervisor.
// SPDX-License-Identifier: GPL-3.0-only
//
// BackoffState: steps [1s, 2s, 5s, 10s, 30s], saturating cursor.
//   next_delay() -> Duration — returns the next sleep duration and advances state.
//   reset()                  — returns sequence to start (call after successful reconnect).
//
// Pure logic — no actual sleeping in this module.
// The actual sleep is performed by Supervisor::run() between retries.
//
// Supervisor::run() is the outer reconnect loop. Each iteration calls
// connect_and_run() (defined in main.rs / the binary) which owns one
// Connection, one EventLoop<'static, AppData>, and one AppData. On
// RunError::BackendDisconnect, Supervisor advances the BackoffState, sleeps,
// and retries. On ExitReason::Signal, Supervisor::run() returns Ok(()).
//
// Implemented in T-008 (Phase 1) for BackoffState; T-020 (Phase 4) for Supervisor.

#![allow(dead_code)]

use std::time::Duration;

// ---------------------------------------------------------------------------
// Backoff sequence (D11)
// ---------------------------------------------------------------------------

const BACKOFF_STEPS: &[Duration] = &[
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Bounded exponential backoff state machine.
///
/// Sequence: 1s → 2s → 5s → 10s → 30s → 30s → 30s → ... (capped at 30s).
/// `reset()` returns to the start (1s) after a successful reconnect.
pub struct BackoffState {
    cursor: usize,
}

impl BackoffState {
    pub fn new() -> Self {
        Self { cursor: 0 }
    }

    /// Return the next delay duration and advance the internal cursor.
    /// Saturates at the last step (30s).
    pub fn next_delay(&mut self) -> Duration {
        let step = BACKOFF_STEPS[self.cursor];
        if self.cursor + 1 < BACKOFF_STEPS.len() {
            self.cursor += 1;
        }
        step
    }

    /// Reset the backoff sequence to the beginning.
    /// Call after a successful reconnect.
    pub fn reset(&mut self) {
        self.cursor = 0;
    }
}

impl Default for BackoffState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Exit reason and run error (T-020)
// ---------------------------------------------------------------------------

/// Reason the inner event loop returned cleanly.
#[derive(Debug, PartialEq, Eq)]
pub enum ExitReason {
    /// SIGTERM or SIGINT received; break the loop and exit 0.
    Signal,
}

/// Error from a single `connect_and_run` iteration.
#[derive(Debug)]
pub enum RunError {
    /// Startup failure: missing Wayland extension (FR-002), bad config, or
    /// non-recoverable init error. Not retried — propagates to main().
    StartupFailure(anyhow::Error),
    /// `DispatchError::Backend` from calloop-wayland-source: compositor
    /// disconnected. Supervisor backs off and retries (FR-021).
    BackendDisconnect,
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::StartupFailure(e) => write!(f, "startup failure: {}", e),
            RunError::BackendDisconnect => write!(f, "Wayland compositor disconnected"),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::StartupFailure(e) => Some(e.as_ref()),
            RunError::BackendDisconnect => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor — outer reconnect loop (T-020)
// ---------------------------------------------------------------------------

/// Outer reconnect supervisor.
///
/// Owns the `BackoffState` and `Arc<Config>` that survive across reconnects.
/// Calls a user-provided `connect_fn` for each iteration; on `BackendDisconnect`
/// backs off and retries; on `StartupFailure` or `Signal` returns immediately.
///
/// The `connect_fn` generic is what makes this unit-testable: in production
/// `main()` passes `crate::app::connect_and_run`; in tests a closure can simulate
/// failure/success sequences without a live Wayland compositor.
pub struct Supervisor<F>
where
    F: FnMut(&std::sync::Arc<crate::config::Config>, &mut BackoffState) -> Result<ExitReason, RunError>,
{
    config: std::sync::Arc<crate::config::Config>,
    backoff: BackoffState,
    connect_fn: F,
}

impl<F> Supervisor<F>
where
    F: FnMut(&std::sync::Arc<crate::config::Config>, &mut BackoffState) -> Result<ExitReason, RunError>,
{
    /// Create a supervisor with a custom connect function (for testing or wiring).
    pub fn with_connect_fn(config: crate::config::Config, connect_fn: F) -> Self {
        Self {
            config: std::sync::Arc::new(config),
            backoff: BackoffState::new(),
            connect_fn,
        }
    }

    /// Run the outer loop using `thread::sleep` for backoff delays.
    ///
    /// - On `Ok(ExitReason::Signal)`: return `Ok(())`.
    /// - On `Err(RunError::BackendDisconnect)`: log INFO, advance backoff, sleep, retry.
    /// - On `Err(RunError::StartupFailure(e))`: propagate immediately.
    pub fn run(&mut self) -> anyhow::Result<()> {
        self.run_with_sleep(std::thread::sleep)
    }

    /// Run the outer loop with an injectable sleep function.
    ///
    /// Production callers use `run()` (which injects `std::thread::sleep`).
    /// Tests inject a no-op closure to avoid real sleeps.
    pub fn run_with_sleep(&mut self, mut sleep_fn: impl FnMut(Duration)) -> anyhow::Result<()> {
        loop {
            match (self.connect_fn)(&self.config, &mut self.backoff) {
                Ok(ExitReason::Signal) => {
                    tracing::info!("received shutdown signal; exiting");
                    return Ok(());
                }
                Err(RunError::BackendDisconnect) => {
                    let delay = self.backoff.next_delay();
                    tracing::info!(
                        delay_secs = delay.as_secs_f64(),
                        "Wayland compositor disconnected; reconnecting after backoff"
                    );
                    sleep_fn(delay);
                    // Continue loop — backoff.reset() is called inside connect_fn
                    // on successful connect+roundtrip (design §6.5).
                }
                Err(RunError::StartupFailure(e)) => {
                    return Err(e);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_advances_through_sequence() {
        let mut b = BackoffState::new();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
        assert_eq!(b.next_delay(), Duration::from_secs(5));
        assert_eq!(b.next_delay(), Duration::from_secs(10));
        assert_eq!(b.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn backoff_caps_at_thirty_seconds() {
        let mut b = BackoffState::new();
        // Drain through the full sequence
        for _ in 0..5 {
            b.next_delay();
        }
        // All subsequent calls return 30s
        assert_eq!(b.next_delay(), Duration::from_secs(30));
        assert_eq!(b.next_delay(), Duration::from_secs(30));
        assert_eq!(b.next_delay(), Duration::from_secs(30));
    }

    #[test]
    fn backoff_resets_to_one_second() {
        let mut b = BackoffState::new();
        b.next_delay(); // 1s
        b.next_delay(); // 2s
        b.next_delay(); // 5s
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_reset_after_full_sequence_returns_to_one_second() {
        let mut b = BackoffState::new();
        for _ in 0..10 {
            b.next_delay();
        }
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
        assert_eq!(b.next_delay(), Duration::from_secs(2));
    }

    #[test]
    fn backoff_starts_at_one_second_on_new() {
        let mut b = BackoffState::new();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_default_starts_at_one_second() {
        let mut b = BackoffState::default();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[test]
    fn backoff_sequence_matches_d11_spec_exactly() {
        // D11: 1s, 2s, 5s, 10s, 30s, 30s, 30s, ...
        let mut b = BackoffState::new();
        let expected = [1u64, 2, 5, 10, 30, 30, 30];
        for secs in expected {
            assert_eq!(b.next_delay(), Duration::from_secs(secs));
        }
    }

    // -----------------------------------------------------------------------
    // T-020 — Supervisor integration (FR-021)
    //
    // These tests use a stub connect_fn instead of a real Wayland compositor
    // so they run in CI without FR-OOS-008 preconditions (no live compositor).
    // The sleep inside Supervisor::run() is bypassed by using zero-duration
    // steps in the tests via a modified approach: we mock the connect_fn
    // to control retry sequences, and we do NOT override BackoffState since
    // the actual sleep is at run() level. Instead tests inject immediate-return
    // connect functions and verify the correct call pattern.
    // -----------------------------------------------------------------------

    use std::sync::{Arc, Mutex};
    use crate::config::{Config, WorkspaceMode};

    fn test_config() -> Config {
        Config {
            workspace_mode: WorkspaceMode::NextFree,
            maximize: false,
            switch_to_workspace: false,
            switch_verify_timeout: None,
            excluded_app_ids: vec![],
            excluded_title_regex: None,
            workspace_output: None,
        }
    }

    #[test]
    fn supervisor_exits_cleanly_on_signal() {
        // connect_fn immediately returns Signal — Supervisor::run() must return Ok(()).
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, _backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            Ok(ExitReason::Signal)
        });

        let result = supervisor.run_with_sleep(|_| {});
        assert!(result.is_ok(), "expected Ok on clean signal, got {:?}", result);
        assert_eq!(*call_count.lock().unwrap(), 1, "connect_fn must be called exactly once");
    }

    #[test]
    fn supervisor_retries_on_backend_disconnect_then_exits_on_signal() {
        // connect_fn: fail twice with BackendDisconnect, then return Signal.
        // Supervisor must retry (call count = 3) and return Ok(()).
        // Uses run_with_sleep(|_| {}) to avoid real sleeps.
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            let attempt = *n;
            drop(n);

            if attempt <= 2 {
                Err(RunError::BackendDisconnect)
            } else {
                // Simulate successful reconnect: reset the backoff cursor.
                backoff.reset();
                Ok(ExitReason::Signal)
            }
        });

        let result = supervisor.run_with_sleep(|_| {});
        assert!(result.is_ok(), "expected Ok after reconnect, got {:?}", result);
        assert_eq!(*call_count.lock().unwrap(), 3, "connect_fn must be called 3 times");
    }

    #[test]
    fn supervisor_propagates_startup_failure_without_retry() {
        // connect_fn: return StartupFailure — Supervisor must NOT retry; return Err.
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, _backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            Err(RunError::StartupFailure(anyhow::anyhow!("missing cosmic extension")))
        });

        let result = supervisor.run_with_sleep(|_| {});
        assert!(result.is_err(), "expected Err on startup failure");
        assert_eq!(*call_count.lock().unwrap(), 1, "must not retry on startup failure");
    }
}
