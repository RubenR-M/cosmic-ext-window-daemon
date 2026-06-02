// cosmic-ext-window-daemon — library surface.
// SPDX-License-Identifier: GPL-3.0-only
//
// This lib.rs exposes the daemon's internal modules so that:
//   1. Integration tests (including trybuild compile_fail tests for D15 Layer 1)
//      can reference types by their fully-qualified crate path.
//   2. The binary (`src/main.rs`) can call into the same logic without duplication.
//
// D15 Layer 2 (T-010): `#![deny(clippy::disallowed_methods)]` at the crate root
// makes direct calls to ExtWorkspaceHandleV1::activate,
// ExtWorkspaceGroupHandleV1::create_workspace, and ExtWorkspaceManagerV1::commit
// (listed in clippy.toml) compile-errors everywhere EXCEPT in `wayland::workspace`,
// which carries `#![allow(clippy::disallowed_methods)]` as its sole authorized site.
//
// The CI gate (T-011, cargo clippy --all-targets -- -D warnings as a required
// status check) is what gives this deny teeth — without enforcement it would be
// decorative. SC-018 (T-012, the test PR) is the observed proof that the gate works.
#![deny(clippy::disallowed_methods)]

pub mod app;
pub mod config;
pub mod ids;
pub(crate) mod mru_jump;
pub mod placement;
pub mod reconnect;
pub mod runtime;
pub mod state;
pub mod verify;
pub mod wayland;
