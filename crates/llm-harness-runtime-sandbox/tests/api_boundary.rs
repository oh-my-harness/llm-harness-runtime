#![cfg(feature = "api-boundary-tests")]

//! Compile-time checks for the child-reaper public API boundary.
//!
//! One temporary crate and one target directory are reused for every probe.
//! The positive control is checked first. Each negative binary then has to
//! produce the expected rustc JSON diagnostic at that binary's primary span.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct ForgeCrate {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    target_dir: PathBuf,
}

impl ForgeCrate {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("create forge temp dir");
        let dir = tmp.path().join("forge_test");
        let target_dir = tmp.path().join("target");
        std::fs::create_dir_all(dir.join("src/bin")).expect("create forge source directory");

        let crate_path =
            std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).expect("canonicalize crate path");
        let serialized_path = serde_json::to_string(&crate_path.to_string_lossy())
            .expect("serialize dependency path as a TOML-compatible basic string");
        let cargo_toml = format!(
            "[package]\n\
             name = \"forge_test\"\n\
             version = \"0.1.0\"\n\
             edition = \"2021\"\n\n\
             [dependencies]\n\
             llm-harness-runtime-sandbox = {{ path = {serialized_path} }}\n"
        );
        std::fs::write(dir.join("Cargo.toml"), cargo_toml).expect("write forge Cargo.toml");

        Self {
            _tmp: tmp,
            dir,
            target_dir,
        }
    }

    fn write_bin(&self, name: &str, source: &str) {
        std::fs::write(self.bin_path(name), source)
            .unwrap_or_else(|error| panic!("write {name}.rs: {error}"));
    }

    fn bin_path(&self, name: &str) -> PathBuf {
        self.dir.join("src/bin").join(format!("{name}.rs"))
    }

    fn check_bin(&self, name: &str) -> Output {
        Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(["check", "--offline", "--message-format=json", "--bin", name])
            .current_dir(&self.dir)
            .env("CARGO_TARGET_DIR", &self.target_dir)
            .env(
                "PYO3_PYTHON",
                std::env::var("PYO3_PYTHON").unwrap_or_default(),
            )
            .output()
            .unwrap_or_else(|error| panic!("cargo check --bin {name}: {error}"))
    }

    fn assert_positive(&self, name: &str) {
        let output = self.check_bin(name);
        assert!(
            output.status.success(),
            "positive API control {name} must compile\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn assert_diagnostic(&self, probe: Probe, output: Output) {
        assert!(
            !output.status.success(),
            "negative API probe {} unexpectedly compiled",
            probe.name
        );

        let expected_file = self.bin_path(probe.name);
        let matched = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|message| diagnostic_matches(&message, &expected_file, &probe));

        assert!(
            matched,
            "probe {} did not emit {} for `{}` at its own primary span\nstdout:\n{}\nstderr:\n{}",
            probe.name,
            probe.code,
            probe.symbol,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

struct Probe {
    name: &'static str,
    source: String,
    code: &'static str,
    symbol: &'static str,
}

fn diagnostic_matches(message: &serde_json::Value, expected_file: &Path, probe: &Probe) -> bool {
    if message["reason"] != "compiler-message"
        || message["target"]["name"] != probe.name
        || message["message"]["code"]["code"] != probe.code
    {
        return false;
    }

    let diagnostic = &message["message"];
    let mentions_symbol = diagnostic["message"]
        .as_str()
        .is_some_and(|text| text.contains(probe.symbol));
    let has_expected_primary_span =
        diagnostic["spans"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|span| {
                span["is_primary"] == true
                    && span["file_name"]
                        .as_str()
                        .is_some_and(|file| same_source_file(Path::new(file), expected_file))
            });

    mentions_symbol && has_expected_primary_span
}

fn same_source_file(reported: &Path, expected: &Path) -> bool {
    std::fs::canonicalize(reported)
        .ok()
        .zip(std::fs::canonicalize(expected).ok())
        .is_some_and(|(reported, expected)| reported == expected)
        || reported.file_name() == expected.file_name()
}

#[test]
fn child_reaper_api_boundaries_are_enforced() {
    let forge = ForgeCrate::new();
    forge.write_bin(
        "positive",
        r#"
use llm_harness_runtime_sandbox::{
    ChildLease, ChildReaper, SpawnAdmission, drain_child_output_guarded,
};
fn check_types(_: Option<ChildLease>, _: Option<SpawnAdmission>) {}
fn check_api() {
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
    forge.assert_positive("positive");

    // Run all negative probes in parallel with per-probe target dirs so
    // cargo doesn't serialize on the target-dir lock.
    let probe_list = probes();
    let forge_dir = forge.dir.clone();
    let target_dir = forge.target_dir.clone();
    let pyo3_python = std::env::var("PYO3_PYTHON").unwrap_or_default();

    let handles: Vec<_> = probe_list
        .into_iter()
        .map(|probe| {
            let forge_dir = forge_dir.clone();
            let target_dir = target_dir.clone();
            let pyo3_python = pyo3_python.clone();
            std::thread::spawn(move || {
                let name = probe.name;
                let bin_path = forge_dir.join("src/bin").join(format!("{name}.rs"));
                std::fs::write(&bin_path, &probe.source)
                    .unwrap_or_else(|error| panic!("write {name}.rs: {error}"));
                let target_dir = target_dir.clone();
                let output =
                    Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
                        .args(["check", "--offline", "--message-format=json", "--bin", name])
                        .current_dir(&forge_dir)
                        .env("CARGO_TARGET_DIR", &target_dir)
                        .env("PYO3_PYTHON", &pyo3_python)
                        .output()
                        .unwrap_or_else(|error| panic!("cargo check --bin {name}: {error}"));
                (probe, output)
            })
        })
        .collect();

    for handle in handles {
        let (probe, output) = handle.join().expect("probe thread panicked");
        forge.assert_diagnostic(probe, output);
    }
}

fn probes() -> Vec<Probe> {
    vec![
        method_probe(
            "child_lease_confirm_exit",
            "confirm_exit",
            "fn probe(value: &mut ChildLease) { value.confirm_exit(); }",
        ),
        method_probe(
            "child_lease_wait",
            "wait",
            "fn probe(value: &mut ChildLease) { let _ = value.wait(); }",
        ),
        method_probe(
            "child_lease_kill_and_wait",
            "kill_and_wait",
            "fn probe(value: &mut ChildLease) { let _ = value.kill_and_wait(); }",
        ),
        method_probe(
            "child_lease_take_stdout",
            "take_stdout",
            "fn probe(value: &mut ChildLease) { let _ = value.take_stdout(); }",
        ),
        method_probe(
            "child_lease_take_stderr",
            "take_stderr",
            "fn probe(value: &mut ChildLease) { let _ = value.take_stderr(); }",
        ),
        field_probe("child_lease_id", "ChildLease", "id"),
        field_probe("child_lease_core", "ChildLease", "core"),
        field_probe("child_lease_stdout", "ChildLease", "stdout"),
        field_probe("child_lease_stderr", "ChildLease", "stderr"),
        field_probe("child_lease_confirmed", "ChildLease", "confirmed"),
        Probe {
            name: "child_lease_construction",
            source: r#"
use llm_harness_runtime_sandbox::ChildLease;
fn probe() {
    let _ = ChildLease { id: 0, core: None, stdout: None, stderr: None, confirmed: false };
}
fn main() {}
"#
            .to_string(),
            code: "E0451",
            symbol: "id",
        },
        trait_probe("child_lease_clone", "ChildLease", "Clone"),
        trait_probe("child_lease_copy", "ChildLease", "Copy"),
        field_probe("spawn_admission_core", "SpawnAdmission", "core"),
        field_probe("spawn_admission_epoch", "SpawnAdmission", "epoch"),
        Probe {
            name: "spawn_admission_construction",
            source: r#"
use llm_harness_runtime_sandbox::SpawnAdmission;
fn probe() { let _ = SpawnAdmission { core: None, epoch: 0 }; }
fn main() {}
"#
            .to_string(),
            code: "E0451",
            symbol: "core",
        },
        trait_probe("spawn_admission_clone", "SpawnAdmission", "Clone"),
        trait_probe("spawn_admission_copy", "SpawnAdmission", "Copy"),
    ]
}

fn method_probe(name: &'static str, symbol: &'static str, body: &'static str) -> Probe {
    let source = format!("use llm_harness_runtime_sandbox::ChildLease;\n{body}\nfn main() {{}}\n");
    Probe {
        name,
        source,
        code: "E0624",
        symbol,
    }
}

fn field_probe(name: &'static str, type_name: &'static str, field: &'static str) -> Probe {
    let source = format!(
        "use llm_harness_runtime_sandbox::{type_name};\n\
         fn probe(value: &{type_name}) {{ let _ = &value.{field}; }}\n\
         fn main() {{}}\n"
    );
    Probe {
        name,
        source,
        code: "E0616",
        symbol: field,
    }
}

fn trait_probe(name: &'static str, type_name: &'static str, trait_name: &'static str) -> Probe {
    let source = format!(
        "use llm_harness_runtime_sandbox::{type_name};\n\
         fn require_trait<T: {trait_name}>() {{}}\n\
         fn probe() {{ require_trait::<{type_name}>(); }}\n\
         fn main() {{}}\n"
    );
    Probe {
        name,
        source,
        code: "E0277",
        symbol: trait_name,
    }
}
