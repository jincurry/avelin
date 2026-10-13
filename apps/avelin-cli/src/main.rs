mod builder;
mod components;
mod diagnose;
mod init;
mod native_builder;
mod presentation_config;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand, ValueEnum};
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Avelin Windows installer builder")]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum Engine {
    #[default]
    Nsis,
    Rust,
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
        /// Installation backend; the runtime and package configuration are shared.
        #[arg(long, value_enum, default_value_t = Engine::Nsis)]
        engine: Engine,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        runtime: Option<PathBuf>,
        /// Rust worker executable; otherwise resolved beside avelin-cli.exe.
        #[arg(long)]
        rust_worker: Option<PathBuf>,
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
            engine,
            config,
            output,
            runtime,
            rust_worker,
            bridge,
            nsis,
        } => {
            validate_engine_components(engine, &rust_worker, &bridge, &nsis)?;
            match engine {
                Engine::Nsis => builder::build(&config, &output, runtime, bridge, nsis)?,
                Engine::Rust => native_builder::build(&config, &output, runtime, rust_worker)?,
            }
        }
    }
    Ok(())
}

fn validate_engine_components(
    engine: Engine,
    rust_worker: &Option<PathBuf>,
    bridge: &Option<PathBuf>,
    nsis: &Option<PathBuf>,
) -> Result<()> {
    match engine {
        Engine::Nsis => ensure!(
            rust_worker.is_none(),
            "--rust-worker requires --engine rust"
        ),
        Engine::Rust => ensure!(
            bridge.is_none() && nsis.is_none(),
            "--engine rust does not accept --bridge or --nsis"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    #[test]
    fn existing_build_defaults_to_nsis_and_rust_is_explicit() {
        let parse = |extra: &[&str]| {
            Args::try_parse_from(
                [
                    "avelin-cli",
                    "build",
                    "--config",
                    "avelin.toml",
                    "--output",
                    "Setup.exe",
                ]
                .into_iter()
                .chain(extra.iter().copied()),
            )
        };
        assert!(matches!(
            parse(&[]).unwrap().command,
            Command::Build {
                engine: Engine::Nsis,
                ..
            }
        ));
        assert!(matches!(
            parse(&["--engine", "rust"]).unwrap().command,
            Command::Build {
                engine: Engine::Rust,
                ..
            }
        ));
        assert!(parse(&["--engine", "unknown"]).is_err());
    }

    #[test]
    fn engine_specific_arguments_cannot_be_silently_ignored() {
        let some = Some(PathBuf::from("component.exe"));
        assert!(validate_engine_components(Engine::Nsis, &some, &None, &None).is_err());
        assert!(validate_engine_components(Engine::Rust, &None, &some, &None).is_err());
        assert!(validate_engine_components(Engine::Rust, &some, &None, &some).is_err());
        assert!(validate_engine_components(Engine::Rust, &some, &None, &None).is_ok());
        assert!(validate_engine_components(Engine::Nsis, &None, &some, &some).is_ok());
    }
}
