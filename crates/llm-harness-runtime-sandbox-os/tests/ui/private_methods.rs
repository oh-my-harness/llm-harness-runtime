// This file should fail to compile because ChildLease's methods are private.
// If someone makes them public, trybuild will detect the change.

use llm_harness_runtime_sandbox_os::ChildLease;

fn _forge_confirm(lease: &mut ChildLease) {
    lease.confirm_exit();
}

fn _forge_child_mut(lease: &mut ChildLease) {
    lease.child_mut();
}

fn _forge_new(child: tokio::process::Child) {
    let _ = ChildLease::new(child);
}
