//! Compile-fail test verifying the public API boundary of `ChildLease`.
//!
//! Uses `trybuild` to verify that `ChildLease`'s private methods
//! (`confirm_exit`, `child_mut`, `new`) cannot be called from outside
//! the crate.  If someone makes these methods public, the compile-fail
//! test in `tests/ui/private_methods.rs` would start compiling, causing
//! this test to fail.

#[test]
fn child_lease_methods_are_private() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/ui/private_methods.rs");
}
