// SC-018 / T-012 — D15 Layer 2 GATE VERIFICATION
// SPDX-License-Identifier: GPL-3.0-only
//
// THIS FILE EXISTS SOLELY TO PROVE THAT CI BLOCKS A DISALLOWED-METHOD CALL
// MADE OUTSIDE wayland::workspace. THIS PR MUST NOT BE MERGED.
//
// The function below calls ExtWorkspaceHandleV1::activate directly — bypassing
// the WorkspaceManager::transaction wrapper that is the ONLY authorized site
// for that call (clippy.toml + #![deny(clippy::disallowed_methods)] in lib.rs
// and main.rs + #![allow(clippy::disallowed_methods)] only in wayland::workspace).
//
// On a correctly-configured CI, `cargo clippy --all-targets -- -D warnings`
// MUST report:
//   error: use of a disallowed method
//          `wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1::activate`
//
// and the PR's `ci` required status check MUST fail, blocking merge.
//
// Per the D15 closure contract (proposal D15 + spec NFR-004 Layer 2 + SC-018),
// THIS PR's CI failure is the operational proof that the gate works. The PR
// URL + the CI run URL are recorded in
// sdd/initial-implementation/verification-log under "T-012 — SC-018 closure".

use wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1;

pub fn bypass_attempt(handle: &ExtWorkspaceHandleV1) {
    // This call MUST trip clippy::disallowed_methods at the crate-root deny.
    handle.activate();
}
