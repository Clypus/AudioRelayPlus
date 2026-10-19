//! Receiver engine: network listeners (UDP + USB/TCP), session handling, the
//! audio pull path (jitter buffer → mic DSP → mute → soundpad → gain), the
//! supervisor (timeouts + clock-drift servo) and self-healing sound outputs.
//! Both the CLI (arp-receiver) and the GUI (arp-gui) are built on top of it.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use crate::decoder::{OpusDec, PcmDec};
use crate::dsp::MicDsp;
use crate::jitter::{FrameDecoder, JitterBuffer, JitterConfig, JitterStats, State};
use crate::protocol as proto;
use crate::resampler::Resampler;
use crate::soundpad::Soundpad;
use crate::t;

/// How to reach the phone again — also later and unprompted (for example
/// with an updated soundpad list).
#[derive(Clone)]
pub enum ReplyPath {
    Udp { sock: Arc<UdpSocket>, to: SocketAddr },
    Tcp(Arc<Mutex<TcpStream>>),
}

impl ReplyPath {
    pub fn send(&self, pkt: &[u8]) {
        match self {
            ReplyPath::Udp { sock, to } => {
                let _ = sock.send_to(pkt, to);
            }
            ReplyPath::Tcp(s) => {
                let _ = s.lock().unwrap().write_all(&proto::tcp_frame(pkt));
            }
        }
    }

    pub fn transport(&self) -> &'static str {
        match self {
            ReplyPath::Udp { .. } => "Wi-Fi",
            ReplyPath::Tcp(_) => "USB",
        }
    }
}

pub struct Session {
    pub id: u32,
    pub peer: SocketAddr,
    pub reply: ReplyPath,
    pub jb: JitterBuffer,
    pub dec: Box<dyn FrameDecoder + Send>,
    pub last_packet: Instant,
    pub started: Instant,
    pub epoch: u64,
}

impl Session {
    /// One-line quality report, logged when the session ends.
    pub fn summary(&self) -> String {
        let st = &self.jb.stats;
        let secs = self.started.elapsed().as_secs();
        let dur = format!("{}:{:02}", secs / 60, secs % 60);
        let pct = st.concealed_ratio() * 100.0;
        let (fec, plc, late, drops, target) =
            (st.fec_recovered, st.plc_frames, st.late, st.underruns, self.jb.target_ms());
        t!(
            "📊 session {dur}: {pct:.1}% concealed (FEC {fec}, PLC {plc}), late {late}, dropouts {drops}, final buffer {target} ms",
            "📊 oturum {dur}: %{pct:.1} gizlendi (FEC {fec}, PLC {plc}), geç {late}, kesinti {drops}, son tampon {target} ms"
        )
    }
}

#[derive(Default)]
pub struct Engine {
    pub session: Option<Session>,
    pub epoch_counter: u64,
    /// Stats of the last finished session (headless runs report them at exit).
    pub last_stats: Option<JitterStats>,
}

impl Engine {
    /// Ends the current session (if any) and logs why, plus its quality summary.
    pub fn end_session(&mut self, log: &EventLog, why: String) {
        if let Some(s) = self.session.take() {
            log.push(why);
            log.push(s.summary());
            self.last_stats = Some(s.jb.stats);
        }
    }
}

pub type Shared = Arc<Mutex<Engine>>;

/// Live controls of the audio path, shared by the UI and the audio thread.
#[derive(Clone)]
pub struct AudioCtl {
    pub gain_bits: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    /// Highest output peak since the UI last looked, as f32 bits (positive
    /// floats order like their bit patterns, so `fetch_max` works on them).
    pub level_bits: Arc<AtomicU32>,
    /// Echo cancellation + noise suppression.
    pub dsp: Arc<Mutex<MicDsp>>,
    pub pad: Arc<Soundpad>,
}

impl AudioCtl {
    pub fn new(pad: Arc<Soundpad>) -> AudioCtl {
        AudioCtl {
            gain_bits: Arc::new(AtomicU32::new(1.0f32.to_bits())),
            muted: Arc::new(AtomicBool::new(false)),
            level_bits: Arc::new(AtomicU32::new(0)),
            dsp: Arc::new(Mutex::new(MicDsp::new())),
            pad,
        }
    }

    pub fn set_gain(&self, g: f32) {
        self.gain_bits.store(g.to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// Output peak (linear, 1.0 = full scale) since the previous call.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.level_bits.swap(0, Ordering::Relaxed))
    }
}

/// Event log: the GUI shows the last lines, the CLI can echo them to stdout.
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

/// State summary the GUI reads every frame.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub peer: SocketAddr,
    pub transport: &'static str,
    pub playing: bool,
    pub fill_ms: u32,
    pub target_ms: u32,
    pub stats: JitterStats,
}

pub fn snapshot(shared: &Shared) -> Option<Snapshot> {
    let eng = shared.lock().unwrap();
    eng.session.as_ref().map(|s| Snapshot {
        peer: s.peer,
        transport: s.reply.transport(),
        playing: s.jb.state() == State::Playing,
        fill_ms: s.jb.avg_fill_ms(),
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

/// What the network listeners share.
#[derive(Clone)]
pub struct NetCtx {
    pub shared: Shared,
    /// PC name shown in the phone's list.
    pub name: String,
    pub port: u16,
    /// Initial jitter buffer target for new sessions.
    pub target_ms: u32,
    pub log: Arc<EventLog>,
    pub pad: Arc<Soundpad>,
    /// Last logged discovery request (scans send a burst; log one line).
    last_discover: Arc<Mutex<Option<(IpAddr, Instant)>>>,
}

impl NetCtx {
    pub fn new(shared: Shared, name: String, port: u16, target_ms: u32, log: Arc<EventLog>, pad: Arc<Soundpad>) -> NetCtx {
        NetCtx { shared, name, port, target_ms, log, pad, last_discover: Arc::new(Mutex::new(None)) }
    }
}

/// Packet handler shared by UDP and TCP; answers go back through `reply`.
fn handle_packet(pkt: proto::Packet<'_>, src: SocketAddr, reply: &ReplyPath, ctx: &NetCtx) {
    let log = &ctx.log;
    match pkt {
        proto::Packet::Discover { nonce } => {
            // USB probes come over TCP on every scan of the app, and the
            // connected phone asks once to learn our name: not worth a line
            let from_session = matches!(&ctx.shared.lock().unwrap().session, Some(s) if s.peer == src);
            if matches!(reply, ReplyPath::Udp { .. }) && !from_session {
                let mut last = ctx.last_discover.lock().unwrap();
                let fresh = !matches!(*last, Some((ip, at)) if ip == src.ip() && at.elapsed() < Duration::from_secs(3));
                if fresh {
                    *last = Some((src.ip(), Instant::now()));
                    log.push(t!("🔎 discovery request from {src} — answered", "🔎 keşif isteği geldi: {src} — yanıtlandı"));
                }
            }
            reply.send(&proto::build_discover_reply(nonce, ctx.port, &ctx.name));
        }
        proto::Packet::Hello(h) => {
            let mut eng = ctx.shared.lock().unwrap();
            match eng.session.as_mut() {
                Some(s) if s.id == h.session => {
                    // Known session reconnecting (e.g. a new USB connection):
                    // from now on it lives here.
                    s.peer = src;
                    s.reply = reply.clone();
                    s.last_packet = Instant::now();
                }
                _ => {
                    if h.sample_rate != proto::SAMPLE_RATE || h.channels != 1 {
                        let (rate, ch) = (h.sample_rate, h.channels);
                        log.push(t!(
                            "⚠ unsupported format ({rate} Hz, {ch} channels) — ignored",
                            "⚠ desteklenmeyen format ({rate} Hz, {ch} kanal) — yok sayıldı"
                        ));
                        return;
                    }
                    let dec = match make_decoder(h.codec) {
                        Ok(d) => d,
                        Err(e) => {
                            log.push(t!("⚠ couldn't open the decoder: {e}", "⚠ çözücü açılamadı: {e}"));
                            return;
                        }
                    };
                    eng.end_session(log, t!("↪ previous session replaced", "↪ önceki oturum devralındı"));
                    let cfg = JitterConfig {
                        frame_samples: h.codec.frame_samples(),
                        start_target_ms: ctx.target_ms,
                        ..Default::default()
                    };
                    eng.epoch_counter += 1;
                    let epoch = eng.epoch_counter;
                    eng.session = Some(Session {
                        id: h.session,
                        peer: src,
                        reply: reply.clone(),
                        jb: JitterBuffer::new(cfg),
                        dec,
                        last_packet: Instant::now(),
                        started: Instant::now(),
                        epoch,
                    });
                    let (via, codec, ms) = (reply.transport(), h.codec, h.frame_ms);
                    log.push(t!(
                        "📱 connected: {src} via {via} (codec={codec:?}, frame={ms} ms)",
                        "📱 bağlandı: {src}, {via} üzerinden (codec={codec:?}, çerçeve={ms} ms)"
                    ));
                }
            }
            drop(eng);
            reply.send(&proto::build_hello_ack(h.session));
        }
        proto::Packet::Audio { session, seq, payload, .. } => {
            let mut eng = ctx.shared.lock().unwrap();
            if let Some(s) = eng.session.as_mut() {
                if s.id == session {
                    s.jb.push(seq, payload.to_vec());
                    s.last_packet = Instant::now();
                    if s.peer != src {
                        s.peer = src;
                        s.reply = reply.clone();
                    }
                }
            }
        }
        proto::Packet::Heartbeat { session, time_ms } => {
            // Only a known session gets an ACK: if ours was dropped, the client
            // must miss its ACKs and send HELLO again (no silent dead link).
            let mut eng = ctx.shared.lock().unwrap();
            let known = match eng.session.as_mut() {
                Some(s) if s.id == session => {
                    s.last_packet = Instant::now();
                    true
                }
                _ => false,
            };
            drop(eng);
            if known {
                reply.send(&proto::build_heartbeat_ack(session, time_ms));
            }
        }
        proto::Packet::Bye { session } => {
            let mut eng = ctx.shared.lock().unwrap();
            if matches!(&eng.session, Some(s) if s.id == session) {
                eng.end_session(log, t!("👋 the phone disconnected", "👋 telefon ayrıldı"));
            }
        }
        proto::Packet::SoundListReq { session } if is_current(ctx, session) => {
            reply.send(&proto::build_sound_list(&ctx.pad.names()));
        }
        proto::Packet::SoundPlay { session, id } if is_current(ctx, session) => {
            ctx.pad.play(id as usize);
            log.push(t!("🔔 soundpad (from the phone): #{id}", "🔔 soundpad (telefondan): #{id}"));
        }
        proto::Packet::SoundStop { session } if is_current(ctx, session) => {
            ctx.pad.stop_all();
        }
        _ => {}
    }
}

fn is_current(ctx: &NetCtx, session: u32) -> bool {
    matches!(&ctx.shared.lock().unwrap().session, Some(s) if s.id == session)
}

/// UDP listener (the Wi-Fi path).
pub fn spawn_net(sock: UdpSocket, ctx: NetCtx) {
    let sock = Arc::new(sock);
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            let (n, src) = match sock.recv_from(&mut buf) {
                Ok(v) => v,
                // e.g. Windows reports an earlier ICMP "port unreachable" here
                Err(_) => continue,
            };
            if let Some(pkt) = proto::parse(&buf[..n]) {
                let reply = ReplyPath::Udp { sock: sock.clone(), to: src };
                handle_packet(pkt, src, &reply, &ctx);
            }
        }
    });
}

/// USB/TCP listener: with `adb reverse tcp:48222 tcp:48222` the phone
/// connects to 127.0.0.1. Framing: [length u16 BE][packet]. Only the
/// loopback interface listens — adb is the only client.
pub fn spawn_tcp(ctx: NetCtx) -> Result<()> {
    let port = ctx.port;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| t!("couldn't open TCP port {port}", "TCP {port} portu açılamadı"))?;
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let ctx = ctx.clone();
            std::thread::spawn(move || serve_tcp(stream, ctx));
        }
    });
    Ok(())
}

fn serve_tcp(stream: TcpStream, ctx: NetCtx) {
    let Ok(peer) = stream.peer_addr() else { return };
    let _ = stream.set_nodelay(true);
    // The app sends a heartbeat every 500 ms: 10 s of silence = a dead link
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let Ok(writer) = stream.try_clone() else { return };
    let reply = ReplyPath::Tcp(Arc::new(Mutex::new(writer)));
    let mut reader = stream;
    let mut hdr = [0u8; 2];
    let mut body = vec![0u8; 4096];
    let mut last_session: Option<u32> = None;
    loop {
        if reader.read_exact(&mut hdr).is_err() {
            break;
        }
        let len = u16::from_be_bytes(hdr) as usize;
        if len == 0 || len > body.len() || reader.read_exact(&mut body[..len]).is_err() {
            break;
        }
        if let Some(pkt) = proto::parse(&body[..len]) {
            if let proto::Packet::Hello(h) = &pkt {
                if last_session.is_none() {
                    ctx.log.push(t!("🔌 USB connection: {peer}", "🔌 USB bağlantısı: {peer}"));
                }
                last_session = Some(h.session);
            }
            handle_packet(pkt, peer, &reply, &ctx);
        }
    }
    // Connection gone: end its session — unless a newer connection has
    // already taken it over (the app redials with the same session id).
    if let Some(sid) = last_session {
        let mut eng = ctx.shared.lock().unwrap();
        if matches!(&eng.session, Some(s) if s.id == sid && s.peer == peer) {
            eng.end_session(&ctx.log, t!("🔌 USB connection closed", "🔌 USB bağlantısı kapandı"));
        }
    }
}

/// Supervisor, once a second: session timeout, clock-drift servo, telling
/// the phone about soundpad changes (+ an optional stats line).
pub fn spawn_supervisor(shared: Shared, adj: Arc<AtomicI32>, log: Arc<EventLog>, pad: Arc<Soundpad>, stats_lines: bool) {
    std::thread::spawn(move || {
        let mut smoothed = 0f64;
        let mut pad_version = pad.version();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut eng = shared.lock().unwrap();
            let timed_out = matches!(&eng.session, Some(s) if s.last_packet.elapsed() > Duration::from_secs(5));
            if timed_out {
                eng.end_session(&log, t!("⏱ session timed out — waiting for a new connection", "⏱ oturum zaman aşımı — yeni bağlantı bekleniyor"));
                smoothed = 0.0;
                adj.store(0, Ordering::Relaxed);
            }
            let mut push_to = None;
            if let Some(s) = eng.session.as_mut() {
                // Steer by the smoothed fill: the instantaneous one jumps by a
                // whole frame each time a packet lands.
                let fill = s.jb.avg_fill_ms() as f64;
                let target = s.jb.target_ms() as f64;
                let raw = ((fill - target) * 100.0).clamp(-3000.0, 3000.0);
                smoothed = if s.jb.state() == State::Playing { 0.7 * smoothed + 0.3 * raw } else { 0.0 };
                adj.store(smoothed as i32, Ordering::Relaxed);
                if stats_lines {
                    let st = s.jb.stats;
                    let (state, fill, target, ppm) = (s.jb.state(), s.jb.avg_fill_ms(), s.jb.target_ms(), smoothed as i32);
                    let (plc, fec, stalls, skipped, drops, late) =
                        (st.plc_frames, st.fec_recovered, st.stalls, st.lost_skipped, st.underruns, st.late);
                    log.push(t!(
                        "▸ {state:?} buffer={fill:3}/{target:3}ms plc={plc} fec={fec} stalls={stalls} skipped={skipped} dropouts={drops} late={late} speed={ppm:+}ppm",
                        "▸ {state:?} tampon={fill:3}/{target:3}ms plc={plc} fec={fec} duraklama={stalls} atlanan={skipped} kesinti={drops} geç={late} hız={ppm:+}ppm"
                    ));
                }
                if pad.version() != pad_version {
                    push_to = Some(s.reply.clone());
                }
            }
            pad_version = pad.version();
            drop(eng);
            // The soundpad folder changed: the phone gets the new list right away
            if let Some(r) = push_to {
                r.send(&proto::build_sound_list(&pad.names()));
            }
        }
    });
}

/// Shared consuming path: the 48 kHz mono chain is jitter buffer → mic DSP
/// (echo/noise) → mute → soundpad, then device-rate conversion, gain and the
/// level meter. The soundpad keeps playing without a session too.
pub fn pull_mono(
    shared: &Shared,
    rs: &mut Resampler,
    last_epoch: &mut u64,
    adj_ppm: &AtomicI32,
    ctl: &AudioCtl,
    out: &mut [f32],
) {
    let gain = f32::from_bits(ctl.gain_bits.load(Ordering::Relaxed));
    let muted = ctl.is_muted();
    let mut eng = shared.lock().unwrap();
    let mut dsp = ctl.dsp.lock().unwrap();
    let pad = &ctl.pad;
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
                dsp.process(buf);
                if muted {
                    buf.fill(0.0);
                }
                pad.mix_into(buf);
            });
        }
        None => {
            rs.set_adj_ppm(0.0);
            rs.process(out, |buf| {
                buf.fill(0.0);
                // keeps the echo reference consumed (and aligned) between sessions
                dsp.process(buf);
                pad.mix_into(buf);
            });
        }
    }
    drop(dsp);
    drop(eng);
    if gain != 1.0 {
        for s in out.iter_mut() {
            // cubic soft clip: smooth saturation instead of hard clipping
            let x = (*s * gain).clamp(-1.5, 1.5);
            *s = x - x * x * x / 6.75;
        }
    }
    let peak = out.iter().fold(0f32, |m, &s| m.max(s.abs()));
    ctl.level_bits.fetch_max(peak.to_bits(), Ordering::Relaxed);
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
        return Err(anyhow!(t!("no output device matches '{pat}'", "'{pat}' ile eşleşen çıkış aygıtı yok")));
    }
    host.default_output_device()
        .ok_or_else(|| anyhow!(t!("no default sound output found", "varsayılan ses çıkışı bulunamadı")))
}

/// Render function of one output stream: fills device-rate mono samples.
pub type Render = Box<dyn FnMut(&mut [f32]) + Send>;

/// Makes the render function for a device running at the given rate.
pub type RenderFactory = Arc<dyn Fn(u32) -> Render + Send + Sync>;

/// Render path of the main output: the phone's microphone (see `pull_mono`).
pub fn mic_render(shared: Shared, adj_ppm: Arc<AtomicI32>, ctl: AudioCtl) -> RenderFactory {
    Arc::new(move |rate| {
        let (shared, adj_ppm, ctl) = (shared.clone(), adj_ppm.clone(), ctl.clone());
        let mut rs = Resampler::new(proto::SAMPLE_RATE as f64, rate as f64);
        let mut last_epoch = 0u64;
        Box::new(move |out: &mut [f32]| pull_mono(&shared, &mut rs, &mut last_epoch, &adj_ppm, &ctl, out))
    })
}

/// Render path of the soundpad monitor (local speakers).
pub fn monitor_render(pad: Arc<Soundpad>) -> RenderFactory {
    Arc::new(move |rate| {
        let pad = pad.clone();
        let mut rs = Resampler::new(proto::SAMPLE_RATE as f64, rate as f64);
        Box::new(move |out: &mut [f32]| {
            rs.process(out, |buf| {
                buf.fill(0.0);
                pad.mix_monitor_into(buf);
            })
        })
    })
}

/// Liveness of an output stream, fed by its callbacks.
struct StreamHealth {
    born: Instant,
    /// ms since `born` of the last data callback (0 = none yet).
    last_cb_ms: AtomicU64,
    /// ms since `born` of the last error (0 = none).
    error_ms: AtomicU64,
    error: Mutex<String>,
}

impl StreamHealth {
    fn new() -> Arc<StreamHealth> {
        Arc::new(StreamHealth {
            born: Instant::now(),
            last_cb_ms: AtomicU64::new(0),
            error_ms: AtomicU64::new(0),
            error: Mutex::new(String::new()),
        })
    }

    fn now_ms(&self) -> u64 {
        self.born.elapsed().as_millis() as u64 + 1
    }

    fn beat(&self) {
        self.last_cb_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    fn fail(&self, e: String) {
        *self.error.lock().unwrap() = e;
        self.error_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    /// Why the stream should be rebuilt, if it should: an error after which
    /// the callbacks stopped (WASAPI ends the stream on device loss), or no
    /// callbacks at all for 3 s.
    fn problem(&self) -> Option<String> {
        let now = self.now_ms();
        let last = self.last_cb_ms.load(Ordering::Relaxed);
        let err_at = self.error_ms.load(Ordering::Relaxed);
        if err_at > 0 && last <= err_at && now.saturating_sub(err_at) >= 500 {
            return Some(self.error.lock().unwrap().clone());
        }
        if now.saturating_sub(last) >= 3000 {
            return Some(t!("no audio for 3 s", "3 sn'dir ses akmıyor"));
        }
        None
    }
}

fn open_output(want: &Option<String>, factory: &RenderFactory, health: Arc<StreamHealth>) -> Result<(cpal::Stream, String)> {
    let host = cpal::default_host();
    let device = pick_device(&host, want)?;
    let default = device
        .default_output_config()
        .context(t!("couldn't get the output format", "çıkış formatı alınamadı"))?;
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
    let name = device.name().unwrap_or_else(|_| "?".into());
    let (rate, ch) = (config.sample_rate.0, config.channels);
    let desc = t!("{name} @ {rate} Hz, {ch} ch", "{name} @ {rate} Hz, {ch} kanal");
    let render = factory(rate);
    let stream = match chosen.sample_format() {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, render, health)?,
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, render, health)?,
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, render, health)?,
        f => return Err(anyhow!(t!("unsupported sample format: {f:?}", "desteklenmeyen örnek formatı: {f:?}"))),
    };
    stream.play()?;
    Ok((stream, desc))
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut render: Render,
    health: Arc<StreamHealth>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut mono: Vec<f32> = Vec::new();
    let beat = health.clone();
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _| {
            beat.beat();
            let frames = data.len() / channels;
            mono.resize(frames, 0.0);
            render(&mut mono);
            for (frame, &v) in data.chunks_mut(channels).zip(mono.iter()) {
                let s = T::from_sample(v);
                for ch in frame.iter_mut() {
                    *ch = s;
                }
            }
        },
        move |e| health.fail(e.to_string()),
        None,
    )?;
    Ok(stream)
}

#[derive(Clone, Debug, Default)]
pub struct OutputStatus {
    /// "device @ rate, channels" while a stream is running.
    pub active: Option<String>,
    /// Why there is no stream right now (cleared once one runs again).
    pub error: Option<String>,
}

enum OutCmd {
    Open(Option<String>),
    Close,
}

/// Owns one output stream on its own thread and keeps it alive: if the
/// device errors out or stops calling back (VB-Cable reset, headset
/// unplugged, sleep/resume, audio service restart...), the stream is
/// rebuilt; if the device is missing, it is retried every few seconds.
pub struct OutputManager {
    tx: mpsc::Sender<OutCmd>,
    status: Arc<Mutex<OutputStatus>>,
}

impl OutputManager {
    /// `label` names this output in the event log ("output", "monitor"...).
    pub fn spawn(label: String, factory: RenderFactory, log: Arc<EventLog>) -> OutputManager {
        let (tx, rx) = mpsc::channel::<OutCmd>();
        let status = Arc::new(Mutex::new(OutputStatus::default()));
        let st = status.clone();
        std::thread::spawn(move || {
            // The stream is created, used and dropped on this thread only
            // (cpal streams aren't Send on every platform).
            let mut want: Option<Option<String>> = None;
            let mut current: Option<(cpal::Stream, Arc<StreamHealth>)> = None;
            let mut retry_at: Option<Instant> = None;
            let mut last_err: Option<String> = None;
            loop {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(OutCmd::Open(dev)) => {
                        current = None; // close the old one first
                        want = Some(dev);
                        retry_at = Some(Instant::now());
                        last_err = None;
                    }
                    Ok(OutCmd::Close) => {
                        current = None;
                        want = None;
                        *st.lock().unwrap() = OutputStatus::default();
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
                if let Some(why) = current.as_ref().and_then(|(_, h)| h.problem()) {
                    log.push(t!("⚠ {label} stopped ({why}) — reopening…", "⚠ {label} durdu ({why}) — yeniden açılıyor…"));
                    current = None;
                    st.lock().unwrap().active = None;
                    retry_at = Some(Instant::now() + Duration::from_secs(1));
                }
                let due = retry_at.is_some_and(|at| Instant::now() >= at);
                if let (None, Some(dev), true) = (&current, &want, due) {
                    let health = StreamHealth::new();
                    match open_output(dev, &factory, health.clone()) {
                        Ok((stream, desc)) => {
                            log.push(format!("🔊 {label}: {desc}"));
                            *st.lock().unwrap() = OutputStatus { active: Some(desc), error: None };
                            current = Some((stream, health));
                            retry_at = None;
                            last_err = None;
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            if last_err.as_deref() != Some(msg.as_str()) {
                                log.push(t!("⚠ couldn't open {label}: {msg}", "⚠ {label} açılamadı: {msg}"));
                            }
                            *st.lock().unwrap() = OutputStatus { active: None, error: Some(msg.clone()) };
                            last_err = Some(msg);
                            retry_at = Some(Instant::now() + Duration::from_secs(3));
                        }
                    }
                }
            }
        });
        OutputManager { tx, status }
    }

    /// (Re)opens the output on a device (name substring; None = default).
    pub fn open(&self, device: Option<String>) {
        let _ = self.tx.send(OutCmd::Open(device));
    }

    pub fn close(&self) {
        let _ = self.tx.send(OutCmd::Close);
    }

    pub fn status(&self) -> OutputStatus {
        self.status.lock().unwrap().clone()
    }
}
