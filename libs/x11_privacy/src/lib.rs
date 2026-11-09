//! Privacy mode for X11 sessions ("RDP-like" console blackout).
//!
//! What it does while active:
//! * every active CRTC gets an all-zero gamma ramp, so the physical monitors show black,
//!   while the X framebuffer (what screen capture reads) is untouched;
//! * every physical input device (keyboard, mouse, touchpad, ...) is disabled through the
//!   XInput "Device Enabled" property. XTEST devices, which remote input is injected through,
//!   stay enabled.
//!
//! A background thread re-applies both every [`ENFORCE_INTERVAL`], because the desktop
//! (night light, colour profiles, monitor hotplug) may reset gamma and newly plugged
//! devices come up enabled.
//!
//! The state needed to undo everything is written to a file, so that if the process dies while
//! privacy mode is on, [`recover`] (called on the next start) brings the console back.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::{self, ConnectionExt as _},
        xinput::{self, ConnectionExt as _},
        xproto::{self, AtomEnum, ConnectionExt as _},
    },
    rust_connection::RustConnection,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub const ENFORCE_INTERVAL: Duration = Duration::from_millis(300);

/// Device names containing one of these are never disabled: they carry remote input.
const KEEP_DEVICE_PATTERNS: &[&str] = &["XTEST", "RustDesk"];

type Gamma = (Vec<u16>, Vec<u16>, Vec<u16>);

#[derive(Default, Debug, Clone, PartialEq)]
struct SavedState {
    /// CRTC id -> original gamma ramp.
    gamma: BTreeMap<u32, Gamma>,
    /// Names of the devices we disabled (ids may change after a restart, names do not).
    devices: BTreeSet<String>,
}

fn connect() -> Result<(RustConnection, xproto::Window)> {
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    Ok((conn, root))
}

/// True if this is an X11 session with RandR >= 1.2 and XInput >= 2.0.
pub fn is_supported() -> bool {
    if std::env::var("XDG_SESSION_TYPE").map_or(false, |t| t == "wayland") {
        return false;
    }
    let Ok((conn, _)) = connect() else {
        return false;
    };
    let randr_ok = conn
        .randr_query_version(1, 2)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(false, |v| v.major_version > 1 || v.minor_version >= 2);
    let xi_ok = conn
        .xinput_xi_query_version(2, 0)
        .ok()
        .and_then(|c| c.reply().ok())
        .map_or(false, |v| v.major_version >= 2);
    randr_ok && xi_ok
}

// ---------------------------------------------------------------------------------------------
// Gamma
// ---------------------------------------------------------------------------------------------

fn active_crtcs(conn: &RustConnection, root: xproto::Window) -> Result<Vec<randr::Crtc>> {
    let res = conn.randr_get_screen_resources_current(root)?.reply()?;
    let mut out = Vec::new();
    for crtc in res.crtcs {
        let info = conn.randr_get_crtc_info(crtc, res.config_timestamp)?.reply()?;
        if info.mode != 0 && !info.outputs.is_empty() {
            out.push(crtc);
        }
    }
    Ok(out)
}

fn is_black((r, g, b): &Gamma) -> bool {
    r.iter().chain(g).chain(b).all(|v| *v == 0)
}

fn linear_ramp(size: usize) -> Vec<u16> {
    if size <= 1 {
        return vec![u16::MAX; size];
    }
    (0..size)
        .map(|i| ((i as u64 * u16::MAX as u64) / (size as u64 - 1)) as u16)
        .collect()
}

fn get_gamma(conn: &RustConnection, crtc: randr::Crtc) -> Result<Gamma> {
    let g = conn.randr_get_crtc_gamma(crtc)?.reply()?;
    Ok((g.red, g.green, g.blue))
}

fn set_gamma(conn: &RustConnection, crtc: randr::Crtc, (r, g, b): &Gamma) -> Result<()> {
    conn.randr_set_crtc_gamma(crtc, r, g, b)?.check()?;
    Ok(())
}

/// Blacks out every active CRTC; records the original ramp of those not yet in `saved`.
/// Returns true if anything had to be (re)applied.
fn blackout_all(conn: &RustConnection, root: xproto::Window, saved: &mut SavedState) -> Result<bool> {
    let mut changed = false;
    for crtc in active_crtcs(conn, root)? {
        let current = get_gamma(conn, crtc)?;
        if is_black(&current) {
            continue;
        }
        // First time we see it, or the desktop reset it (night light, profile, hotplug):
        // the ramp it has now is what should come back on turn-off.
        saved.gamma.insert(crtc, current.clone());
        let size = current.0.len();
        let zero = vec![0u16; size];
        set_gamma(conn, crtc, &(zero.clone(), zero.clone(), zero))?;
        changed = true;
    }
    Ok(changed)
}

fn restore_gamma(conn: &RustConnection, root: xproto::Window, saved: &SavedState) -> Result<()> {
    let mut first_err = None;
    for crtc in active_crtcs(conn, root)? {
        let current = match get_gamma(conn, crtc) {
            Ok(g) => g,
            Err(e) => {
                first_err.get_or_insert(e);
                continue;
            }
        };
        let target = match saved.gamma.get(&crtc) {
            Some(g) if g.0.len() == current.0.len() && !is_black(g) => g.clone(),
            // Unknown CRTC, size changed, or a black ramp got saved: fall back to identity.
            _ if is_black(&current) => {
                let l = linear_ramp(current.0.len());
                (l.clone(), l.clone(), l)
            }
            _ => continue,
        };
        if let Err(e) = set_gamma(conn, crtc, &target) {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------------------------
// Input devices
// ---------------------------------------------------------------------------------------------

struct InputAtoms {
    device_enabled: xproto::Atom,
}

fn input_atoms(conn: &RustConnection) -> Result<InputAtoms> {
    Ok(InputAtoms {
        device_enabled: conn.intern_atom(false, b"Device Enabled")?.reply()?.atom,
    })
}

fn is_physical(info: &xinput::XIDeviceInfo) -> bool {
    let is_slave = info.type_ == xinput::DeviceType::SLAVE_POINTER
        || info.type_ == xinput::DeviceType::SLAVE_KEYBOARD
        || info.type_ == xinput::DeviceType::FLOATING_SLAVE;
    let name = String::from_utf8_lossy(&info.name);
    is_slave && !KEEP_DEVICE_PATTERNS.iter().any(|p| name.contains(p))
}

fn set_device_enabled(
    conn: &RustConnection,
    atoms: &InputAtoms,
    id: xinput::DeviceId,
    enabled: bool,
) -> Result<()> {
    conn.xinput_xi_change_property(
        id,
        xproto::PropMode::REPLACE,
        atoms.device_enabled,
        AtomEnum::INTEGER.into(),
        1,
        &xinput::XIChangePropertyAux::Data8(vec![enabled as u8]),
    )?
    .check()?;
    Ok(())
}

/// Disables every enabled physical device. Returns true if anything was disabled.
fn disable_inputs(conn: &RustConnection, atoms: &InputAtoms, saved: &mut SavedState) -> Result<bool> {
    let mut changed = false;
    for info in conn.xinput_xi_query_device(xinput::Device::ALL)?.reply()?.infos {
        if !info.enabled || !is_physical(&info) {
            continue;
        }
        let name = String::from_utf8_lossy(&info.name).into_owned();
        match set_device_enabled(conn, atoms, info.deviceid, false) {
            Ok(()) => {
                log::info!("x11 privacy: disabled input device {} ({})", info.deviceid, name);
                saved.devices.insert(name);
                changed = true;
            }
            // A device may vanish between query and change; keep going with the others.
            Err(e) => log::warn!("x11 privacy: failed to disable {}: {}", name, e),
        }
    }
    Ok(changed)
}

fn enable_inputs(conn: &RustConnection, atoms: &InputAtoms, saved: &SavedState) -> Result<()> {
    let mut first_err = None;
    for info in conn.xinput_xi_query_device(xinput::Device::ALL)?.reply()?.infos {
        let name = String::from_utf8_lossy(&info.name);
        if info.enabled || !is_physical(&info) || !saved.devices.contains(name.as_ref()) {
            continue;
        }
        if let Err(e) = set_device_enabled(conn, atoms, info.deviceid, true) {
            log::warn!("x11 privacy: failed to re-enable {}: {}", name, e);
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

// ---------------------------------------------------------------------------------------------
// State file (crash recovery)
// ---------------------------------------------------------------------------------------------

fn serialize(state: &SavedState) -> String {
    let mut s = String::from("# rustdesk x11 privacy mode state; delete to forget\n");
    for (crtc, (r, g, b)) in &state.gamma {
        let join = |v: &Vec<u16>| v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",");
        s += &format!("gamma {} {} {} {}\n", crtc, join(r), join(g), join(b));
    }
    for d in &state.devices {
        s += &format!("device {}\n", d);
    }
    s
}

fn deserialize(text: &str) -> SavedState {
    let mut state = SavedState::default();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("device ") {
            state.devices.insert(name.to_owned());
        } else if let Some(rest) = line.strip_prefix("gamma ") {
            let parts: Vec<&str> = rest.split(' ').collect();
            let parse = |s: &str| -> Option<Vec<u16>> {
                s.split(',').map(|x| x.parse().ok()).collect()
            };
            if let [crtc, r, g, b] = parts[..] {
                if let (Ok(crtc), Some(r), Some(g), Some(b)) = (crtc.parse(), parse(r), parse(g), parse(b)) {
                    state.gamma.insert(crtc, (r, g, b));
                }
            }
        }
    }
    state
}

fn write_state(path: &Path, state: &SavedState) {
    let res = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serialize(state).as_bytes())?;
        f.sync_all()?;
        fs::rename(tmp, path)
    })();
    if let Err(e) = res {
        log::error!("x11 privacy: failed to write state file {:?}: {}", path, e);
    }
}

/// Undo a privacy mode left behind by a crashed process, if `state_file` exists.
/// Returns true if something was restored.
pub fn recover(state_file: &Path) -> Result<bool> {
    let Ok(text) = fs::read_to_string(state_file) else {
        return Ok(false);
    };
    let state = deserialize(&text);
    let (conn, root) = connect()?;
    let atoms = input_atoms(&conn)?;
    let r1 = enable_inputs(&conn, &atoms, &state);
    let r2 = restore_gamma(&conn, root, &state);
    conn.flush()?;
    r1?;
    r2?;
    let _ = fs::remove_file(state_file);
    log::info!("x11 privacy: recovered console from {:?}", state_file);
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub blackout: bool,
    pub block_input: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { blackout: true, block_input: true }
    }
}

/// Privacy mode is on for as long as this lives (or until [`PrivacyGuard::turn_off`]).
pub struct PrivacyGuard {
    state: Arc<Mutex<SavedState>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    state_file: PathBuf,
    opts: Options,
}

impl PrivacyGuard {
    pub fn turn_on(opts: Options, state_file: PathBuf) -> Result<Self> {
        // Leftovers from a crash would otherwise be saved as "original" (black) values.
        let _ = recover(&state_file);

        let (conn, root) = connect()?;
        let atoms = input_atoms(&conn)?;
        let mut saved = SavedState::default();
        // Write the (empty) state first, so a crash in the middle is still recoverable.
        write_state(&state_file, &saved);

        let res = (|| -> Result<()> {
            if opts.blackout {
                blackout_all(&conn, root, &mut saved)?;
                write_state(&state_file, &saved);
            }
            if opts.block_input {
                disable_inputs(&conn, &atoms, &mut saved)?;
                write_state(&state_file, &saved);
            }
            conn.flush()?;
            Ok(())
        })();
        if let Err(e) = res {
            let _ = enable_inputs(&conn, &atoms, &saved);
            let _ = restore_gamma(&conn, root, &saved);
            let _ = conn.flush();
            let _ = fs::remove_file(&state_file);
            return Err(e);
        }

        let state = Arc::new(Mutex::new(saved));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (state, stop, file) = (state.clone(), stop.clone(), state_file.clone());
            std::thread::Builder::new()
                .name("x11-privacy-enforce".into())
                .spawn(move || enforce_loop(conn, root, atoms, opts, state, stop, file))?
        };
        Ok(Self { state, stop, thread: Some(thread), state_file, opts })
    }

    pub fn turn_off(mut self) -> Result<()> {
        self.do_turn_off()
    }

    fn do_turn_off(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        self.stop.store(true, Ordering::SeqCst);
        let _ = thread.join();
        let saved = self.state.lock().unwrap().clone();
        let (conn, root) = connect()?;
        let atoms = input_atoms(&conn)?;
        let r1 = if self.opts.block_input { enable_inputs(&conn, &atoms, &saved) } else { Ok(()) };
        let r2 = if self.opts.blackout { restore_gamma(&conn, root, &saved) } else { Ok(()) };
        conn.flush()?;
        r1?;
        r2?;
        let _ = fs::remove_file(&self.state_file);
        Ok(())
    }
}

impl Drop for PrivacyGuard {
    fn drop(&mut self) {
        if let Err(e) = self.do_turn_off() {
            log::error!("x11 privacy: failed to turn off on drop: {}", e);
        }
    }
}

fn enforce_loop(
    conn: RustConnection,
    root: xproto::Window,
    atoms: InputAtoms,
    opts: Options,
    state: Arc<Mutex<SavedState>>,
    stop: Arc<AtomicBool>,
    state_file: PathBuf,
) {
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(ENFORCE_INTERVAL);
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let mut saved = state.lock().unwrap();
        let mut changed = false;
        if opts.blackout {
            match blackout_all(&conn, root, &mut saved) {
                Ok(c) => changed |= c,
                Err(e) => log::warn!("x11 privacy: gamma enforce failed: {}", e),
            }
        }
        if opts.block_input {
            match disable_inputs(&conn, &atoms, &mut saved) {
                Ok(c) => changed |= c,
                Err(e) => log::warn!("x11 privacy: input enforce failed: {}", e),
            }
        }
        let _ = conn.flush();
        if changed {
            write_state(&state_file, &saved);
        }
    }
}

/// Human readable snapshot, for diagnostics.
pub fn status() -> Result<String> {
    let (conn, root) = connect()?;
    let mut s = String::new();
    for crtc in active_crtcs(&conn, root)? {
        let g = get_gamma(&conn, crtc)?;
        s += &format!("crtc {}: gamma size {}, black: {}\n", crtc, g.0.len(), is_black(&g));
    }
    for info in conn.xinput_xi_query_device(xinput::Device::ALL)?.reply()?.infos {
        s += &format!(
            "device {:>3} {:<45} type {:?} enabled {} physical {}\n",
            info.deviceid,
            String::from_utf8_lossy(&info.name),
            u16::from(info.type_),
            info.enabled,
            is_physical(&info)
        );
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_roundtrip() {
        let mut s = SavedState::default();
        s.gamma.insert(63, (vec![0, 100, 65535], vec![1, 2, 3], vec![4, 5, 6]));
        s.devices.insert("SYNA Touchpad".into());
        s.devices.insert("AT Translated Set 2 keyboard".into());
        assert_eq!(deserialize(&serialize(&s)), s);
    }

    #[test]
    fn ramp() {
        assert_eq!(linear_ramp(3), vec![0, 32767, 65535]);
        assert!(is_black(&(vec![0; 4], vec![0; 4], vec![0; 4])));
    }
}
