// ids — common identifier types shared across modules.
// SPDX-License-Identifier: GPL-3.0-only
//
// These aliases avoid drift between modules that need to refer to the same
// abstract identifier. In Phase 1 (pure-logic) they are simple `u64`. In
// Phase 3 they will be replaced (or wrapped) by the real `wayland-client`
// `wayland_client::backend::ObjectId`. Having ONE place to change keeps the
// transition mechanical instead of a multi-file rename.

#![allow(dead_code)]

/// A workspace identifier — the `ExtWorkspaceHandleV1`'s `ObjectId` once
/// wired in Phase 3. In Phase 1 stubs it is a bare `u64`.
pub type WorkspaceId = u64;
