// This probe intentionally keeps a console for renderer diagnostics and errors.
// The product's GUI-subsystem executable must provide its own native error prompt.

use avelin_ui::{Locale, UiOptions};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct ProbeLogger {
    errors: AtomicUsize,
    warnings: AtomicUsize,
}
static LOGGER: ProbeLogger = ProbeLogger {
    errors: AtomicUsize::new(0),
    warnings: AtomicUsize::new(0),
};
impl log::Log for ProbeLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        if record.level() == log::Level::Error {
            self.errors.fetch_add(1, Ordering::Relaxed);
        } else {
            self.warnings.fetch_add(1, Ordering::Relaxed);
        }
        eprintln!(
            "AVELIN_UI_LOG level={} target={} message={}",
            record.level(),
            record.target(),
            record.args()
        );
    }
    fn flush(&self) {}
}

fn main() {
    // Argument handling intentionally precedes GPUI application/platform creation.
    let mut options = UiOptions::default();
    let mut smoke_duration = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--en" => options.locale = Locale::English,
            "--dark" => options.dark = true,
            "--smoke-seconds" => {
                let seconds = arguments.next().and_then(|value| value.parse::<u64>().ok());
                match seconds {
                    Some(seconds @ 1..=60) => smoke_duration = Some(Duration::from_secs(seconds)),
                    _ => {
                        eprintln!("--smoke-seconds requires an integer from 1 to 60.");
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "ui-smoke [--en] [--dark] [--smoke-seconds 1..60]\nCompact UI demonstration; no installation is performed.\nLogged framework errors cause exit 5; startup/loop errors cause exit 1."
                );
                return;
            }
            "--version" => {
                eprintln!("avelin-ui {} (gpui-kit 0.6.1)", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {
                eprintln!("Unknown option: {argument}");
                std::process::exit(2);
            }
        }
    }
    if let Err(error) = log::set_logger(&LOGGER) {
        eprintln!("AVELIN_UI_PROBE_LOGGER_FAILED: {error}");
        std::process::exit(5);
    }
    log::set_max_level(log::LevelFilter::Warn);
    let result = match smoke_duration {
        Some(duration) => avelin_ui::run_smoke(options, duration),
        None => avelin_ui::run(options),
    };
    let errors = LOGGER.errors.load(Ordering::Relaxed);
    eprintln!(
        "AVELIN_UI_PROBE_LOG_SUMMARY errors={} warnings={}",
        errors,
        LOGGER.warnings.load(Ordering::Relaxed)
    );
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
    if errors != 0 {
        std::process::exit(5);
    }
}
