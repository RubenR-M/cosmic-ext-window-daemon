// compile_fail: WorkspaceTx cannot be smuggled out of the transaction closure
// via a wrapper struct.
//
// D15 Layer 1 invariant: the `for<'tx>` HRTB on the closure parameter makes 'tx
// universally quantified per-call — fresh and unnameable by the outer scope.
// Any attempt to wrap `&mut WorkspaceTx<'tx>` in a struct that uses an outer
// lifetime fails because 'tx cannot unify with any caller-visible lifetime.
//
// This is the REAL B3 lifetime-escape test (the earlier "smuggle by returning
// tx directly" was a strawman — it failed only because the closure must return
// Result, so the type mismatch fired before the lifetime check). Returning a
// wrapper struct passes the type check and reaches the lifetime check, which
// is the actual invariant we want to pin.
//
// Expected error: lifetime may not live long enough — the closure tries to
//                 expose tx with a fresh 'tx, but the outer signature demands
//                 a caller-visible lifetime.

use cosmic_ext_window_daemon::wayland::workspace::{
    WorkspaceManager, WorkspaceTx, WorkspaceTxError,
};

/// A struct that "smuggles" a transaction handle by holding a reference to it.
struct Smuggler<'a, 'tx>(&'a mut WorkspaceTx<'tx>);

fn try_to_smuggle<'a>(
    manager: &'a WorkspaceManager<'a>,
) -> Result<Smuggler<'a, 'a>, WorkspaceTxError> {
    // The closure's `tx: &mut WorkspaceTx<'tx>` has a fresh `'tx` (HRTB).
    // Wrapping it in `Smuggler<'a, 'a>` requires `'tx == 'a`, but the HRTB
    // says the closure must work for ANY `'tx`, not just one chosen by the
    // caller. The lifetime check fails.
    manager.transaction(|tx| Ok(Smuggler(tx)))
}

fn main() {}
