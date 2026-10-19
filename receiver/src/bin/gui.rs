//! arp-gui — the windowed AudioRelayPlus receiver (Windows + Linux).
//!
//! Double-click it and connect from the phone. Status, level meter, output
//! device, gain, noise suppression, echo cancellation, plug-and-play USB,
//! soundpad and (on Linux) a one-click virtual microphone all live here.
//! Settings are remembered between runs.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arp::engine::{self, AudioCtl, Engine, EventLog, NetCtx, OutputManager, Shared};
use arp::platform::{exe_dir, open_folder};
use arp::protocol as proto;
use arp::soundpad::{Soundpad, EXTENSIONS};
use arp::t;
use arp::usb::{self, UsbAuto};

const APP_NAME: &str = "AudioRelayPlus";
#[cfg(target_os = "linux")]
const VIRT_SINK: &str = "arp_sink";
#[cfg(target_os = "linux")]
const VIRT_SOURCE: &str = "arp_mic";
#[cfg(target_os = "linux")]
const VIRT_DESC: &str = "AudioRelayPlus Mic";
const PLATFORM_TOOLS_URL: &str = "https://developer.android.com/tools/releases/platform-tools";

/// Everything the window remembers between runs.
#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    /// Output device (name substring); None = system default.
    device: Option<String>,
    /// The user picked the device themselves (no more VB-Cable auto-pick).
    device_chosen: bool,
    gain: f32,
    denoise: bool,
    aec: bool,
    /// Linux: route into the "AudioRelayPlus Mic" virtual microphone.
    virtual_mic: bool,
    auto_usb: bool,
    pad_volume: f32,
    pad_monitor: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            device: None,
            device_chosen: false,
            gain: 1.0,
            denoise: false,
            aec: false,
            virtual_mic: cfg!(target_os = "linux"),
            auto_usb: true,
            pad_volume: 0.9,
            pad_monitor: false,
        }
    }
}

fn main() -> eframe::Result {
    let minimized = std::env::args().any(|a| a == "--minimized");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id("audiorelayplus")
            .with_inner_size([500.0, 780.0])
            .with_min_inner_size([420.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |cc| {
            // A touch larger than egui's default, on top of the OS scaling
            cc.egui_ctx.set_zoom_factor(1.1);
            let settings: Settings =
                cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
            Ok(Box::new(App::new(settings, minimized)))
        }),
    )
}

/// Linux: the virtual microphone modules loaded with pactl.
#[cfg(target_os = "linux")]
struct VirtualMic {
    module_ids: Vec<String>,
}

#[cfg(target_os = "linux")]
impl VirtualMic {
    fn create(log: &EventLog) -> Option<VirtualMic> {
        let mut ids = Vec::new();
        let run = |args: &[&str]| -> Option<String> {
            let out = arp::platform::quiet_command("pactl").args(args).output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        };
        let sinks = run(&["list", "short", "sinks"]).unwrap_or_default();
        if !sinks.lines().any(|l| l.split_whitespace().nth(1) == Some(VIRT_SINK)) {
            match run(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={VIRT_SINK}"),
                "sink_properties=device.description='ARP-Internal-Output'",
            ]) {
                Some(id) => ids.push(id),
                None => {
                    log.push(t!(
                        "⚠ couldn't create the virtual microphone (is pactl installed?)",
                        "⚠ sanal mikrofon kurulamadı (pactl yok mu?)"
                    ));
                    return None;
                }
            }
        }
        let sources = run(&["list", "short", "sources"]).unwrap_or_default();
        if !sources.lines().any(|l| l.split_whitespace().nth(1) == Some(VIRT_SOURCE)) {
            if let Some(id) = run(&[
                "load-module",
                "module-remap-source",
                &format!("master={VIRT_SINK}.monitor"),
                &format!("source_name={VIRT_SOURCE}"),
                &format!("source_properties=device.description='{VIRT_DESC}'"),
            ]) {
                ids.push(id);
            }
        }
        log.push(t!("🎤 virtual microphone ready: \"{VIRT_DESC}\"", "🎤 sanal mikrofon hazır: \"{VIRT_DESC}\""));
        Some(VirtualMic { module_ids: ids })
    }
}

#[cfg(target_os = "linux")]
impl Drop for VirtualMic {
    fn drop(&mut self) {
        for id in &self.module_ids {
            let _ = arp::platform::quiet_command("pactl").args(["unload-module", id]).output();
        }
    }
}

/// Windows-only switches: autostart and the firewall rule.
#[cfg(windows)]
struct WinState {
    autostart: bool,
    /// None while checking / fixing.
    firewall_ok: Arc<Mutex<Option<bool>>>,
}

#[cfg(windows)]
impl WinState {
    fn new(log: &Arc<EventLog>) -> WinState {
        if let Ok(exe) = std::env::current_exe() {
            arp::platform::windows::refresh_autostart(&exe);
        }
        let s = WinState {
            autostart: arp::platform::windows::autostart_enabled(),
            firewall_ok: Arc::new(Mutex::new(None)),
        };
        s.check_firewall(log.clone(), false);
        s
    }

    /// Checks (and with `fix`, first repairs) the firewall rule in the background.
    fn check_firewall(&self, log: Arc<EventLog>, fix: bool) {
        let state = self.firewall_ok.clone();
        *state.lock().unwrap() = None;
        std::thread::spawn(move || {
            use arp::platform::windows as win;
            if fix {
                let res = std::env::current_exe()
                    .map_err(anyhow::Error::from)
                    .and_then(|exe| win::fix_firewall(&exe, proto::DEFAULT_PORT));
                match res {
                    Ok(()) => log.push(t!(
                        "🛡 firewall: AudioRelayPlus is allowed on every network",
                        "🛡 güvenlik duvarı: AudioRelayPlus tüm ağlarda izinli"
                    )),
                    Err(e) => log.push(t!("⚠ firewall: {e}", "⚠ güvenlik duvarı: {e}")),
                }
            }
            let ok = win::firewall_rule_present();
            *state.lock().unwrap() = Some(ok);
        });
    }
}

/// The PC's LAN address, for typing it into the app by hand (connecting a
/// UDP socket sends nothing; it just picks the outgoing interface).
fn lan_ip() -> Option<String> {
    let sock = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    sock.connect(("8.8.8.8", 80)).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then(|| ip.to_string())
}

/// Soundpad folder: next to the program (portable layout), or `./soundpad`
/// if that one exists. It may not exist yet — "Open folder" creates it.
fn soundpad_dir() -> PathBuf {
    let beside_exe = exe_dir().map(|d| d.join("soundpad"));
    match beside_exe {
        Some(d) if d.is_dir() => d,
        _ if PathBuf::from("soundpad").is_dir() => PathBuf::from("soundpad"),
        Some(d) => d,
        None => PathBuf::from("soundpad"),
    }
}

struct App {
    settings: Settings,
    shared: Shared,
    log: Arc<EventLog>,
    ctl: AudioCtl,
    output: Option<OutputManager>,
    monitor: Option<OutputManager>,
    /// Loopback capture feeding the echo canceller (while AEC is on).
    loopback: Option<cpal::Stream>,
    usb: Option<UsbAuto>,
    devices: Vec<String>,
    #[cfg(target_os = "linux")]
    virtual_mic: Option<VirtualMic>,
    #[cfg(windows)]
    win: WinState,
    error: Option<String>,
    pc_name: String,
    lan_ip: Option<String>,
    pad_names: Vec<String>,
    pad_version: u64,
    /// Displayed level (decaying peak).
    meter: f32,
    last_frame: Instant,
    start_minimized: bool,
}

impl App {
    fn new(mut settings: Settings, start_minimized: bool) -> Self {
        let shared: Shared = Arc::new(Mutex::new(Engine::default()));
        let adj = Arc::new(AtomicI32::new(0));
        let log = EventLog::new(false);

        let pad = Arc::new(Soundpad::load_dir(&soundpad_dir(), &log));
        pad.spawn_watcher(log.clone());
        let ctl = AudioCtl::new(pad.clone());

        let devices = engine::list_output_devices();
        if cfg!(windows) && !settings.device_chosen && settings.device.is_none() {
            if let Some(cable) = devices.iter().find(|d| d.to_lowercase().contains("cable input")) {
                log.push(t!(
                    "✅ VB-Cable found: output set to \"{cable}\" (in Discord pick \"CABLE Output\" as the microphone)",
                    "✅ VB-Cable bulundu: çıkış \"{cable}\" yapıldı (Discord'da mikrofon olarak \"CABLE Output\" seçin)"
                ));
                settings.device = Some(cable.clone());
            }
        }

        let pc_name = engine::default_name();
        let (mut output, mut monitor, mut usb_auto, mut error) = (None, None, None, None);
        match UdpSocket::bind(("0.0.0.0", proto::DEFAULT_PORT)) {
            Ok(sock) => {
                let port = proto::DEFAULT_PORT;
                let ctx = NetCtx::new(shared.clone(), pc_name.clone(), port, 60, log.clone(), pad.clone());
                engine::spawn_net(sock, ctx.clone());
                if let Err(e) = engine::spawn_tcp(ctx) {
                    log.push(t!("⚠ couldn't start the USB/TCP listener: {e}", "⚠ USB/TCP dinleyicisi açılamadı: {e}"));
                }
                engine::spawn_supervisor(shared.clone(), adj.clone(), log.clone(), pad.clone(), false);
                log.push(t!(
                    "🎧 listening on UDP+TCP {port} — connect from the phone",
                    "🎧 UDP+TCP {port} dinleniyor — telefondan bağlanabilirsiniz"
                ));
                output = Some(OutputManager::spawn(
                    t!("output", "çıkış"),
                    engine::mic_render(shared.clone(), adj.clone(), ctl.clone()),
                    log.clone(),
                ));
                monitor = Some(OutputManager::spawn(
                    t!("soundpad monitor", "soundpad dinleme"),
                    engine::monitor_render(pad.clone()),
                    log.clone(),
                ));
                usb_auto = Some(usb::spawn(port, settings.auto_usb, log.clone()));
            }
            Err(e) => {
                let port = proto::DEFAULT_PORT;
                error = Some(t!(
                    "Couldn't open port {port}: {e}\nAnother arp-receiver / arp-gui is probably running.",
                    "Port {port} açılamadı: {e}\nBaşka bir arp-receiver/arp-gui açık olabilir."
                ));
            }
        }

        #[cfg(windows)]
        let win = WinState::new(&log);

        let mut app = App {
            settings,
            shared,
            log,
            ctl,
            output,
            monitor,
            loopback: None,
            usb: usb_auto,
            devices,
            #[cfg(target_os = "linux")]
            virtual_mic: None,
            #[cfg(windows)]
            win,
            error,
            pc_name,
            lan_ip: lan_ip(),
            pad_names: pad.names(),
            pad_version: pad.version(),
            meter: 0.0,
            last_frame: Instant::now(),
            start_minimized,
        };
        app.ctl.set_gain(app.settings.gain);
        app.ctl.dsp.lock().unwrap().set_denoise(app.settings.denoise);
        app.ctl.pad.set_volume(app.settings.pad_volume);
        if app.output.is_some() {
            app.apply_output();
            if app.settings.aec {
                app.set_aec(true);
            }
        }
        app
    }

    /// (Re)opens the main output per the settings (and on Linux the virtual mic).
    fn apply_output(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if self.settings.virtual_mic {
                if self.virtual_mic.is_none() {
                    self.virtual_mic = VirtualMic::create(&self.log);
                }
                if self.virtual_mic.is_some() {
                    // libpulse reads this when the stream connects; set it first
                    std::env::set_var("PULSE_SINK", VIRT_SINK);
                    if let Some(o) = &self.output {
                        o.open(Some("pulse".into()));
                    }
                    self.apply_monitor();
                    return;
                }
            } else {
                self.virtual_mic = None;
                std::env::remove_var("PULSE_SINK");
            }
        }
        if let Some(o) = &self.output {
            o.open(self.settings.device.clone());
        }
        self.apply_monitor();
    }

    /// The monitor only makes sense when the main output isn't the speakers
    /// themselves (otherwise every sound would play twice).
    fn monitor_possible(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.settings.virtual_mic && self.virtual_mic.is_some() {
            return true;
        }
        self.settings.device.is_some()
    }

    fn apply_monitor(&mut self) {
        let on = self.settings.pad_monitor && self.monitor_possible();
        self.ctl.pad.set_monitor(on);
        let Some(m) = &self.monitor else { return };
        if on {
            // On Linux the "pulse" device would follow PULSE_SINK into the
            // virtual mic; PipeWire's own ALSA device goes to the speakers.
            let dev = if cfg!(target_os = "linux") && self.devices.iter().any(|d| d == "pipewire") {
                Some("pipewire".to_string())
            } else {
                None
            };
            m.open(dev);
        } else {
            m.close();
        }
    }

    fn set_aec(&mut self, on: bool) {
        if on {
            match arp::aec::spawn_loopback_reference(&self.log) {
                Ok((stream, ring)) => {
                    self.ctl.dsp.lock().unwrap().set_aec(Some(arp::aec::Aec::new(ring)));
                    self.loopback = Some(stream);
                    self.log.push(t!("🔇 echo cancellation on (experimental)", "🔇 yankı engelleme açık (deneysel)"));
                }
                Err(e) => {
                    self.log.push(t!("⚠ couldn't start echo cancellation: {e}", "⚠ yankı engelleme açılamadı: {e}"));
                    self.settings.aec = false;
                }
            }
        } else {
            self.ctl.dsp.lock().unwrap().set_aec(None);
            self.loopback = None;
            self.log.push(t!("echo cancellation off", "yankı engelleme kapatıldı"));
        }
    }

    /// Copies audio files dropped on the window into the soundpad folder
    /// (the folder watcher loads them a moment later).
    fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|i| i.raw.dropped_files.clone());
        if dropped.is_empty() {
            return;
        }
        let dir = self.ctl.pad.dir().to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        for f in dropped {
            let Some(path) = f.path else { continue };
            let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase());
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !matches!(ext.as_deref(), Some(e) if EXTENSIONS.contains(&e)) {
                self.log.push(t!("⚠ not an audio file (mp3/ogg/wav/flac): {name}", "⚠ ses dosyası değil (mp3/ogg/wav/flac): {name}"));
                continue;
            }
            match std::fs::copy(&path, dir.join(&name)) {
                Ok(_) => self.log.push(t!("📥 soundpad: added {name}", "📥 soundpad: {name} eklendi")),
                Err(e) => self.log.push(t!("⚠ couldn't copy {name}: {e}", "⚠ {name} kopyalanamadı: {e}")),
            }
        }
    }

    fn status_ui(&mut self, ui: &mut egui::Ui) {
        match engine::snapshot(&self.shared) {
            Some(s) => {
                let color = if s.playing { egui::Color32::from_rgb(80, 200, 100) } else { egui::Color32::YELLOW };
                ui.horizontal(|ui| {
                    status_dot(ui, color);
                    let (peer, via) = (s.peer, s.transport);
                    ui.label(t!("Connected: {peer} · {via}", "Bağlı: {peer} · {via}"));
                });
                let st = s.stats;
                let (fill, target, fec, plc, drops) = (s.fill_ms, s.target_ms, st.fec_recovered, st.plc_frames, st.underruns);
                ui.label(
                    egui::RichText::new(t!(
                        "buffer {fill}/{target} ms · recovered (FEC) {fec} · concealed (PLC) {plc} · dropouts {drops}",
                        "tampon {fill}/{target} ms · kurtarılan (FEC) {fec} · gizlenen (PLC) {plc} · kesinti {drops}"
                    ))
                    .weak(),
                );
            }
            None => {
                ui.horizontal(|ui| {
                    status_dot(ui, egui::Color32::GRAY);
                    ui.label(t!(
                        "Waiting for the phone — open the app, this PC shows up in its list",
                        "Telefon bekleniyor — uygulamayı açın, bu PC listede görünür"
                    ));
                });
                let name = &self.pc_name;
                let hint = match &self.lan_ip {
                    Some(ip) => t!("This PC: {name} · manual connect: {ip}", "Bu PC: {name} · elle bağlanmak için: {ip}"),
                    None => t!("This PC: {name}", "Bu PC: {name}"),
                };
                ui.label(egui::RichText::new(hint).weak());
            }
        }

        // Level meter + mute
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        let peak = self.ctl.take_peak();
        self.meter = peak.max(self.meter * 0.5f32.powf(dt / 0.3));
        let muted = self.ctl.is_muted();
        ui.horizontal(|ui| {
            level_meter(ui, self.meter, muted);
            let db = 20.0 * self.meter.max(1e-5).log10();
            ui.label(egui::RichText::new(format!("{db:>4.0} dB")).monospace().size(12.0));
            let label = if muted { t!("🔇 Muted", "🔇 Susturuldu") } else { t!("🎤 Mute mic", "🎤 Mikrofonu sustur") };
            let btn = egui::Button::new(label).selected(muted);
            if ui.add(btn).clicked() {
                self.ctl.set_muted(!muted);
            }
        });
    }

    fn output_ui(&mut self, ui: &mut egui::Ui) {
        #[cfg(target_os = "linux")]
        {
            if ui
                .checkbox(
                    &mut self.settings.virtual_mic,
                    t!("Virtual microphone (\"{VIRT_DESC}\") — pick it in Discord", "Sanal mikrofon (\"{VIRT_DESC}\") — Discord'da bunu seçin"),
                )
                .changed()
            {
                self.apply_output();
            }
        }
        #[cfg(windows)]
        {
            ui.label(t!(
                "For Discord/games: install VB-Cable, choose \"CABLE Input\" below,",
                "Discord/oyun için: VB-Cable kurun, aşağıdan \"CABLE Input\" seçin,"
            ));
            ui.label(t!(
                "and pick \"CABLE Output\" as the microphone in Discord.",
                "Discord'da mikrofon olarak \"CABLE Output\" seçin."
            ));
        }

        let combo_enabled = !(cfg!(target_os = "linux") && self.settings.virtual_mic);
        let default_label = t!("(system default)", "(varsayılan)");
        let mut changed: Option<Option<String>> = None;
        ui.add_enabled_ui(combo_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(t!("Output device:", "Çıkış aygıtı:"));
                let current = self.settings.device.clone().unwrap_or_else(|| default_label.clone());
                egui::ComboBox::from_id_salt("dev")
                    .width(240.0)
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(self.settings.device.is_none(), &default_label).clicked() {
                            changed = Some(None);
                        }
                        for d in &self.devices {
                            if ui.selectable_label(self.settings.device.as_deref() == Some(d), d).clicked() {
                                changed = Some(Some(d.clone()));
                            }
                        }
                    });
                if ui.button("⟳").on_hover_text(t!("refresh devices", "aygıtları yenile")).clicked() {
                    self.devices = engine::list_output_devices();
                }
            });
        });
        if let Some(sel) = changed {
            self.settings.device = sel;
            self.settings.device_chosen = true;
            self.apply_output();
        }
        if let Some(o) = &self.output {
            let st = o.status();
            if let Some(active) = st.active {
                ui.label(egui::RichText::new(t!("active: {active}", "etkin: {active}")).weak());
            } else if let Some(err) = st.error {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    t!("⚠ {err} — retrying…", "⚠ {err} — tekrar deneniyor…"),
                );
            }
        }

        ui.add_space(4.0);
        let slider = egui::Slider::new(&mut self.settings.gain, 1.0..=4.0)
            .text(t!("PC-side gain", "PC tarafı kazanç"))
            .custom_formatter(|v, _| format!("×{v:.1}"));
        if ui.add(slider).changed() {
            self.ctl.set_gain(self.settings.gain);
        }
        if ui
            .checkbox(
                &mut self.settings.denoise,
                t!("AI noise suppression (RNNoise) — fans, keyboard, street", "Yapay zekâ gürültü engelleme (RNNoise) — fan, klavye, sokak"),
            )
            .changed()
        {
            self.ctl.dsp.lock().unwrap().set_denoise(self.settings.denoise);
            let msg = if self.settings.denoise {
                t!("✨ noise suppression on", "✨ gürültü engelleme açık")
            } else {
                t!("noise suppression off", "gürültü engelleme kapatıldı")
            };
            self.log.push(msg);
        }
        if ui
            .checkbox(
                &mut self.settings.aec,
                t!("Echo cancellation (experimental) — the real fix: headphones", "Yankı engelleme (deneysel) — kesin çözüm: kulaklık"),
            )
            .changed()
        {
            let on = self.settings.aec;
            self.set_aec(on);
        }
    }

    fn usb_ui(&mut self, ui: &mut egui::Ui) {
        let Some(usb) = &self.usb else { return };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(t!("USB cable:", "USB kablo:")).strong());
            if ui
                .checkbox(&mut self.settings.auto_usb, t!("automatic (adb)", "otomatik (adb)"))
                .changed()
            {
                usb.enabled.store(self.settings.auto_usb, Ordering::Relaxed);
            }
        });
        if !self.settings.auto_usb {
            return;
        }
        let st = usb.status.lock().unwrap().clone();
        if st.adb.is_none() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(t!(
                        "adb not found — unzip Android platform-tools next to this program:",
                        "adb bulunamadı — Android platform-tools'u bu programın yanına açın:"
                    ))
                    .weak(),
                );
                ui.hyperlink_to("platform-tools", PLATFORM_TOOLS_URL);
            });
        } else if !st.phones.is_empty() {
            let models = st.phones.values().cloned().collect::<Vec<_>>().join(", ");
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 100),
                t!("✅ {models} ready — tap \"(USB cable)\" in the app's list", "✅ {models} hazır — uygulamada \"(USB kablo)\" satırına dokunun"),
            );
        } else if st.unauthorized {
            ui.colored_label(
                egui::Color32::YELLOW,
                t!("📱 Allow USB debugging on the phone's screen", "📱 Telefon ekranında USB hata ayıklamaya izin verin"),
            );
        } else {
            ui.label(
                egui::RichText::new(t!(
                    "Plug the phone in (USB debugging on) — the rest is automatic.",
                    "Telefonu takın (USB hata ayıklama açık) — gerisi otomatik."
                ))
                .weak(),
            );
        }
    }

    fn soundpad_ui(&mut self, ui: &mut egui::Ui) {
        let pad = self.ctl.pad.clone();
        if pad.version() != self.pad_version {
            self.pad_version = pad.version();
            self.pad_names = pad.names();
        }
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Soundpad").strong());
            if ui.button(t!("⏹ Stop", "⏹ Durdur")).clicked() {
                pad.stop_all();
            }
            if ui.button(t!("📂 Folder", "📂 Klasör")).clicked() {
                let dir = pad.dir().to_path_buf();
                let res = std::fs::create_dir_all(&dir).and_then(|_| open_folder(&dir));
                if let Err(e) = res {
                    let d = dir.display();
                    self.log.push(t!("⚠ couldn't open {d}: {e}", "⚠ {d} açılamadı: {e}"));
                }
            }
        });
        ui.horizontal(|ui| {
            let vol = egui::Slider::new(&mut self.settings.pad_volume, 0.0..=1.5)
                .text(t!("volume", "ses"))
                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
            if ui.add(vol).changed() {
                pad.set_volume(self.settings.pad_volume);
            }
            let possible = self.monitor_possible();
            let hint = if possible {
                t!("Also play the sounds on this PC's speakers", "Sesleri bu PC'nin hoparlöründen de çal")
            } else {
                t!(
                    "Pick an output device other than the speakers first (it would play twice)",
                    "Önce hoparlör dışında bir çıkış aygıtı seçin (yoksa iki kez çalar)"
                )
            };
            let resp = ui
                .add_enabled(possible, egui::Checkbox::new(&mut self.settings.pad_monitor, t!("hear it myself", "ben de duyayım")))
                .on_hover_text(hint)
                .on_disabled_hover_text(t!("Not needed: the sounds already play on the speakers", "Gerek yok: sesler zaten hoparlörden çalıyor"));
            if resp.changed() {
                self.apply_monitor();
            }
        });
        if self.pad_names.is_empty() {
            ui.label(
                egui::RichText::new(t!(
                    "Drop mp3/ogg/wav/flac files on this window (or into the folder) — they appear here and on the phone.",
                    "mp3/ogg/wav/flac dosyalarını bu pencereye sürükleyin (ya da klasöre atın) — burada ve telefonda görünür."
                ))
                .weak()
                .size(11.5),
            );
        } else {
            egui::ScrollArea::vertical().max_height(110.0).id_salt("padscroll").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, name) in self.pad_names.iter().enumerate() {
                        if ui.button(format!("🔔 {name}")).clicked() {
                            pad.play(i);
                        }
                    }
                });
            });
        }
    }

    #[cfg(windows)]
    fn windows_ui(&mut self, ui: &mut egui::Ui) {
        use arp::platform::windows as win;
        let mut on = self.win.autostart;
        if ui
            .checkbox(&mut on, t!("Start with Windows (minimized)", "Windows açılınca başlat (simge durumunda)"))
            .changed()
        {
            match std::env::current_exe().map_err(anyhow::Error::from).and_then(|exe| win::set_autostart(&exe, on)) {
                Ok(()) => self.win.autostart = on,
                Err(e) => self.log.push(t!("⚠ autostart: {e}", "⚠ otomatik başlatma: {e}")),
            }
        }
        let state = *self.win.firewall_ok.lock().unwrap();
        ui.horizontal(|ui| {
            match state {
                Some(true) => {
                    ui.label(t!("🛡 Firewall: allowed", "🛡 Güvenlik duvarı: izinli"));
                }
                Some(false) => {
                    ui.colored_label(
                        egui::Color32::YELLOW,
                        t!("⚠ Firewall: no rule — the phone may not find this PC", "⚠ Güvenlik duvarı: kural yok — telefon PC'yi bulamayabilir"),
                    );
                }
                None => {
                    ui.spinner();
                }
            }
            if ui
                .add_enabled(state.is_some(), egui::Button::new(t!("Allow (admin)", "İzin ver (yönetici)")))
                .on_hover_text(t!(
                    "Adds a Windows Firewall rule for every network type (Windows asks for permission)",
                    "Tüm ağ türleri için Windows Güvenlik Duvarı kuralı ekler (Windows izin sorar)"
                ))
                .clicked()
            {
                self.win.check_firewall(self.log.clone(), true);
            }
        });
    }
}

/// A filled status circle (the "●" glyph isn't in egui's proportional font).
fn status_dot(ui: &mut egui::Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

/// Horizontal level bar, -60..0 dBFS.
fn level_meter(ui: &mut egui::Ui, level: f32, muted: bool) {
    let size = egui::vec2(ui.available_width().min(220.0), 14.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
    let db = 20.0 * level.max(1e-5).log10();
    let frac = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let color = if muted {
        egui::Color32::GRAY
    } else if db > -3.0 {
        egui::Color32::from_rgb(230, 70, 60)
    } else if db > -12.0 {
        egui::Color32::from_rgb(230, 200, 60)
    } else {
        egui::Color32::from_rgb(80, 200, 100)
    };
    let mut bar = rect;
    bar.set_width(rect.width() * frac);
    painter.rect_filled(bar, 3.0, color);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.start_minimized {
            self.start_minimized = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        self.take_dropped_files(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🎤 AudioRelayPlus");
                ui.label(egui::RichText::new(concat!("v", env!("CARGO_PKG_VERSION"))).weak());
            });
            ui.add_space(4.0);

            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
                return;
            }

            self.status_ui(ui);
            ui.add_space(4.0);
            ui.separator();
            self.output_ui(ui);
            ui.add_space(4.0);
            ui.separator();
            self.usb_ui(ui);
            ui.add_space(4.0);
            ui.separator();
            self.soundpad_ui(ui);
            #[cfg(windows)]
            {
                ui.add_space(4.0);
                ui.separator();
                self.windows_ui(ui);
            }

            ui.add_space(4.0);
            ui.separator();
            ui.label(egui::RichText::new(t!("Events", "Olaylar")).strong());
            egui::ScrollArea::vertical().stick_to_bottom(true).id_salt("logscroll").show(ui, |ui| {
                for line in self.log.tail(60) {
                    ui.label(egui::RichText::new(line).monospace().size(12.0));
                }
            });
        });

        // Smooth meter while something plays; otherwise a calm 4 fps is enough
        let busy = self.meter > 0.001 || engine::snapshot(&self.shared).is_some();
        ctx.request_repaint_after(Duration::from_millis(if busy { 50 } else { 250 }));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.settings);
    }

    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }
}
