// compile_fail: WorkspaceTx cannot be constructed by calling code.
//
// D15 Layer 1 invariant: the only way to obtain a WorkspaceTx is through
// WorkspaceManager::transaction(). All WorkspaceTx fields are private, so
// struct-literal construction from outside the wayland::workspace module
// is rejected by the compiler.
//
// Expected error: field `manager` of struct `WorkspaceTx` is private

use cosmic_ext_window_daemon::wayland::workspace::WorkspaceTx;

fn main() {
    // Attempting struct-literal construction of WorkspaceTx from outside
    // the module. This must not compile.
    let _tx = WorkspaceTx {
        manager: unreachable!(),
        _marker: std::marker::PhantomData,
    };
}
