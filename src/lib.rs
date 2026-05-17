// cosmic-ext-window-daemon — library surface.
// SPDX-License-Identifier: GPL-3.0-only
//
// This lib.rs exposes the daemon's internal modules so that:
//   1. Integration tests (including trybuild compile_fail tests for D15 Layer 1)
//      can reference types by their fully-qualified crate path.
//   2. The binary (`src/main.rs`) can call into the same logic without duplication.
//
// D15 Layer 2: `#![deny(clippy::disallowed_methods)]` is added to this file in
// T-010 alongside src/main.rs. The allow on `wayland::workspace` is sufficient
// to carve out the authorized call site — all other code in this crate (including
// any module reached through lib.rs) is covered by the crate-root deny.

pub mod config;
pub mod ids;
pub mod placement;
pub mod reconnect;
pub mod state;
pub mod verify;
pub mod wayland;
