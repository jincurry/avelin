//! Internal native worker. A package is launched by the authenticated coordinator.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod worker;

fn main() {
    #[cfg(windows)]
    let code = worker::entry();
    #[cfg(not(windows))]
    let code = {
        eprintln!("The native installation worker requires Windows.");
        3
    };
    std::process::exit(code);
}
