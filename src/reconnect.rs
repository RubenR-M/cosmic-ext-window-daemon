// reconnect — bounded exponential backoff state machine.
// SPDX-License-Identifier: GPL-3.0-only
//
// BackoffState: steps [1s, 2s, 5s, 10s, 30s], saturating cursor.
//   next_delay() -> Duration — returns the next sleep duration and advances state.
//   reset()                  — returns sequence to start (call after successful reconnect).
//
// Pure logic — no actual sleeping in this module.
// The caller (event loop, Phase 3 T-020) sleeps for the returned Duration.
//
// Implemented in T-008 (Phase 1).

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
}
