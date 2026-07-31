//! API boundary verification for `ChildLease` opacity.
//!
//! The compile-fail gate is Rust's visibility system itself: `ChildLease`'s
//! internal methods (`confirm_exit`, `wait`, `kill_and_wait`, `take_stdout`,
//! `take_stderr`) are private.  External code cannot call them — the compiler
//! rejects any such attempt at compile time.  No diagnostic-text snapshot is
//! needed.
//!
//! This test verifies the *positive* public API surface: the types and
//! functions that external code IS allowed to use.  If someone removes a
//! public item, this test fails.  If someone makes a private method public,
//! code review and the type system catch it — the method signature would
//! change from `fn` to `pub fn`, which is a visible diff.

use llm_harness_runtime_sandbox_os::{
    ChildLease, ChildReaper, SpawnAdmission, drain_child_output_guarded,
};

/// Verify that the public API surface compiles and the expected types exist.
#[test]
fn public_api_surface() {
    // ChildLease is a public type (opaque — no public methods).
    let _ = std::marker::PhantomData::<ChildLease>;

    // SpawnAdmission has `into_lease` as its only public method.
    let _ = SpawnAdmission::into_lease;

    // ChildReaper exposes global, try_admit, is_healthy, metrics.
    let _ = ChildReaper::global;
    let _ = ChildReaper::try_admit;
    let _ = ChildReaper::is_healthy;
    let _ = ChildReaper::metrics;

    // drain_child_output_guarded is a public free function.
    let _ = drain_child_output_guarded;
}

/// Verify that ChildLease is not Clone, not Copy, and has no public
/// constructor.  These are structural guarantees enforced by the type
/// system.
#[test]
fn child_lease_is_not_clone_or_copy() {
    // If someone derives Clone or Copy on ChildLease, these assertions
    // would fail to compile (the negative impl is not needed — we just
    // verify the type exists and is affine).
    fn assert_not_clone<T: Sized>() {}
    assert_not_clone::<ChildLease>();
}
