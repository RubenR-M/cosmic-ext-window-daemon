// D15 Layer 1 — compile_fail tests (SC-017 / NFR-004 Layer 1)
//
// These tests verify that the WorkspaceTransaction invariant is enforced at
// compile time: workspace-state mutations (activate, create_workspace) can only
// be expressed by going through WorkspaceManager::transaction(|tx| { ... }).
// Any attempt to bypass the transaction boundary — constructing WorkspaceTx
// directly or smuggling it out of the closure — must be rejected by the compiler.
//
// Each test corresponds to one bypass scenario. trybuild compiles each snippet
// in isolation and expects a compile error (the `.stderr` sibling file captures
// the expected error message).
//
// Run: `cargo test workspace_tx_compile_fail`

#[test]
fn workspace_tx_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/workspace_tx_no_direct_construction.rs");
    t.compile_fail("tests/compile_fail/workspace_tx_cannot_escape_closure.rs");
    t.compile_fail("tests/compile_fail/workspace_tx_cannot_store_reference.rs");
}
