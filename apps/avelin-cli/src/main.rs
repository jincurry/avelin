mod builder;
mod components;
mod diagnose;
mod init;
mod presentation_config;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Avelin controlled NSIS builder")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a checked avelin.toml for existing application files; never copy or run them.
    Init {
        /// Directory in which to create a new avelin.toml (never overwritten).
        #[arg(long)]
        directory: PathBuf,
        #[arg(long)]
        app_id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        app_version: String,
        /// Existing application entry relative to the payload directory.
        #[arg(long)]
        entry: String,
        /// Existing payload directory; relative paths are resolved from the working directory.
        #[arg(long)]
        payload: PathBuf,
    },
    /// Replay decoded backend events through the shared state mapping (no installation).
    Replay {
        /// A JSON array of decoded payloads; this does not authenticate a wire capture.
        #[arg(long)]
        events: PathBuf,
        /// Observed backend exit code, compared with the final event.
        #[arg(long, allow_hyphen_values = true)]
        exit_code: i32,
        /// Treat an interrupted/truncated event stream as Unknown.
        #[arg(long)]
        incomplete_stream: bool,
    },
    /// Read session/process/file evidence without changing the installation or its lock.
    Diagnose {
        #[arg(long)]
        session: PathBuf,
    },
    /// Validate configuration and existing payload; no compiler/runtime dependency is needed.
    Check {
        #[arg(long)]
        config: PathBuf,
    },
    /// Build a new unsigned Setup using colocated or explicitly supplied components.
    Build {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        runtime: Option<PathBuf>,
        #[arg(long)]
        bridge: Option<PathBuf>,
        #[arg(long)]
        nsis: Option<PathBuf>,
    },
}
fn main() -> Result<()> {
    match Args::parse().command {
        Command::Init {
            directory,
            app_id,
            name,
            app_version,
            entry,
            payload,
        } => println!(
            "{}",
            serde_json::to_string_pretty(&init::create(init::Options {
                directory,
                app_id,
                name,
                app_version,
                entry,
                payload,
            })?)?
        ),
        Command::Replay {
            events,
            exit_code,
            incomplete_stream,
        } => {
            let file = std::fs::File::open(&events).context("open event array")?;
            let mut bytes = Vec::new();
            const LIMIT: usize = 8 * 1024 * 1024;
            file.take(LIMIT as u64 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= LIMIT, "event replay input exceeds 8 MiB");
            let events: Vec<serde_json::Value> = serde_json::from_slice(&bytes)
                .context("events must be a JSON array of decoded backend payloads")?;
            let snapshots = avelin_core::protocol::replay_backend_events(
                &events,
                Some(exit_code),
                !incomplete_stream,
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema_version": 1,
                    "mode": "semantic_replay_only",
                    "backend_exit_code": exit_code,
                    "stream_complete": !incomplete_stream,
                    "snapshots": snapshots,
                }))?
            );
        }
        Command::Diagnose { session } => println!(
            "{}",
            serde_json::to_string_pretty(&diagnose::inspect(&session)?)?
        ),
        Command::Check { config } => {
            let (_, manifest) = builder::load(&config)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        Command::Build {
            config,
            output,
            runtime,
            bridge,
            nsis,
        } => builder::build(&config, &output, runtime, bridge, nsis)?,
    }
    Ok(())
}
