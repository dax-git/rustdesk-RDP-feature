//! Privacy mode for Linux X11 sessions.
//!
//! Like the macOS implementation: the physical monitors are blacked out through the CRTC gamma
//! ramp (the framebuffer, and so the capture, is unaffected) and physical keyboards/mice are
//! disabled through XInput, while remote input (XTEST) keeps working. See `libs/x11_privacy`.

use super::{PrivacyMode, PrivacyModeState};
use hbb_common::{anyhow::anyhow, config::Config, log, ResultType};
use std::{
    process::Command,
    time::{Duration, Instant},
};
use x11_privacy::{Options, PrivacyGuard};

pub const PRIVACY_MODE_IMPL: &str = "privacy_mode_impl_x11";

fn state_file() -> std::path::PathBuf {
    Config::path("privacy_mode_x11.state")
}

pub fn is_supported() -> bool {
    crate::platform::linux::is_x11() && x11_privacy::is_supported()
}

/// Undo a privacy mode left on by a crashed server process. Called when `--server` starts.
pub fn recover() {
    match x11_privacy::recover(&state_file()) {
        Ok(true) => log::info!("Restored the console left in privacy mode by a previous run"),
        Ok(false) => {}
        Err(e) => log::error!("Failed to restore the console from privacy mode: {}", e),
    }
}

/// Asks the screen locker over the session bus whether it is showing.
/// `None` if no known locker answered.
fn is_screen_locker_active() -> Option<bool> {
    const LOCKERS: &[(&str, &str)] = &[
        ("org.cinnamon.ScreenSaver", "/org/cinnamon/ScreenSaver"),
        ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
        ("org.mate.ScreenSaver", "/org/mate/ScreenSaver"),
        ("org.freedesktop.ScreenSaver", "/org/freedesktop/ScreenSaver"),
    ];
    let bus = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| {
        format!("unix:path=/run/user/{}/bus", unsafe { hbb_common::libc::getuid() })
    });
    for (dest, path) in LOCKERS {
        let out = Command::new("dbus-send")
            .env("DBUS_SESSION_BUS_ADDRESS", &bus)
            .args([
                "--session",
                "--print-reply=literal",
                "--reply-timeout=500",
                &format!("--dest={dest}"),
                path,
                &format!("{dest}.GetActive"),
            ])
            .output();
        if let Ok(out) = out {
            if out.status.success() {
                return Some(String::from_utf8_lossy(&out.stdout).contains("true"));
            }
        }
    }
    None
}

/// Used when the session ends with "lock after session end": waits (up to `timeout`) until the
/// lock screen is up, so that switching the monitors back on never shows the unlocked desktop.
pub fn wait_for_screen_locked(timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        match is_screen_locker_active() {
            Some(true) => {
                // Give the locker a moment to finish painting / fading in.
                std::thread::sleep(Duration::from_millis(500));
                log::info!("Screen locked after {:?}", start.elapsed());
                return;
            }
            Some(false) => std::thread::sleep(Duration::from_millis(100)),
            None => {
                // Unknown locker: we cannot tell, just give it a fixed time.
                log::warn!("No screen locker answered on D-Bus, waiting a fixed time");
                std::thread::sleep(timeout.min(Duration::from_secs(2)));
                return;
            }
        }
    }
    log::warn!("Screen was not locked within {:?}", timeout);
}

pub struct PrivacyModeImpl {
    impl_key: String,
    conn_id: i32,
    guard: Option<PrivacyGuard>,
}

impl PrivacyModeImpl {
    pub fn new(impl_key: &str) -> Self {
        Self {
            impl_key: impl_key.to_owned(),
            conn_id: 0,
            guard: None,
        }
    }

    fn release(&mut self) -> ResultType<()> {
        let res = match self.guard.take() {
            Some(guard) => guard.turn_off().map_err(|e| anyhow!(e.to_string())),
            None => Ok(()),
        };
        self.conn_id = 0;
        res
    }
}

impl PrivacyMode for PrivacyModeImpl {
    fn is_async_privacy_mode(&self) -> bool {
        false
    }

    fn init(&self) -> ResultType<()> {
        Ok(())
    }

    fn clear(&mut self) {
        if let Err(e) = self.release() {
            log::error!("Failed to turn off x11 privacy mode: {}", e);
        }
    }

    fn turn_on_privacy(&mut self, conn_id: i32) -> ResultType<bool> {
        if self.check_on_conn_id(conn_id)? {
            return Ok(true);
        }
        let guard = PrivacyGuard::turn_on(Options::default(), state_file())
            .map_err(|e| anyhow!("Failed to turn on privacy mode: {}", e))?;
        self.guard = Some(guard);
        self.conn_id = conn_id;
        Ok(true)
    }

    fn turn_off_privacy(
        &mut self,
        conn_id: i32,
        _state: Option<PrivacyModeState>,
    ) -> ResultType<()> {
        self.check_off_conn_id(conn_id)?;
        self.release()
    }

    fn pre_conn_id(&self) -> i32 {
        self.conn_id
    }

    fn get_impl_key(&self) -> &str {
        &self.impl_key
    }
}

impl Drop for PrivacyModeImpl {
    fn drop(&mut self) {
        self.clear();
    }
}
