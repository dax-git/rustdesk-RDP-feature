//! Manual test tool.
//!
//!   x11-privacy status
//!   x11-privacy on [SECONDS] [--no-input] [--no-blackout]   (default 15 s, then restores)
//!   x11-privacy recover                                     (undo after a crash)
//!
//! Safety: `on` always turns itself off after SECONDS, so you cannot lock yourself out
//! of a machine you are sitting at.

use std::{path::PathBuf, time::Duration};
use x11_privacy::{recover, status, Options, PrivacyGuard};

fn state_file() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("x11-privacy.state")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let res = match args.first().map(String::as_str) {
        Some("status") => status().map(|s| print!("{s}")),
        Some("recover") => recover(&state_file()).map(|r| println!("recovered: {r}")),
        Some("on") => {
            let secs = args.iter().skip(1).find_map(|a| a.parse::<u64>().ok()).unwrap_or(15);
            let opts = Options {
                blackout: !args.iter().any(|a| a == "--no-blackout"),
                block_input: !args.iter().any(|a| a == "--no-input"),
            };
            println!("supported: {}", x11_privacy::is_supported());
            match PrivacyGuard::turn_on(opts, state_file()) {
                Ok(guard) => {
                    println!("privacy mode ON for {secs}s ({opts:?})");
                    if let Ok(s) = status() {
                        print!("{s}");
                    }
                    std::thread::sleep(Duration::from_secs(secs));
                    let r = guard.turn_off();
                    println!("privacy mode OFF");
                    r
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            eprintln!("usage: x11-privacy status | on [SECONDS] [--no-input] [--no-blackout] | recover");
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
