// compile_fail: WorkspaceTx reference cannot be stored in an outer variable.
//
// D15 Layer 1 invariant: the `for<'tx>` HRTB on the transaction closure means
// the compiler chooses `'tx` — the caller cannot name `'tx` and therefore
// cannot type an outer storage location as `&mut WorkspaceTx<'tx>`.
//
// Attempting to declare an outer `Option<&mut WorkspaceTx<'?>>` and populate
// it inside the closure fails because `'tx` is not in scope outside the closure
// and no concrete lifetime satisfies the borrow rules.
//
// Expected error: cannot infer an appropriate lifetime due to conflicting
//                 requirements (lifetime not long enough / borrows escape)

use cosmic_ext_window_daemon::wayland::workspace::{WorkspaceManager, WorkspaceTx, WorkspaceTxError};

fn attempt_outer_storage<'outer>(manager: &'outer WorkspaceManager<'outer>) {
    // Trying to store a reference to WorkspaceTx in a variable that outlives
    // the closure. The borrow checker must reject this.
    let mut outer: Option<&mut WorkspaceTx<'outer>> = None;

    let _ = manager.transaction(|tx: &mut WorkspaceTx<'_>| -> Result<(), WorkspaceTxError> {
        // Assigning a reference with the closure-bound lifetime to an outer
        // variable annotated with 'outer: the borrow checker must reject this
        // because 'tx != 'outer (the HRTB makes 'tx unnameable from outside).
        outer = Some(tx);
        Ok(())
    });
}

fn main() {}
