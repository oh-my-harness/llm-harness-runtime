//! `session-viewer` CLI: serve a sessions directory in the browser.
//!
//! Usage:
//!   session-viewer <dir> [--port <PORT>]
//!
//! `<dir>` may be a sessions root (containing `<session_id>/` subdirs) or a
//! single session directory (containing `meta.json` + `entries.jsonl`).

use std::path::PathBuf;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: session-viewer <dir> [--port <PORT>]");
        std::process::exit(2);
    }

    let mut dir: Option<PathBuf> = None;
    let mut port: u16 = 0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or_else(|| anyhow::anyhow!("--port requires a number"))?;
            }
            "-h" | "--help" => {
                println!("usage: session-viewer <dir> [--port <PORT>]");
                return Ok(());
            }
            s if !s.starts_with('-') => {
                dir = Some(PathBuf::from(s));
            }
            other => {
                anyhow::bail!("unknown argument: {other}");
            }
        }
        i += 1;
    }

    let dir = dir.ok_or_else(|| anyhow::anyhow!("missing <dir> argument"))?;
    session_viewer::serve_on(&dir, port).await
}
