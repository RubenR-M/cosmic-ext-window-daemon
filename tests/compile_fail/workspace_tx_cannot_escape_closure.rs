// compile_fail: WorkspaceTx cannot be returned from the transaction closure.
//
// D15 Layer 1 invariant: the transaction() closure signature is fixed as
// `for<'tx> FnOnce(&mut WorkspaceTx<'tx>) -> Result<R, WorkspaceTxError>`.
// The return type is Result, not &mut WorkspaceTx<'_>. Any attempt to return
// the transaction handle itself from the closure fails as a type mismatch.
//
// Expected error: mismatched types — expected Result<_, WorkspaceTxError>,
//                 found &mut WorkspaceTx<'_>

use cosmic_ext_window_daemon::wayland::workspace::{WorkspaceManager, WorkspaceTxError};
use wayland_protocols::ext::workspace::v1::client::ext_workspace_manager_v1::ExtWorkspaceManagerV1;

fn attempt_to_get_tx_handle(manager: &WorkspaceManager<'_>) {
    // Trying to return the tx handle out of the transaction closure.
    // This must not compile: the closure must return Result<_, WorkspaceTxError>.
    let _smuggled = manager.transaction(|tx| {
        tx // type mismatch: &mut WorkspaceTx is not Result<_, WorkspaceTxError>
    });
}

fn main() {}
