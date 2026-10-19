//! Plug-and-play USB: while enabled, keeps an `adb reverse tcp:PORT tcp:PORT`
//! tunnel on every authorized phone attached to adb, so the phone app can
//! reach the receiver at 127.0.0.1 — it then lists the PC as "USB" by itself.
//! No buttons: plug the cable in and it just appears.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::engine::EventLog;
use crate::platform::{exe_dir, quiet_command};
use crate::t;

const ADB: &str = if cfg!(windows) { "adb.exe" } else { "adb" };

/// Finds adb: next to the program (also in a `platform-tools` folder there),
/// in the default Android SDK location, or on PATH.
pub fn find_adb() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = exe_dir() {
        candidates.push(dir.join(ADB));
        candidates.push(dir.join("platform-tools").join(ADB));
    }
    #[cfg(windows)]
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(PathBuf::from(local).join(r"Android\Sdk\platform-tools\adb.exe"));
    }
    #[cfg(not(windows))]
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join("Android/Sdk/platform-tools/adb"));
    }
    if let Some(found) = candidates.into_iter().find(|c| c.is_file()) {
        return Some(found);
    }
    let on_path = quiet_command(ADB).arg("version").output().map(|o| o.status.success()).unwrap_or(false);
    on_path.then(|| PathBuf::from(ADB))
}

/// Starts the adb server with no pipes attached. If a command we capture
/// output from had to start it instead, the server could inherit our pipes
/// (on Windows it inherits handles) and that command would never seem to end.
fn start_server(adb: &Path) {
    let _ = quiet_command(adb)
        .arg("start-server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[derive(Clone, Debug, Default)]
pub struct UsbStatus {
    /// Where adb was found (None: not installed / not found).
    pub adb: Option<PathBuf>,
    /// Phones with a working tunnel: serial → model.
    pub phones: BTreeMap<String, String>,
    /// A phone is attached but hasn't allowed USB debugging yet.
    pub unauthorized: bool,
}

pub struct UsbAuto {
    pub enabled: Arc<AtomicBool>,
    pub status: Arc<Mutex<UsbStatus>>,
}

/// A device line of `adb devices -l`.
struct Device {
    serial: String,
    state: String,
    model: String,
}

fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            let model = it
                .find_map(|kv| kv.strip_prefix("model:"))
                .map(|m| m.replace('_', " "))
                .unwrap_or_else(|| serial.clone());
            Some(Device { serial, state, model })
        })
        .collect()
}

/// Starts the background tunnel keeper. It idles while `enabled` is false.
pub fn spawn(port: u16, enabled: bool, log: Arc<EventLog>) -> UsbAuto {
    let auto = UsbAuto {
        enabled: Arc::new(AtomicBool::new(enabled)),
        status: Arc::new(Mutex::new(UsbStatus::default())),
    };
    let enabled = auto.enabled.clone();
    let status = auto.status.clone();
    std::thread::spawn(move || {
        let mapping = format!("tcp:{port}");
        let mut adb: Option<PathBuf> = None;
        let mut warned_unauth = false;
        let mut tick = 0u64;
        loop {
            if tick > 0 {
                std::thread::sleep(Duration::from_secs(2));
            }
            tick += 1;
            if !enabled.load(Ordering::Relaxed) {
                let mut st = status.lock().unwrap();
                st.phones.clear();
                st.unauthorized = false;
                continue;
            }
            // Keep looking until adb turns up (cheap: a missing program fails
            // before any process is created)
            if adb.is_none() {
                adb = find_adb();
                status.lock().unwrap().adb = adb.clone();
                if let Some(a) = &adb {
                    start_server(a);
                }
            }
            let Some(adb_path) = adb.as_ref() else { continue };
            let out = match quiet_command(adb_path).args(["devices", "-l"]).output() {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => {
                    start_server(adb_path); // e.g. something killed the adb server
                    continue;
                }
            };
            let devices = parse_devices(&out);
            let mut phones = status.lock().unwrap().phones.clone();

            for d in devices.iter().filter(|d| d.state == "device") {
                // New phones get the tunnel at once; known ones are re-checked
                // every ~10 s (the phone side forgets it when adbd restarts).
                let known = phones.contains_key(&d.serial);
                if known && !tick.is_multiple_of(5) {
                    continue;
                }
                let has_tunnel = quiet_command(adb_path)
                    .args(["-s", &d.serial, "reverse", "--list"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("{mapping} {mapping}")))
                    .unwrap_or(false);
                let ok = has_tunnel
                    || quiet_command(adb_path)
                        .args(["-s", &d.serial, "reverse", &mapping, &mapping])
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                if ok && !known {
                    let model = &d.model;
                    log.push(t!(
                        "🔌 USB: {model} is ready — it shows up in the phone app's list",
                        "🔌 USB: {model} hazır — telefondaki listede görünür"
                    ));
                    phones.insert(d.serial.clone(), d.model.clone());
                } else if !ok && known {
                    phones.remove(&d.serial);
                }
            }
            // Unplugged phones
            let gone: Vec<String> = phones
                .keys()
                .filter(|s| !devices.iter().any(|d| &d.serial == *s && d.state == "device"))
                .cloned()
                .collect();
            for s in gone {
                if let Some(model) = phones.remove(&s) {
                    log.push(t!("🔌 USB: {model} disconnected", "🔌 USB: {model} çıkarıldı"));
                }
            }
            let unauthorized = devices.iter().any(|d| d.state == "unauthorized");
            if unauthorized && !warned_unauth {
                log.push(t!(
                    "📱 USB: allow USB debugging on the phone (a prompt is waiting there)",
                    "📱 USB: telefonda USB hata ayıklamaya izin verin (ekranda soru bekliyor)"
                ));
            }
            warned_unauth = unauthorized;
            let mut st = status.lock().unwrap();
            st.phones = phones;
            st.unauthorized = unauthorized;
        }
    });
    auto
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_adb_devices_listing() {
        let out = "* daemon started successfully\nList of devices attached\n\
                   R58M12ABC      device usb:1-2 product:a52 model:SM_A525F device:a52q transport_id:3\n\
                   0123456789ABCDEF unauthorized usb:1-1 transport_id:4\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].serial, "R58M12ABC");
        assert_eq!(d[0].state, "device");
        assert_eq!(d[0].model, "SM A525F");
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[1].model, "0123456789ABCDEF");
    }
}
