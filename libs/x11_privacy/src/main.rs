//! Manual test tool.
//!
//!   x11-privacy status
//!   x11-privacy on [SECONDS] [--no-input] [--no-blackout] [--no-takeover]
//!                                    (default 15 s; Ctrl+C or SIGTERM also turn it off)
//!   x11-privacy recover              (undo after a crash / kill -9)
//!
//! Safety: `on` always turns itself off after SECONDS, and on Ctrl+C / SIGTERM, so you cannot
//! lock yourself out of a machine you are sitting at. While it is on, a key press or click on the
//! physical keyboard/mouse locks the screen and shows the lock screen; unlocking it there ends it.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use x11_privacy::{recover, status, Options, Phase, PrivacyGuard};

static STOP: AtomicBool = AtomicBool::new(false);

/// Prints the library's log to stderr with a timestamp, so test runs show what it decided.
struct StderrLogger(Instant);

impl log::Log for StderrLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, r: &log::Record) {
        if self.enabled(r.metadata()) {
            eprintln!("[{:7.2}s {}] {}", self.0.elapsed().as_secs_f32(), r.level(), r.args());
        }
    }
    fn flush(&self) {}
}

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        unsafe {
            libc::signal(sig, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
    }
}

fn state_file() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("x11-privacy.state")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let res = match args.first().map(String::as_str) {
        Some("status") => status().map(|s| print!("{s}")),
        Some("recover") => recover(&state_file()).map(|r| println!("recovered: {r}")),
        Some("on") => {
            let logger: &'static StderrLogger = Box::leak(Box::new(StderrLogger(Instant::now())));
            let _ = log::set_logger(logger).map(|()| log::set_max_level(log::LevelFilter::Info));
            install_signal_handlers();
            let secs = args.iter().skip(1).find_map(|a| a.parse::<u64>().ok()).unwrap_or(15);
            let taken_over = Arc::new(AtomicBool::new(false));
            let opts = Options {
                blackout: !flag("--no-blackout"),
                block_input: !flag("--no-input"),
                local_takeover: !flag("--no-takeover"),
                on_takeover: Some({
                    let taken_over = taken_over.clone();
                    Box::new(move || taken_over.store(true, Ordering::SeqCst))
                }),
                ..Default::default()
            };
            println!("supported: {}", x11_privacy::is_supported());
            println!("{opts:?}");
            match PrivacyGuard::turn_on(opts, state_file()) {
                Ok(guard) => {
                    println!("privacy mode ON for {secs}s (Ctrl+C to stop)");
                    if let Ok(s) = status() {
                        print!("{s}");
                    }
                    let end = Instant::now() + Duration::from_secs(secs);
                    let mut phase = guard.phase();
                    while Instant::now() < end
                        && !STOP.load(Ordering::SeqCst)
                        && !taken_over.load(Ordering::SeqCst)
                    {
                        std::thread::sleep(Duration::from_millis(100));
                        let p = guard.phase();
                        if p != phase {
                            println!("phase: {p:?}");
                            if let Ok(s) = status() {
                                print!("{s}");
                            }
                            phase = p;
                        }
                    }
                    if taken_over.load(Ordering::SeqCst) {
                        println!("LOCAL TAKEOVER: unlocked at the console (RustDesk would disconnect the remote side now)");
                    } else if phase == Phase::Active || phase == Phase::Suspended {
                        println!("stopping");
                    }
                    let r = guard.turn_off();
                    println!("privacy mode OFF");
                    r
                }
                Err(e) => Err(e),
            }
        }
        _ => {
            eprintln!(
                "usage: x11-privacy status | on [SECONDS] [--no-input] [--no-blackout] [--no-takeover] | recover"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = res {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
