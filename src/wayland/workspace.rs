// wayland::workspace — THE D15 module.
// SPDX-License-Identifier: GPL-3.0-only
//
// Implements WorkspaceManager, the ONLY authorized site for raw
// ExtWorkspaceHandleV1::activate(), ExtWorkspaceGroupHandleV1::create_workspace(),
// and ExtWorkspaceManagerV1::commit() calls. All other modules MUST go through
// WorkspaceManager::transaction(|tx| { ... }).
//
// The clippy::disallowed_methods lint (configured in clippy.toml and denied at the
// crate root in T-010) is allowed here and ONLY here. `wayland::workspace` is the
// sole authorized call site for the three disallowed protocol methods.
//
// D15 Layer 1 invariant (SC-017 / NFR-004):
//   Constructing a workspace-state mutation without a corresponding commit() is
//   impossible to express in the Rust type system within this module because:
//   1. WorkspaceTx<'tx> has no public constructor — it can only be created by
//      WorkspaceManager::transaction(), which owns the commit() call.
//   2. WorkspaceTx<'tx> carries PhantomData<&'tx mut ()>, making 'tx invariant.
//      Combined with the HRTB `for<'tx>` bound on the closure, the transaction
//      handle cannot outlive the closure or be stored outside it.
//   3. The only path that compiles for a caller is:
//        manager.transaction(|tx| { tx.activate(handle); Ok(()) })
//      which unconditionally calls commit() after the closure returns Ok(_).

#![allow(dead_code, unused_imports, clippy::disallowed_methods)]

use std::marker::PhantomData;

use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1,
    ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    ext_workspace_manager_v1::ExtWorkspaceManagerV1,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that may arise from a workspace transaction.
#[derive(Debug)]
pub enum WorkspaceTxError {
    /// The ext_workspace_manager_v1 global is not yet bound (extension absent or
    /// registry roundtrip not complete). The transaction was not started.
    ManagerUnavailable,
    /// The caller's closure returned an error. commit() was NOT called.
    Other(anyhow::Error),
}

impl std::fmt::Display for WorkspaceTxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkspaceTxError::ManagerUnavailable => {
                write!(f, "ext_workspace_manager_v1 not available")
            }
            WorkspaceTxError::Other(e) => write!(f, "workspace transaction error: {}", e),
        }
    }
}

impl std::error::Error for WorkspaceTxError {}

// ---------------------------------------------------------------------------
// WorkspaceManager — the public entry-point
// ---------------------------------------------------------------------------

/// RAII wrapper around `ExtWorkspaceManagerV1` that enforces D15:
/// every workspace-state mutation (activate, create_workspace) is paired with
/// exactly one commit(), issued unconditionally after the closure returns Ok(_).
///
/// Construct via `WorkspaceManager::new(manager)`.
/// Issue mutations via `WorkspaceManager::transaction(|tx| { ... })`.
pub struct WorkspaceManager<'a> {
    manager: &'a ExtWorkspaceManagerV1,
}

impl<'a> WorkspaceManager<'a> {
    /// Wrap a reference to an `ExtWorkspaceManagerV1` obtained from the registry.
    pub fn new(manager: &'a ExtWorkspaceManagerV1) -> Self {
        Self { manager }
    }

    /// The ONLY supported way to issue workspace-mutating requests.
    ///
    /// `commit()` is called unconditionally after the closure returns `Ok(_)`.
    /// If the closure returns `Err(_)`, `commit()` is NOT called — the partial
    /// mutations are left uncommitted (atomic rollback semantics at the compositor).
    ///
    /// # D15 compile-time guarantee
    ///
    /// The closure receives a `&mut WorkspaceTx<'tx>` where `'tx` is bound by a
    /// higher-ranked trait bound (`for<'tx>`). This means:
    /// - The caller cannot name `'tx` and therefore cannot store a reference to
    ///   `WorkspaceTx` outside the closure.
    /// - `WorkspaceTx` has no public constructor, so the only way to obtain one
    ///   is by entering this method.
    /// - `PhantomData<&'tx mut ()>` makes `'tx` invariant, preventing lifetime
    ///   coercions that could smuggle the handle out.
    pub fn transaction<R, F>(&self, f: F) -> Result<R, WorkspaceTxError>
    where
        F: for<'tx> FnOnce(&mut WorkspaceTx<'tx>) -> Result<R, WorkspaceTxError>,
    {
        let mut tx = WorkspaceTx {
            manager: self.manager,
            _marker: PhantomData,
        };
        match f(&mut tx) {
            Ok(value) => {
                // THE only commit() call site in the entire codebase.
                // clippy::disallowed_methods allows this call inside this module.
                self.manager.commit();
                Ok(value)
            }
            Err(e) => Err(e), // closure errored → no commit (rollback)
        }
    }
}

// ---------------------------------------------------------------------------
// WorkspaceTx — the transaction handle
// ---------------------------------------------------------------------------

/// A workspace transaction handle. Handed to the closure inside
/// `WorkspaceManager::transaction`; cannot be constructed or stored outside it.
///
/// All mutation methods return `&mut Self` for ergonomic chaining:
///   `tx.activate(handle).create_workspace(group, name.into())`
///
/// Invariants:
/// - No public constructor: constructible only from `WorkspaceManager::transaction`.
/// - `PhantomData<&'tx mut ()>`: `'tx` is invariant, preventing escape.
pub struct WorkspaceTx<'tx> {
    manager: &'tx ExtWorkspaceManagerV1,
    // Invariant in 'tx — the transaction handle cannot outlive the closure scope.
    _marker: PhantomData<&'tx mut ()>,
}

impl<'tx> WorkspaceTx<'tx> {
    /// Call `ExtWorkspaceHandleV1::activate()` on the given workspace handle.
    ///
    /// The `activate()` call is batched; `commit()` will be issued by
    /// `WorkspaceManager::transaction` after the closure returns `Ok(_)`.
    pub fn activate(&mut self, handle: &ExtWorkspaceHandleV1) -> &mut Self {
        // clippy::disallowed_methods allowed at module scope.
        handle.activate();
        self
    }

    /// Call `ExtWorkspaceGroupHandleV1::create_workspace()` on the given group.
    ///
    /// The `create_workspace()` call is batched; `commit()` will be issued by
    /// `WorkspaceManager::transaction` after the closure returns `Ok(_)`.
    pub fn create_workspace(
        &mut self,
        group: &ExtWorkspaceGroupHandleV1,
        name: String,
    ) -> &mut Self {
        // clippy::disallowed_methods allowed at module scope.
        group.create_workspace(name);
        self
    }
}
