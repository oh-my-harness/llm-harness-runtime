//! Manual snapshot retention operations; all paths supplied explicitly.
use llm_harness_agent_team_studio::snapshot_storage::{
    SnapshotPolicy, StorageLease, protect_run, retain, set_run_dependency,
};
use std::{io, path::Path};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let usage = "usage: snapshot-storage dry-run|cleanup REPO DATA_ROOT\n       snapshot-storage pin|unpin REPO RUN_UUID REASON\n       snapshot-storage depend|undepend REPO OWNER_RUN_UUID TARGET_RUN_UUID\n       snapshot-storage inspect REPO RUN_UUID";
    match args.first().map(String::as_str) {
        Some("dry-run" | "cleanup") if args.len() == 3 => {
            let policy = SnapshotPolicy::load(Path::new(&args[2]))?;
            let report = retain(Path::new(&args[1]), &policy.retention, args[0] == "dry-run")?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if report.incomplete || !report.errors.is_empty() {
                std::process::exit(2);
            }
        }
        Some("pin" | "unpin") if args.len() == 4 => {
            protect_run(
                Path::new(&args[1]),
                args[2].parse()?,
                &args[3],
                args[0] == "pin",
            )?;
        }
        Some("depend" | "undepend") if args.len() == 4 => {
            set_run_dependency(
                Path::new(&args[1]),
                args[2].parse()?,
                args[3].parse()?,
                args[0] == "depend",
            )?;
        }
        Some("inspect") if args.len() == 3 => {
            let _lease = StorageLease::inspection(Path::new(&args[1]))?;
            let run: uuid::Uuid = args[2].parse()?;
            let root = Path::new(&args[1])
                .join(".pipeline/snapshots")
                .join(run.to_string());
            if !root.is_dir() {
                return Err(
                    io::Error::new(io::ErrorKind::NotFound, "snapshot run not found").into(),
                );
            }
            println!(
                "Inspection lease held for {}. Press Enter after inspection to release.",
                root.display()
            );
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
        }
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, usage).into()),
    }
    Ok(())
}
