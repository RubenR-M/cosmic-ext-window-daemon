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

fn main() {
    todo!("implement main entrypoint — T-021")
}
