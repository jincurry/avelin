//! Synthetic diagnostic exercise: no GUI, GPU fault, backend or installation.

#[path = "../src/runtime_log.rs"]
mod runtime_log;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, ValueEnum)]
enum Scenario {
    Normal,
    Storm,
}

#[derive(Parser)]
#[command(
    version,
    about = "Synthetic Avelin runtime diagnostic probe; does not initialize graphics or install anything"
)]
struct Args {
    #[arg(long, value_enum, default_value = "normal")]
    scenario: Scenario,
}

fn main() {
    // Clap handles help/version/invalid arguments before any log directory exists.
    let args = Args::parse();
    let registered = match runtime_log::initialize() {
        Ok(logger) => logger,
        Err(error) => {
            eprintln!("SYNTHETIC_DIAGNOSTICS_REGISTRATION_FAILED: {error}");
            std::process::exit(1);
        }
    };
    let registration_reused = runtime_log::initialize()
        .map(|again| std::ptr::eq(registered, again))
        .unwrap_or(false);

    log::warn!(target: "avelin::synthetic_probe", "synthetic warning: 中文安装诊断\nsecond line\0NUL preserved as JSON escape");
    log::error!(target: "avelin::synthetic_probe", "synthetic error: 窗口测试 😀; no actual graphics or installation failure");

    let scenario = match args.scenario {
        Scenario::Normal => "normal",
        Scenario::Storm => {
            // The payload exceeds the per-message limit and the sequence exceeds
            // the file limit. Counts remain observable once writes stop.
            let payload = "synthetic storm 中文😀 ".repeat(300);
            for sequence in 0..2000 {
                log::warn!(target: "avelin::synthetic_probe", "sequence={sequence} {payload}");
            }
            "storm"
        }
    };
    let snapshot = runtime_log::finish();
    let result = serde_json::json!({
        "schema_version": 1,
        "probe_kind": "synthetic_runtime_diagnostics",
        "scenario": scenario,
        "status": "PROBE_COMPLETED",
        "registration_reused": registration_reused,
        "graphics_initialized": false,
        "gpu_fault_injected": false,
        "installation_executed": false,
        "snapshot": snapshot,
        "exit_code_meaning": "0 means this synthetic exercise completed; injected Error records are expected and do not establish a task result"
    });
    println!("{result}");
}
