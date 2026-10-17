//! Explicit operator authorization; this is intentionally not an agent tool.
use llm_harness_agent_team_studio::execution_policy;
use std::{io, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("authorize") if args.len() == 4 && args[3] == "--acknowledge-unrestricted-host-access" => {
            let grant = execution_policy::authorize(Path::new(&args[1]), &args[2])?;
            println!("Authorized unrestricted host execution for {} at {}. Stop/restart this Team to apply; no workflow is resumed automatically.", grant.team_id, grant.workspace.display());
        }
        Some("revoke") if args.len() == 3 => {
            execution_policy::revoke(Path::new(&args[1]), &args[2])?;
            println!("Grant revoked. Stop/restart the Team immediately to remove its existing host execution environment.");
        }
        Some("list") if args.len() == 2 => {
            println!("{}", serde_json::to_string_pretty(&execution_policy::grants(Path::new(&args[1]))?)?);
        }
        _ => return Err(io::Error::other("usage: team-execution authorize DATA_ROOT TEAM_ID --acknowledge-unrestricted-host-access\n       team-execution revoke DATA_ROOT TEAM_ID\n       team-execution list DATA_ROOT").into()),
    }
    Ok(())
}
