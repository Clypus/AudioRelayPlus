//! Alıcı motoru: ağ dinleyicisi, oturum yönetimi, ses çekme yolu ve gözetmen.
//! Hem CLI (arp-receiver) hem GUI (arp-gui) bunun üzerine kuruludur.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::decoder::{OpusDec, PcmDec};
use crate::jitter::{FrameDecoder, JitterBuffer, JitterConfig, JitterStats, State};
use crate::protocol as proto;
use crate::resampler::Resampler;

pub struct Session {
    pub id: u32,
    pub peer: SocketAddr,
    pub jb: JitterBuffer,
    pub dec: Box<dyn FrameDecoder + Send>,
    pub last_packet: Instant,
    pub epoch: u64,
}

#[derive(Default)]
pub struct Engine {
    pub session: Option<Session>,
    pub epoch_counter: u64,
}

pub type Shared = Arc<Mutex<Engine>>;

/// Olay günlüğü: GUI son satırları gösterir, CLI isterse stdout'a da basar.
pub struct EventLog {
    lines: Mutex<VecDeque<String>>,
    echo_stdout: bool,
}

impl EventLog {
    pub fn new(echo_stdout: bool) -> Arc<Self> {
        Arc::new(EventLog { lines: Mutex::new(VecDeque::new()), echo_stdout })
    }

    pub fn push(&self, s: String) {
        if self.echo_stdout {
            println!("{s}");
        }
        let mut l = self.lines.lock().unwrap();
        l.push_back(s);
        while l.len() > 200 {
            l.pop_front();
        }
    }

    pub fn tail(&self, n: usize) -> Vec<String> {
        let l = self.lines.lock().unwrap();
        l.iter().rev().take(n).rev().cloned().collect()
    }
}

/// GUI'nin her karede okuduğu durum özeti.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub peer: SocketAddr,
    pub playing: bool,
    pub fill_ms: u32,
    pub target_ms: u32,
    pub stats: JitterStats,
}

pub fn snapshot(shared: &Shared) -> Option<Snapshot> {
    let eng = shared.lock().unwrap();
    eng.session.as_ref().map(|s| Snapshot {
        peer: s.peer,
        playing: s.jb.state() == State::Playing,
        fill_ms: s.jb.fill_ms(),
        target_ms: s.jb.target_ms(),
        stats: s.jb.stats,
    })
}

pub fn default_name() -> String {
    if cfg!(windows) {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into())
    } else {
        std::fs::read_to_string("/etc/hostname")
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "PC".into())
    }
}

fn make_decoder(codec: proto::Codec) -> Result<Box<dyn FrameDecoder + Send>> {
    Ok(match codec {
        proto::Codec::Opus => Box::new(OpusDec::new()?),
        proto::Codec::Pcm16 => Box::new(PcmDec::new()),
    })
}

/// UDP dinleyici iş parçacığını başlatır (keşif + oturum + ses paketleri).
pub fn spawn_net(sock: UdpSocket, shared: Shared, name: String, target_ms: u32, log: Arc<EventLog>) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let pkt = match proto::parse(&buf[..n]) {
                Some(p) => p,
                None => continue,
            };
            match pkt {
                proto::Packet::Discover { nonce } => {
                    log.push(format!("🔎 keşif isteği geldi: {src} — yanıtlandı"));
                    let port = sock.local_addr().map(|a| a.port()).unwrap_or(proto::DEFAULT_PORT);
                    let _ = sock.send_to(&proto::build_discover_reply(nonce, port, &name), src);
                }
                proto::Packet::Hello(h) => {
                    let mut eng = shared.lock().unwrap();
                    let already = matches!(&eng.session, Some(s) if s.id == h.session);
                    if !already {
                        if h.sample_rate != proto::SAMPLE_RATE || h.channels != 1 {
                            log.push(format!(
                                "⚠ desteklenmeyen format ({} Hz, {} kanal) — yok sayıldı",
                                h.sample_rate, h.channels
                            ));
                            continue;
                        }
                        let dec = match make_decoder(h.codec) {
                            Ok(d) => d,
                            Err(e) => {
                                log.push(format!("⚠ çözücü açılamadı: {e}"));
                                continue;
                            }
                        };
                        let cfg = JitterConfig {
                            frame_samples: h.codec.frame_samples(),
                            start_target_ms: target_ms,
                            ..Default::default()
                        };
                        eng.epoch_counter += 1;
                        let epoch = eng.epoch_counter;
                        eng.session = Some(Session {
                            id: h.session,
                            peer: src,
                            jb: JitterBuffer::new(cfg),
                            dec,
                            last_packet: Instant::now(),
                            epoch,
                        });
                        log.push(format!("📱 bağlandı: {src} (codec={:?}, çerçeve={} ms)", h.codec, h.frame_ms));
                    }
                    drop(eng);
                    let _ = sock.send_to(&proto::build_hello_ack(h.session), src);
                }
                proto::Packet::Audio { session, seq, payload, .. } => {
                    let mut eng = shared.lock().unwrap();
                    if let Some(s) = eng.session.as_mut() {
                        if s.id == session {
                            s.jb.push(seq, payload.to_vec());
                            s.last_packet = Instant::now();
                            s.peer = src;
                        }
                    }
                }
                proto::Packet::Heartbeat { session, time_ms } => {
                    // Yalnızca bilinen oturuma ACK: oturum düştüyse istemci
                    // ACK alamayıp yeniden HELLO göndersin (sessiz kopukluk olmasın).
                    let mut eng = shared.lock().unwrap();
                    let known = match eng.session.as_mut() {
                        Some(s) if s.id == session => {
                            s.last_packet = Instant::now();
                            true
                        }
                        _ => false,
                    };
                    drop(eng);
                    if known {
                        let _ = sock.send_to(&proto::build_heartbeat_ack(session, time_ms), src);
                    }
                }
                proto::Packet::Bye { session } => {
                    let mut eng = shared.lock().unwrap();
                    if matches!(&eng.session, Some(s) if s.id == session) {
                        eng.session = None;
                        log.push("👋 istemci ayrıldı".into());
                    }
                }
                _ => {}
            }
        }
    });
}

/// Gözetmen: saniyede bir oturum zaman aşımı + saat kayması servosu
/// (+ istenirse istatistik satırı).
pub fn spawn_supervisor(shared: Shared, adj: Arc<AtomicI32>, log: Arc<EventLog>, stats_lines: bool) {
    std::thread::spawn(move || {
        let mut smoothed = 0f64;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut eng = shared.lock().unwrap();
            let mut drop_session = false;
            if let Some(s) = eng.session.as_mut() {
                if s.last_packet.elapsed() > Duration::from_secs(5) {
                    drop_session = true;
                } else {
                    let fill = s.jb.fill_ms() as f64;
                    let target = s.jb.target_ms() as f64;
                    let raw = ((fill - target) * 100.0).clamp(-3000.0, 3000.0);
                    smoothed = if s.jb.state() == State::Playing {
                        0.7 * smoothed + 0.3 * raw
                    } else {
                        0.0
                    };
                    adj.store(smoothed as i32, Ordering::Relaxed);
                    if stats_lines {
                        let st = s.jb.stats;
                        log.push(format!(
                            "▸ {:?} tampon={:3}/{:3}ms plc={} fec={} atlanan={} underrun={} geç={} hız={:+}ppm",
                            s.jb.state(),
                            s.jb.fill_ms(),
                            s.jb.target_ms(),
                            st.plc_frames,
                            st.fec_recovered,
                            st.lost_skipped,
                            st.underruns,
                            st.late,
                            smoothed as i32
                        ));
                    }
                }
            }
            if drop_session {
                eng.session = None;
                smoothed = 0.0;
                adj.store(0, Ordering::Relaxed);
                log.push("⏱ oturum zaman aşımı — yeni bağlantı bekleniyor".into());
            }
        }
    });
}

/// Ortak tüketim yolu: oturumdan mono 48 kHz örnek çek (yoksa sessizlik).
pub fn pull_mono(
    shared: &Shared,
    rs: &mut Resampler,
    last_epoch: &mut u64,
    adj_ppm: &AtomicI32,
    gain: f32,
    out: &mut [f32],
) {
    let mut eng = shared.lock().unwrap();
    match eng.session.as_mut() {
        Some(s) => {
            if s.epoch != *last_epoch {
                rs.reset();
                *last_epoch = s.epoch;
            }
            rs.set_adj_ppm(adj_ppm.load(Ordering::Relaxed) as f64);
            let jb = &mut s.jb;
            let dec = s.dec.as_mut();
            rs.process(out, |buf| {
                jb.pull(buf, dec);
            });
            if gain != 1.0 {
                for s in out.iter_mut() {
                    // kübik soft-clip: sert kırpma yerine düzgün doyma
                    let x = (*s * gain).clamp(-1.5, 1.5);
                    *s = x - x * x * x / 6.75;
                }
            }
        }
        None => out.fill(0.0),
    }
}

pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(n) = d.name() {
                out.push(n);
            }
        }
    }
    out
}

fn pick_device(host: &cpal::Host, want: &Option<String>) -> Result<cpal::Device> {
    if let Some(pat) = want {
        let pat = pat.to_lowercase();
        for d in host.output_devices()? {
            if d.name().map(|n| n.to_lowercase().contains(&pat)).unwrap_or(false) {
                return Ok(d);
            }
        }
        return Err(anyhow!("'{pat}' ile eşleşen çıkış aygıtı yok"));
    }
    host.default_output_device().ok_or_else(|| anyhow!("varsayılan ses çıkışı bulunamadı"))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Shared,
    adj_ppm: Arc<AtomicI32>,
    gain_bits: Arc<AtomicU32>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut rs = Resampler::new(proto::SAMPLE_RATE as f64, config.sample_rate.0 as f64);
    let mut mono: Vec<f32> = Vec::new();
    let mut last_epoch = 0u64;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            let frames = data.len() / channels;
            mono.resize(frames, 0.0);
            let gain = f32::from_bits(gain_bits.load(Ordering::Relaxed));
            pull_mono(&shared, &mut rs, &mut last_epoch, &adj_ppm, gain, &mut mono);
            for (i, frame) in data.chunks_mut(channels).enumerate() {
                let v = T::from_sample(mono[i]);
                for ch in frame.iter_mut() {
                    *ch = v;
                }
            }
        },
        |e| eprintln!("⚠ ses akışı hatası: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Ses çıkış akışını kurar; (akış, açıklama) döner. `gain_bits` canlı değiştirilebilir.
pub fn run_audio(
    shared: Shared,
    adj_ppm: Arc<AtomicI32>,
    want: &Option<String>,
    gain_bits: Arc<AtomicU32>,
) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = pick_device(&host, want)?;
    let default = device.default_output_config().context("çıkış formatı alınamadı")?;
    let mut chosen = default.clone();
    if default.sample_rate().0 != proto::SAMPLE_RATE {
        if let Ok(configs) = device.supported_output_configs() {
            for c in configs {
                if c.min_sample_rate().0 <= proto::SAMPLE_RATE
                    && proto::SAMPLE_RATE <= c.max_sample_rate().0
                    && c.sample_format() == default.sample_format()
                {
                    chosen = c.with_sample_rate(cpal::SampleRate(proto::SAMPLE_RATE));
                    break;
                }
            }
        }
    }
    let config: cpal::StreamConfig = chosen.config();
    let desc = format!(
        "{} @ {} Hz, {} kanal",
        device.name().unwrap_or_else(|_| "?".into()),
        config.sample_rate.0,
        config.channels
    );
    let stream = match chosen.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, shared, adj_ppm, gain_bits)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, shared, adj_ppm, gain_bits)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, shared, adj_ppm, gain_bits)?,
        f => return Err(anyhow!("desteklenmeyen örnek formatı: {f:?}")),
    };
    stream.play()?;
    Ok((stream, desc))
}
