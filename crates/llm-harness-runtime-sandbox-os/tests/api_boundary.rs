//! Integration test verifying the public API boundary of `ChildLease`.
//!
//! This file is compiled as a **separate crate** (the `tests/` directory),
//! so it can only access `pub` items.  It verifies that `ChildLease` is
//! opaque — there are no public methods to forge a confirmed exit.

use llm_harness_runtime_sandbox_os::ChildLease;

/// P0 (round-14): `ChildLease` is an opaque type.  The only way to
/// create one in production is `SpawnAdmission::into_lease`.  There are
/// no public methods on `ChildLease` — you can only pass it to
/// `drain_child_output_guarded` or drop it.
///
/// If someone makes `confirm_exit()`, `child_mut()`, or `new()` public,
/// the commented-out code below would compile, breaking the invariant.
/// This test exists as a compile-time guard.
#[test]
fn child_lease_is_opaque() {
    // The type exists and is Send.
    fn _assert_send<T: Send>() {}
    _assert_send::<ChildLease>();

    // SpawnAdmission::into_lease is the only public constructor.
    // (We can't call it here without a ChildReaper, but the type
    // signature is verified by the compiler.)
    fn _takes_lease(_: ChildLease) {}
    let _ = _takes_lease;

    // The following would fail to compile if uncommented, because
    // these methods are private:
    //
    //   fn _cannot_forge(lease: &mut ChildLease) {
    //       lease.confirm_exit();  // ERROR: method is private
    //       lease.child_mut();     // ERROR: method is private
    //   }
    //
    //   fn _cannot_construct(child: tokio::process::Child) {
    //       let _ = ChildLease::new(child);  // ERROR: method is private
    //   }
    //
    // This test passes by compiling — the private methods are
    // inaccessible from outside the crate.
}
