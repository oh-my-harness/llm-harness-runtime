//! API boundary verification for `ChildLease` opacity.
//!
//! Uses a **trusted compile-fail gate** with positive and negative controls:
//!
//! 1. **Positive control**: a temp crate that uses only the public API
//!    (`SpawnAdmission::into_lease`, `ChildReaper::global`, etc.) must
//!    compile successfully with `--offline`.  This verifies the dependency
//!    is healthy and the environment is clean.
//!
//! 2. **Negative controls**: separate temp binaries that each attempt to
//!    access ONE private method, field, or trait.  Each must fail to
//!    compile, and the stderr must contain a privacy-related diagnostic
//!    (E0624, E0616, E0451, or "private").  If any target is made public,
//!    the corresponding binary compiles, no privacy error appears, and the
//!    test fails.
//!
//! All builds use `--offline` to prevent network access during tests.

use std::process::Command;

/// Set up a temp crate with multiple binaries, each testing one API boundary.
/// Returns the temp dir path and the crate dir path.
struct ForgeCrate {
    _tmp: tempfile::TempDir,
    dir: std::path::PathBuf,
    target_dir: std::path::PathBuf,
}

impl ForgeCrate {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dir = tmp.path().join("forge_test");
        let target_dir = tmp.path().join("target");
        std::fs::create_dir_all(&dir).expect("create crate dir");
        std::fs::create_dir_all(dir.join("src").join("bin")).expect("create bin dir");

        let crate_path =
            std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("canonicalize crate path");

        let cargo_toml = format!(
            r#"[package]
name = "forge_test"
version = "0.1.0"
edition = "2021"

[dependencies]
llm-harness-runtime-sandbox-os = {{ path = "{}" }}
"#,
            crate_path.to_str().unwrap()
        );
        std::fs::write(dir.join("Cargo.toml"), cargo_toml).expect("write Cargo.toml");

        Self {
            _tmp: tmp,
            dir,
            target_dir,
        }
    }

    /// Write a binary source file.
    fn write_bin(&self, name: &str, src: &str) {
        std::fs::write(
            self.dir.join("src").join("bin").join(format!("{name}.rs")),
            src,
        )
        .unwrap_or_else(|e| panic!("write {name}.rs: {e}"));
    }

    /// Build a specific binary with `--offline`.  Returns (success, stderr).
    fn build_bin(&self, name: &str) -> (bool, String) {
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["build", "--offline", "--bin", name])
            .current_dir(&self.dir)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env(
                "PYO3_PYTHON",
                std::env::var("PYO3_PYTHON").unwrap_or_default(),
            )
            .output()
            .unwrap_or_else(|e| panic!("cargo build --bin {name}: {e}"));

        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        (output.status.success(), stderr)
    }
}

/// Check that stderr contains a privacy-related diagnostic.
fn stderr_has_privacy_error(stderr: &str) -> bool {
    stderr.contains("private")
        || stderr.contains("E0624")
        || stderr.contains("E0616")
        || stderr.contains("E0451")
        || stderr.contains("E0423")
        || stderr.contains("method not found")
}

/// Positive control: uses only the public API.  Must compile successfully.
#[test]
fn positive_control_compiles() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "positive",
        r#"
// Uses ONLY the public API surface.
use llm_harness_runtime_sandbox_os::{
    ChildLease, ChildReaper, SpawnAdmission, drain_child_output_guarded,
};
fn _check_types(_lease: ChildLease, _admission: SpawnAdmission) {}
fn _check_fns() {
    let _ = ChildReaper::global;
    let _ = ChildReaper::try_admit;
    let _ = ChildReaper::is_healthy;
    let _ = ChildReaper::metrics;
    let _ = SpawnAdmission::into_lease;
    let _ = drain_child_output_guarded;
}
fn main() {}
"#,
    );

    let (ok, stderr) = crate_.build_bin("positive");
    assert!(
        ok,
        "positive control must compile — if this fails, the dependency \
         or environment is broken, not the API boundary.\nstderr: {stderr}"
    );
}

/// Negative control: `confirm_exit` must be private.
#[test]
fn neg_confirm_exit_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_confirm_exit",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge(lease: &mut ChildLease) { lease.confirm_exit(); }
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_confirm_exit");
    assert!(!ok, "confirm_exit must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from privacy, not unrelated error\nstderr: {stderr}"
    );
}

/// Negative control: `wait` must be private.
#[test]
fn neg_wait_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_wait",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge(lease: &mut ChildLease) { let _ = lease.wait(); }
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_wait");
    assert!(!ok, "wait must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from privacy\nstderr: {stderr}"
    );
}

/// Negative control: `kill_and_wait` must be private.
#[test]
fn neg_kill_and_wait_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_kill_and_wait",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge(lease: &mut ChildLease) { lease.kill_and_wait(); }
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_kill_and_wait");
    assert!(!ok, "kill_and_wait must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from privacy\nstderr: {stderr}"
    );
}

/// Negative control: `take_stdout` must be private.
#[test]
fn neg_take_stdout_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_take_stdout",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge(lease: &mut ChildLease) { lease.take_stdout(); }
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_take_stdout");
    assert!(!ok, "take_stdout must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from privacy\nstderr: {stderr}"
    );
}

/// Negative control: `take_stderr` must be private.
#[test]
fn neg_take_stderr_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_take_stderr",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge(lease: &mut ChildLease) { lease.take_stderr(); }
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_take_stderr");
    assert!(!ok, "take_stderr must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from privacy\nstderr: {stderr}"
    );
}

/// Negative control: struct fields must be private (cannot construct
/// `ChildLease` directly).
#[test]
fn neg_field_construction_is_private() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_fields",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _forge() {
    let _ = ChildLease {
        id: 0,
        core: None,
        stdout: None,
        stderr: None,
        confirmed: false,
    };
}
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_fields");
    assert!(!ok, "ChildLease fields must be private — build succeeded");
    assert!(
        stderr_has_privacy_error(&stderr),
        "failure must be from private fields\nstderr: {stderr}"
    );
}

/// Negative control: `ChildLease` must not be `Clone`.
/// Uses an explicit trait bound so the failure is about `Clone`, not
/// about reference auto-clone (`&T: Clone` always holds).
#[test]
fn neg_child_lease_is_not_clone() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_clone",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _requires_clone<T: Clone>(_: &T) {}
fn _forge(lease: &ChildLease) {
    _requires_clone(lease);
}
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_clone");
    assert!(!ok, "ChildLease must not be Clone — build succeeded");
    assert!(
        stderr.contains("trait") || stderr.contains("E0277") || stderr.contains("not satisfied"),
        "failure must be from missing Clone trait bound\nstderr: {stderr}"
    );
}

/// Negative control: `ChildLease` must not be `Copy`.
#[test]
fn neg_child_lease_is_not_copy() {
    let crate_ = ForgeCrate::new();
    crate_.write_bin(
        "neg_copy",
        r#"
use llm_harness_runtime_sandbox_os::ChildLease;
fn _requires_copy<T: Copy>(_: &T) {}
fn _forge(lease: &ChildLease) {
    _requires_copy(lease);
}
fn main() {}
"#,
    );
    let (ok, stderr) = crate_.build_bin("neg_copy");
    assert!(!ok, "ChildLease must not be Copy — build succeeded");
    assert!(
        stderr.contains("trait") || stderr.contains("E0277") || stderr.contains("not satisfied"),
        "failure must be from missing Copy trait bound\nstderr: {stderr}"
    );
}

/// Verify the positive public API surface exists (compile-time check
/// in the test crate itself).
#[test]
fn public_api_surface() {
    use llm_harness_runtime_sandbox_os::{
        ChildLease, ChildReaper, SpawnAdmission, drain_child_output_guarded,
    };
    let _ = std::marker::PhantomData::<ChildLease>;
    let _ = SpawnAdmission::into_lease;
    let _ = ChildReaper::global;
    let _ = ChildReaper::try_admit;
    let _ = ChildReaper::is_healthy;
    let _ = ChildReaper::metrics;
    let _ = drain_child_output_guarded;
}
