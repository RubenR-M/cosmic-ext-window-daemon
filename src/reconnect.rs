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
// connect_and_run() (defined in app.rs) which owns one Connection, one
// EventLoop<'static, AppData>, and one AppData. On RunError::BackendDisconnect,
// Supervisor advances the BackoffState, sleeps, and retries.
//
// Signal safety during backoff:
//   calloop's Signals source blocks SIGTERM/SIGINT on the event-loop thread and
//   delivers them via signalfd. When the EventLoop drops (on BackendDisconnect),
//   the Signals source drops and unblocks the signals at the thread level.
//   The process is then vulnerable to default signal disposition (SIGTERM kills)
//   while sleeping in the backoff window. To prevent this, Supervisor::run()
//   installs a process-level SIGTERM/SIGINT handler BEFORE the first iteration.
//   The handler sets an Arc<AtomicBool> shutdown flag; run_with_sleep polls it
//   at 50ms intervals during backoff sleep. If the flag is set, the loop returns
//   Ok(()) immediately. The calloop Signals source (happy path) also remains —
//   whichever fires first wins.
//
// Implemented in T-008 (Phase 1) for BackoffState; T-020 (Phase 4) for Supervisor.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
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

/// Poll interval during backoff sleep — how often we check the shutdown flag.
/// Must be short enough that SIGTERM is acted upon within 100ms.
const SLEEP_POLL_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Process-level shutdown flag
//
// This static is set by the extern "C" signal handler and read by
// run_with_sleep during backoff polling. Using OnceLock<Arc<AtomicBool>>
// allows the flag to be shared with run_with_sleep while keeping the
// extern "C" handler dependency-free.
// ---------------------------------------------------------------------------

static PROCESS_SHUTDOWN_FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// SIGTERM / SIGINT handler (process-level safety net).
///
/// # Safety
/// This function is an async-signal-safe handler: it performs only an atomic
/// store, which is permitted inside a signal handler on Linux.
/// Installed via nix::sys::signal::sigaction before the supervisor loop starts.
extern "C" fn handle_process_shutdown(_sig: std::os::raw::c_int) {
    // SAFETY: atomic store is async-signal-safe.
    if let Some(flag) = PROCESS_SHUTDOWN_FLAG.get() {
        flag.store(true, Ordering::Relaxed);
    }
}

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
    /// Non-I/O calloop error (InvalidToken, OtherError) indicating an internal
    /// logic fault. Not retried — same fail-fast behavior as StartupFailure.
    InternalError(anyhow::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::StartupFailure(e) => write!(f, "startup failure: {}", e),
            RunError::BackendDisconnect => write!(f, "Wayland compositor disconnected"),
            RunError::InternalError(e) => write!(f, "internal event loop error: {}", e),
        }
    }
}

impl std::error::Error for RunError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RunError::StartupFailure(e) => Some(e.as_ref()),
            RunError::BackendDisconnect => None,
            RunError::InternalError(e) => Some(e.as_ref()),
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

    /// Run the outer loop with process-level SIGTERM/SIGINT handling.
    ///
    /// Installs a process-level signal handler that sets a shutdown flag before
    /// the first iteration. This ensures SIGTERM during a backoff sleep terminates
    /// the daemon within 50ms (the poll interval) rather than relying on
    /// calloop's Signals source, which is only active inside the event loop.
    ///
    /// - On `Ok(ExitReason::Signal)`: return `Ok(())`.
    /// - On `Err(RunError::BackendDisconnect)`: log INFO, advance backoff, sleep, retry.
    /// - On `Err(RunError::StartupFailure(e))` or `Err(RunError::InternalError(e))`:
    ///   propagate immediately without retry.
    pub fn run(&mut self) -> anyhow::Result<()> {
        // Initialize the process shutdown flag (idempotent across reconnects).
        let flag = PROCESS_SHUTDOWN_FLAG
            .get_or_init(|| Arc::new(AtomicBool::new(false)))
            .clone();

        // Install process-level SIGTERM/SIGINT handlers.
        // SAFETY: we install a simple async-signal-safe handler (atomic store).
        // These coexist with calloop's Signals source: when the event loop is
        // running, calloop delivers signals via signalfd (happy path). When the
        // event loop has exited (backoff window), these handlers fire instead.
        unsafe {
            use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal};
            let sa = SigAction::new(
                SigHandler::Handler(handle_process_shutdown),
                SaFlags::SA_RESTART,
                SigSet::empty(),
            );
            // Ignore error: if sigaction fails we degrade gracefully (no handler).
            let _ = nix::sys::signal::sigaction(Signal::SIGTERM, &sa);
            let _ = nix::sys::signal::sigaction(Signal::SIGINT, &sa);
        }

        self.run_with_sleep(flag, std::thread::sleep)
    }

    /// Run the outer loop with an injectable sleep function and shutdown flag.
    ///
    /// - `shutdown_flag`: polled at `SLEEP_POLL_INTERVAL` during backoff sleep.
    ///   When set, the loop exits with `Ok(())` (treated as Signal).
    ///   Production callers use `run()` which supplies the process signal handler flag.
    ///   Tests inject their own `Arc<AtomicBool>` to control shutdown without signals.
    /// - `sleep_fn(Duration)`: called with sub-intervals up to `SLEEP_POLL_INTERVAL`.
    ///   Tests inject a no-op to avoid real sleeps; in production this is
    ///   `std::thread::sleep`.
    pub(crate) fn run_with_sleep(
        &mut self,
        shutdown_flag: Arc<AtomicBool>,
        mut sleep_fn: impl FnMut(Duration),
    ) -> anyhow::Result<()> {
        loop {
            // Check the shutdown flag before each connect attempt so that a
            // signal received between the last sleep and the next connect
            // attempt is handled promptly.
            if shutdown_flag.load(Ordering::Relaxed) {
                tracing::info!("shutdown flag set before connect attempt; exiting");
                return Ok(());
            }

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
                    // Poll shutdown_flag at SLEEP_POLL_INTERVAL during the full delay.
                    // Guarantees we respond to SIGTERM within SLEEP_POLL_INTERVAL (50ms).
                    let mut remaining = delay;
                    while remaining > Duration::ZERO {
                        if shutdown_flag.load(Ordering::Relaxed) {
                            tracing::info!("shutdown flag set during backoff sleep; exiting");
                            return Ok(());
                        }
                        let step = remaining.min(SLEEP_POLL_INTERVAL);
                        sleep_fn(step);
                        remaining = remaining.saturating_sub(step);
                    }
                    // Continue loop — backoff.reset() is called inside connect_fn
                    // on successful connect+roundtrip (design §6.5).
                }
                Err(RunError::StartupFailure(e)) | Err(RunError::InternalError(e)) => {
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
    // Tests inject their own Arc<AtomicBool> shutdown flag so that no process
    // signal handler is involved — the flag is the only control mechanism.
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

    fn no_shutdown() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    #[test]
    fn supervisor_exits_cleanly_on_signal() {
        // connect_fn immediately returns Signal — Supervisor must return Ok(()).
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, _backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            Ok(ExitReason::Signal)
        });

        let result = supervisor.run_with_sleep(no_shutdown(), |_| {});
        assert!(result.is_ok(), "expected Ok on clean signal, got {:?}", result);
        assert_eq!(*call_count.lock().unwrap(), 1, "connect_fn must be called exactly once");
    }

    #[test]
    fn supervisor_retries_on_backend_disconnect_then_exits_on_signal() {
        // connect_fn: fail twice with BackendDisconnect, then return Signal.
        // Supervisor must retry (call count = 3) and return Ok(()).
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

        let result = supervisor.run_with_sleep(no_shutdown(), |_| {});
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

        let result = supervisor.run_with_sleep(no_shutdown(), |_| {});
        assert!(result.is_err(), "expected Err on startup failure");
        assert_eq!(*call_count.lock().unwrap(), 1, "must not retry on startup failure");
    }

    #[test]
    fn supervisor_propagates_internal_error_without_retry() {
        // RunError::InternalError must NOT be retried — same fail-fast as StartupFailure.
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, _backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            Err(RunError::InternalError(anyhow::anyhow!("calloop InvalidToken")))
        });

        let result = supervisor.run_with_sleep(no_shutdown(), |_| {});
        assert!(result.is_err(), "expected Err on internal error");
        assert_eq!(*call_count.lock().unwrap(), 1, "must not retry on internal error");
    }

    #[test]
    fn shutdown_flag_set_during_backoff_exits_before_next_connect() {
        // Scenario: BackendDisconnect happens, then shutdown flag is set *during*
        // the sleep phase. The supervisor must exit Ok(()) without calling
        // connect_fn a second time.
        //
        // Implementation: the sleep_fn sets the shutdown flag on its first call.
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();
        let flag = Arc::new(AtomicBool::new(false));
        let flag_for_sleep = flag.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, _backoff| {
            let mut n = call_count_clone.lock().unwrap();
            *n += 1;
            Err(RunError::BackendDisconnect)
        });

        let result = supervisor.run_with_sleep(flag.clone(), move |_d| {
            // Set the flag on first sleep call — simulates SIGTERM during backoff.
            flag_for_sleep.store(true, Ordering::Relaxed);
        });

        assert!(result.is_ok(), "expected Ok when shutdown flag set during backoff");
        // connect_fn called once (the disconnect); NOT called a second time
        // because the flag was set during the sleep phase.
        assert_eq!(
            *call_count.lock().unwrap(),
            1,
            "connect_fn must not be called again after shutdown flag set during backoff"
        );
    }

    #[test]
    fn d11_delay_sequence_captured_correctly() {
        // Asserts that run_with_sleep passes the correct D11 backoff delays to
        // the sleep function for two consecutive BackendDisconnect returns.
        // D11: 1s → 2s → 5s → 10s → 30s
        let captured = Arc::new(Mutex::new(Vec::<Duration>::new()));
        let captured_sleep = captured.clone();
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_fn = call_count.clone();

        let mut supervisor = Supervisor::with_connect_fn(test_config(), move |_cfg, backoff| {
            let mut n = call_count_fn.lock().unwrap();
            *n += 1;
            let attempt = *n;
            drop(n);
            if attempt <= 2 {
                Err(RunError::BackendDisconnect)
            } else {
                backoff.reset();
                Ok(ExitReason::Signal)
            }
        });

        // Sleep function accumulates all sub-interval calls.
        supervisor.run_with_sleep(no_shutdown(), |d| {
            captured_sleep.lock().unwrap().push(d);
        }).unwrap();

        // run_with_sleep chops each delay into SLEEP_POLL_INTERVAL (50ms) steps.
        // For a 1s delay: 20 × 50ms steps = 1s total.
        // For a 2s delay: 40 × 50ms steps = 2s total.
        // Verify the summed durations match D11 sequence steps 1 and 2.
        let all_sleeps = captured.lock().unwrap().clone();
        let first_delay_total: Duration = all_sleeps.iter().take(
            // Count slices for the first 1s delay
            (Duration::from_secs(1).as_millis() / SLEEP_POLL_INTERVAL.as_millis()) as usize
        ).sum();
        let second_delay_total: Duration = all_sleeps.iter().skip(
            (Duration::from_secs(1).as_millis() / SLEEP_POLL_INTERVAL.as_millis()) as usize
        ).sum();

        assert_eq!(first_delay_total, Duration::from_secs(1),
            "first backoff delay must be 1s (D11)");
        assert_eq!(second_delay_total, Duration::from_secs(2),
            "second backoff delay must be 2s (D11)");
    }
}
