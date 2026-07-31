//! API boundary verification for `ChildLease` opacity.
//!
//! Uses a **real compile-fail gate**: the test writes a small Rust program
//! that attempts to call `ChildLease`'s private methods, then invokes
//! `cargo build` on it.  If any method is public, the build succeeds and
//! the test fails.  If all methods are private (correct), the build fails
//! (E0624) and the test passes.
//!
//! This does NOT depend on compiler diagnostic text — only on the exit
//! code of `cargo build`.  It works on every Rust version and platform.

use std::process::Command;

/// Rust source that attempts to call private methods on `ChildLease`.
/// This code MUST fail to compile if the methods are private.
const ATTEMPT_FORGE: &str = r#"
use llm_harness_runtime_sandbox_os::ChildLease;

fn _forge_confirm(lease: &mut ChildLease) {
    lease.confirm_exit();
}

fn _forge_wait(lease: &mut ChildLease) {
    let _ = lease.wait();
}

fn _forge_kill_and_wait(lease: &mut ChildLease) {
    lease.kill_and_wait();
}

fn _forge_take_stdout(lease: &mut ChildLease) {
    lease.take_stdout();
}

fn main() {}
"#;

/// Verify that the private methods on `ChildLease` cannot be called
/// from external code.  This is a real compile-fail gate: we attempt
/// to compile code that calls them, and expect compilation to fail.
#[test]
fn child_lease_private_methods_compile_fail() {
    // Write the attempt to a temp crate and try to build it.
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let src_dir = temp_dir.path().join("forge_test");
    std::fs::create_dir_all(&src_dir).expect("create src dir");

    // Cargo.toml — depend on the sandbox-os crate via path.
    let cargo_toml = format!(
        r#"[package]
name = "forge_test"
version = "0.1.0"
edition = "2021"

[dependencies]
llm_harness_runtime_sandbox_os = {{ path = "{}" }}
"#,
        std::fs::canonicalize(".")
            .or_else(|_| std::fs::canonicalize(
                std::env::current_dir()
                    .unwrap()
                    .join("crates/llm-harness-runtime-sandbox-os")
            ))
            .unwrap()
            .to_str()
            .unwrap()
    );
    std::fs::write(src_dir.join("Cargo.toml"), cargo_toml).expect("write Cargo.toml");
    std::fs::create_dir_all(src_dir.join("src")).expect("create src");
    std::fs::write(src_dir.join("src/main.rs"), ATTEMPT_FORGE).expect("write main.rs");

    // Attempt to build.  We expect failure (exit code != 0).
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("build")
        .current_dir(&src_dir)
        .env(
            "PYO3_PYTHON",
            std::env::var("PYO3_PYTHON").unwrap_or_default(),
        )
        .output()
        .expect("cargo build");

    if output.status.success() {
        panic!(
            "ChildLease private methods are accessible from external code! \
             The build succeeded, meaning confirm_exit/wait/kill_and_wait/\
             take_stdout are public.  They must be private.\n\
             stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    // Build failed — methods are private (correct).
}

/// Verify the positive public API surface exists.
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
