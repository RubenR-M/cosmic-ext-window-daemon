// wayland::workspace — THE D15 module.
// SPDX-License-Identifier: GPL-3.0-only
//
// Implements WorkspaceHandler and exports WorkspaceManager — the ONLY authorized site for
// raw ExtWorkspaceHandleV1::activate(), ExtWorkspaceGroupHandleV1::create_workspace(), and
// ExtWorkspaceManagerV1::commit() calls. All other modules must go through
// WorkspaceManager::transaction(|tx| { ... }).
//
// The clippy::disallowed_methods lint (configured in clippy.toml) is allowed here and
// ONLY here. The crate root denies it everywhere else (applied in PR 2, T-010).
//
// Also exports query functions: select_group_for_output, first_empty_workspace_in_group,
// workspace_is_active. Implemented in T-009 and T-018 (Phase 2 and Phase 3).

#![allow(dead_code, unused_imports, clippy::disallowed_methods)]
