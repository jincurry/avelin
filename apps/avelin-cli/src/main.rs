mod build_output;
mod components;
mod config;
mod diagnose;
mod init;
mod native_builder;
mod presentation_config;
use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{io::Read, path::PathBuf};

#[derive(Parser)]
#[command(version, about = "Avelin Windows installer builder")]
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
    /// Inspect an interrupted Rust transaction with its original Setup; apply only when requested.
    Recover {
        #[arg(long)]
        session: PathBuf,
        /// Original Rust Setup containing the matching runtime and payload.
        #[arg(long)]
        setup: PathBuf,
        /// Explicitly request the default read-only check.
        #[arg(long, conflicts_with = "apply")]
        check: bool,
        /// Apply the recovery decision after validating the retained session.
        #[arg(long)]
        apply: bool,
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
        /// Rust worker executable; otherwise resolved beside avelin-cli.exe.
        #[arg(long)]
        rust_worker: Option<PathBuf>,
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
        Command::Recover {
            session,
            setup,
            check: _,
            apply,
        } => {
            avelin_core::reject_reparse(&setup)?;
            avelin_core::reject_reparse(&session)?;
            // Keep ordinary absolute Windows paths: canonicalize adds a verbatim
            // prefix that the Setup path contract deliberately does not accept.
            let setup = std::path::absolute(setup).context("locate original Rust Setup")?;
            let session =
                std::path::absolute(session).context("locate retained recovery session")?;
            ensure!(
                setup.is_file(),
                "--setup must name an existing Setup executable"
            );
            ensure!(
                session.is_dir(),
                "--session must name an existing session directory"
            );
            let mut command = std::process::Command::new(&setup);
            command.args(["--silent", "--operation", "recover", "--recovery-session"]);
            command.arg(&session);
            if apply {
                command.arg("--recovery-apply");
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW; all stdio remains inherited.
            }
            let status = command
                .status()
                .context("run recovery through original Setup")?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Command::Check { config } => {
            let (_, manifest) = config::load(&config)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        Command::Build {
            config,
            output,
            runtime,
            rust_worker,
        } => native_builder::build(&config, &output, runtime, rust_worker)?,
    }
    Ok(())
}

#[cfg(test)]
mod argument_tests {
    use super::*;

    #[test]
    fn build_accepts_only_current_component_options() {
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
                runtime: None,
                rust_worker: None,
                ..
            }
        ));
        let Command::Build {
            runtime,
            rust_worker,
            ..
        } = parse(&["--runtime", "runtime.exe", "--rust-worker", "worker.exe"])
            .unwrap()
            .command
        else {
            panic!("expected build");
        };
        assert_eq!(runtime, Some(PathBuf::from("runtime.exe")));
        assert_eq!(rust_worker, Some(PathBuf::from("worker.exe")));
        // Removed command-line options must fail instead of silently changing behavior.
        for option in ["--engine", "--bridge", "--nsis"] {
            assert!(parse(&[option, "unused"]).is_err(), "accepted {option}");
        }
    }

    #[test]
    fn recovery_is_read_only_unless_apply_is_explicit() {
        let parse = |extra: &[&str]| {
            Args::try_parse_from(
                [
                    "avelin-cli",
                    "recover",
                    "--session",
                    "session",
                    "--setup",
                    "Setup.exe",
                ]
                .into_iter()
                .chain(extra.iter().copied()),
            )
        };
        assert!(matches!(
            parse(&[]).unwrap().command,
            Command::Recover { apply: false, .. }
        ));
        assert!(matches!(
            parse(&["--apply"]).unwrap().command,
            Command::Recover { apply: true, .. }
        ));
        assert!(parse(&["--check", "--apply"]).is_err());
    }
}
