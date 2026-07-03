//! arp-gui — AudioRelayPlus alıcısının pencereli sürümü (Windows + Linux).
//!
//! Çift tıkla aç, telefondan bağlan; durum, aygıt seçimi, kazanç, soundpad,
//! yankı iptali ve (Linux'ta) tek tıkla sanal mikrofon burada.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui;
use std::net::UdpSocket;
use std::sync::atomic::AtomicI32;
use std::sync::{Arc, Mutex};

use arp::engine::{self, AudioCtl, Engine, EventLog, Shared};
use arp::protocol as proto;
use arp::soundpad::Soundpad;

const VIRT_SINK: &str = "arp_sink";
const VIRT_SOURCE: &str = "arp_mic";
const VIRT_DESC: &str = "AudioRelayPlus Mic";

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 640.0])
            .with_min_inner_size([400.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AudioRelayPlus Alıcısı",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_pixels_per_point(1.15);
            Ok(Box::new(App::new()))
        }),
    )
}

/// Linux'ta pactl ile kurulan sanal mikrofon modülleri.
struct VirtualMic {
    module_ids: Vec<String>,
}

impl VirtualMic {
    fn create(log: &EventLog) -> Option<VirtualMic> {
        let mut ids = Vec::new();
        let run = |args: &[&str]| -> Option<String> {
            let out = std::process::Command::new("pactl").args(args).output().ok()?;
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        };
        let sinks = run(&["list", "short", "sinks"]).unwrap_or_default();
        if !sinks.lines().any(|l| l.split_whitespace().nth(1) == Some(VIRT_SINK)) {
            match run(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={VIRT_SINK}"),
                "sink_properties=device.description='ARP-Ara-Cikis'",
            ]) {
                Some(id) => ids.push(id),
                None => {
                    log.push("⚠ sanal mikrofon kurulamadı (pactl yok mu?)".into());
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
        log.push(format!("🎙 sanal mikrofon hazır: \"{VIRT_DESC}\""));
        Some(VirtualMic { module_ids: ids })
    }
}

impl Drop for VirtualMic {
    fn drop(&mut self) {
        for id in &self.module_ids {
            let _ = std::process::Command::new("pactl").args(["unload-module", id]).output();
        }
    }
}

fn adb_available() -> bool {
    std::process::Command::new("adb")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct App {
    shared: Shared,
    adj: Arc<AtomicI32>,
    log: Arc<EventLog>,
    ctl: AudioCtl,
    gain: f32,
    stream: Option<cpal::Stream>,
    loopback: Option<cpal::Stream>,
    aec_enabled: bool,
    out_desc: String,
    devices: Vec<String>,
    selected_device: Option<String>,
    virtual_mic_enabled: bool,
    virtual_mic: Option<VirtualMic>,
    net_ok: bool,
    error: Option<String>,
    adb_found: bool,
    pad_names: Vec<String>,
}

impl App {
    fn new() -> Self {
        let shared: Shared = Arc::new(Mutex::new(Engine::default()));
        let adj = Arc::new(AtomicI32::new(0));
        let log = EventLog::new(false);

        // Soundpad: exe'nin yanındaki ya da çalışma dizinindeki "soundpad" klasörü
        let pad_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("soundpad")))
            .filter(|d| d.is_dir())
            .or_else(|| {
                let d = std::path::PathBuf::from("soundpad");
                if d.is_dir() {
                    Some(d)
                } else {
                    None
                }
            });
        let pad = Arc::new(match &pad_dir {
            Some(d) => Soundpad::load_dir(d, &log),
            None => Soundpad::empty(),
        });
        let pad_names = pad.names();
        let ctl = AudioCtl::new(pad.clone());

        let mut net_ok = false;
        let mut error = None;
        match UdpSocket::bind(("0.0.0.0", proto::DEFAULT_PORT)) {
            Ok(sock) => {
                let name = engine::default_name();
                engine::spawn_net(sock, shared.clone(), name.clone(), 60, log.clone(), pad.clone());
                if let Err(e) = engine::spawn_tcp(proto::DEFAULT_PORT, shared.clone(), name, 60, log.clone(), pad.clone()) {
                    log.push(format!("⚠ USB/TCP dinleyicisi açılamadı: {e}"));
                }
                engine::spawn_supervisor(shared.clone(), adj.clone(), log.clone(), false);
                log.push(format!("🎧 UDP+TCP {} dinleniyor — telefondan bağlanabilirsiniz", proto::DEFAULT_PORT));
                net_ok = true;
            }
            Err(e) => {
                error = Some(format!(
                    "Port {} açılamadı: {e}\nBaşka bir arp-receiver/arp-gui açık olabilir.",
                    proto::DEFAULT_PORT
                ));
            }
        }

        let mut app = App {
            shared,
            adj,
            log,
            ctl,
            gain: 1.0,
            stream: None,
            loopback: None,
            aec_enabled: false,
            out_desc: String::new(),
            devices: engine::list_output_devices(),
            selected_device: None,
            virtual_mic_enabled: cfg!(target_os = "linux"),
            virtual_mic: None,
            net_ok,
            error,
            adb_found: adb_available(),
            pad_names,
        };
        if app.net_ok {
            app.apply_output();
        }
        app
    }

    /// Seçime göre ses çıkışını (ve Linux'ta sanal mikrofonu) kurar.
    fn apply_output(&mut self) {
        self.stream = None; // önce eskisini kapat

        #[cfg(target_os = "linux")]
        {
            if self.virtual_mic_enabled {
                if self.virtual_mic.is_none() {
                    self.virtual_mic = VirtualMic::create(&self.log);
                }
                if self.virtual_mic.is_some() {
                    // libpulse bağlantı anında okur; akıştan önce ayarla
                    std::env::set_var("PULSE_SINK", VIRT_SINK);
                    self.selected_device = Some("pulse".into());
                }
            } else {
                self.virtual_mic = None;
                std::env::remove_var("PULSE_SINK");
            }
        }

        match engine::run_audio(self.shared.clone(), self.adj.clone(), &self.selected_device, self.ctl.clone()) {
            Ok((stream, desc)) => {
                self.out_desc = desc.clone();
                self.stream = Some(stream);
                self.log.push(format!("🔊 çıkış: {desc}"));
            }
            Err(e) => {
                self.out_desc = String::new();
                self.log.push(format!("⚠ ses çıkışı açılamadı: {e}"));
            }
        }
    }

    fn toggle_aec(&mut self) {
        if self.aec_enabled {
            match arp::aec::spawn_loopback_reference(&self.log) {
                Ok((stream, ring)) => {
                    *self.ctl.aec.lock().unwrap() = Some(arp::aec::Aec::new(ring));
                    self.loopback = Some(stream);
                    self.log.push("🔇 yankı iptali etkin (deneysel)".into());
                }
                Err(e) => {
                    self.log.push(format!("⚠ yankı iptali açılamadı: {e}"));
                    self.aec_enabled = false;
                }
            }
        } else {
            *self.ctl.aec.lock().unwrap() = None;
            self.loopback = None;
            self.log.push("yankı iptali kapatıldı".into());
        }
    }

    fn setup_usb(&mut self) {
        let out = std::process::Command::new("adb")
            .args(["reverse", "tcp:48222", "tcp:48222"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                self.log.push("🔌 USB hazır: telefonda \"USB modu\"nu açıp bağlanın".into());
            }
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                self.log.push(format!("⚠ adb reverse başarısız: {}", err.trim()));
            }
            Err(e) => self.log.push(format!("⚠ adb çalıştırılamadı: {e}")),
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("🎙 AudioRelayPlus");
            ui.add_space(4.0);

            if let Some(err) = &self.error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
                return;
            }

            // Durum
            let snap = engine::snapshot(&self.shared);
            match &snap {
                Some(s) => {
                    let renk = if s.playing {
                        egui::Color32::from_rgb(80, 200, 100)
                    } else {
                        egui::Color32::YELLOW
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(renk, "●");
                        ui.label(format!("Bağlı: {}", s.peer));
                    });
                    ui.label(format!(
                        "tampon {}/{} ms   kurtarılan(FEC) {}   gizlenen(PLC) {}   kesinti {}",
                        s.fill_ms, s.target_ms, s.stats.fec_recovered, s.stats.plc_frames, s.stats.underruns
                    ));
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.colored_label(egui::Color32::GRAY, "○");
                        ui.label("Telefon bekleniyor — uygulamayı açın, PC otomatik görünür");
                    });
                }
            }

            ui.add_space(6.0);
            ui.separator();

            // Çıkış / sanal mikrofon
            #[cfg(target_os = "linux")]
            {
                let before = self.virtual_mic_enabled;
                ui.checkbox(
                    &mut self.virtual_mic_enabled,
                    format!("Sanal mikrofon (\"{VIRT_DESC}\") — Discord'da bunu seçin"),
                );
                if before != self.virtual_mic_enabled {
                    self.apply_output();
                }
            }
            #[cfg(target_os = "windows")]
            {
                ui.label("Discord/oyun için: VB-Cable kurun, aşağıdan \"CABLE Input\" seçin,");
                ui.label("Discord'da mikrofon olarak \"CABLE Output\" seçin.");
            }

            let combo_enabled = !(cfg!(target_os = "linux") && self.virtual_mic_enabled);
            ui.add_enabled_ui(combo_enabled, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Çıkış aygıtı:");
                    let current = self.selected_device.clone().unwrap_or_else(|| "(varsayılan)".into());
                    let mut changed: Option<Option<String>> = None;
                    egui::ComboBox::from_id_salt("dev")
                        .width(230.0)
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(self.selected_device.is_none(), "(varsayılan)").clicked() {
                                changed = Some(None);
                            }
                            for d in &self.devices {
                                if ui
                                    .selectable_label(self.selected_device.as_deref() == Some(d), d)
                                    .clicked()
                                {
                                    changed = Some(Some(d.clone()));
                                }
                            }
                        });
                    if ui.button("⟳").on_hover_text("aygıtları yenile").clicked() {
                        self.devices = engine::list_output_devices();
                    }
                    if let Some(sel) = changed {
                        self.selected_device = sel;
                        self.apply_output();
                    }
                });
            });
            if !self.out_desc.is_empty() {
                ui.label(egui::RichText::new(format!("aktif çıkış: {}", self.out_desc)).weak());
            }

            // Kazanç + yankı iptali
            ui.add_space(4.0);
            let slider = egui::Slider::new(&mut self.gain, 1.0..=4.0)
                .text("PC tarafı kazanç")
                .custom_formatter(|v, _| format!("×{v:.1}"));
            if ui.add(slider).changed() {
                self.ctl.set_gain(self.gain);
            }
            let before_aec = self.aec_enabled;
            ui.checkbox(&mut self.aec_enabled, "Yankı engelleme (deneysel) — kesin çözüm: kulaklık");
            if before_aec != self.aec_enabled {
                self.toggle_aec();
            }

            // USB
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("USB (kablo) modu:").strong());
                if self.adb_found {
                    if ui.button("USB bağlantısını kur").clicked() {
                        self.setup_usb();
                    }
                } else {
                    ui.label("adb bulunamadı — platform-tools kurulumu gerekir");
                }
            });
            ui.label(
                egui::RichText::new(
                    "Kablo tak → telefonda USB hata ayıklama açık olsun → düğmeye bas → uygulamada \"USB modu\"nu aç.",
                )
                .weak()
                .size(11.5),
            );

            // Soundpad
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Soundpad").strong());
                if !self.pad_names.is_empty() && ui.button("⏹ durdur").clicked() {
                    self.ctl.pad.stop_all();
                }
            });
            if self.pad_names.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "Programın yanına \"soundpad\" klasörü açıp içine mp3/ogg/wav/flac atın (yeniden başlatınca yüklenir). Telefondan da çalınabilir.",
                    )
                    .weak()
                    .size(11.5),
                );
            } else {
                egui::ScrollArea::vertical().max_height(120.0).id_salt("padscroll").show(ui, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        for (i, name) in self.pad_names.clone().iter().enumerate() {
                            if ui.button(format!("🔔 {name}")).clicked() {
                                self.ctl.pad.play(i);
                            }
                        }
                    });
                });
            }

            // Olay günlüğü
            ui.add_space(6.0);
            ui.separator();
            ui.label(egui::RichText::new("Olaylar").strong());
            egui::ScrollArea::vertical().stick_to_bottom(true).id_salt("logscroll").show(ui, |ui| {
                for line in self.log.tail(50) {
                    ui.label(egui::RichText::new(line).monospace().size(12.0));
                }
            });
        });

        ctx.request_repaint_after(std::time::Duration::from_millis(250));
    }
}
