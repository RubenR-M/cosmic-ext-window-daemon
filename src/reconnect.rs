// reconnect — bounded exponential backoff state machine and Supervisor.
// SPDX-License-Identifier: GPL-3.0-only
//
// BackoffState: steps [1s, 2s, 5s, 10s, 30s], saturating cursor.
// Supervisor: outer reconnect loop around the Wayland event loop.
// Implemented in T-008 (Phase 1) and T-020 (Phase 4).

#![allow(dead_code, unused_imports)]
