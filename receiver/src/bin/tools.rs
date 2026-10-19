//! arp-tools — development/test tools.
//!
//! `send`    : encodes a sine wave with Opus/PCM and streams it to the
//!             receiver, simulating loss, jitter (incl. reordering), Wi-Fi
//!             stalls and clock drift.
//! `analyze` : looks for dropouts (silent gaps) and clicks in the wav the
//!             receiver wrote with `--dump` — an objective "choppy" check.
//!             Note that it can't see concealment (PLC/FEC) on a sine: read
//!             the receiver's session summary for that.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arp::protocol as proto;

#[derive(Parser)]
#[command(name = "arp-tools", version, about = "AudioRelayPlus test tools")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Send a test tone under simulated network conditions
    Send(SendArgs),
    /// Analyze a recorded wav for dropouts and clicks
    Analyze(AnalyzeArgs),
}

#[derive(Parser, Debug)]
struct SendArgs {
    /// Receiver address
    #[arg(long, default_value = "127.0.0.1:48222")]
    target: String,
    /// Codec: opus | pcm
    #[arg(long, default_value = "opus")]
    codec: String,
    /// Duration (seconds)
    #[arg(long, default_value_t = 10.0)]
    duration: f64,
    /// Packet loss percentage (0-100)
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Upper bound of the random per-packet delay (ms) — causes reordering
    #[arg(long, default_value_t = 0.0)]
    jitter: f64,
    /// Every this many seconds, hold all packets back (a Wi-Fi power-save /
    /// scan stall); they then arrive in one burst. 0 = off
    #[arg(long, default_value_t = 0.0)]
    spike_every: f64,
    /// Length of such a stall (ms)
    #[arg(long, default_value_t = 150.0)]
    spike_ms: f64,
    /// Sender clock drift (ppm; +200 = the phone's clock runs 0.02% fast)
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    drift_ppm: f64,
    /// Opus bitrate (b/s)
    #[arg(long, default_value_t = 64000)]
    bitrate: i32,
    /// Test tone frequency (Hz)
    #[arg(long, default_value_t = 440.0)]
    freq: f64,
    /// Random seed (for repeatable tests)
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// Use TCP instead of UDP (tests the USB/adb path; loss/jitter are ignored)
    #[arg(long)]
    tcp: bool,
}

#[derive(Parser, Debug)]
struct AnalyzeArgs {
    /// The wav file to inspect
    wav: PathBuf,
    /// Skip this much at the start (initial buffering, ms)
    #[arg(long, default_value_t = 1000)]
    skip_ms: u64,
    /// Skip this much at the end (ms)
    #[arg(long, default_value_t = 300)]
    tail_ms: u64,
    /// Silence longer than this counts as a DROPOUT (ms)
    #[arg(long, default_value_t = 20.0)]
    gap_ms: f64,
}

/// xorshift64* — dependency-free seeded RNG
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Send + read-ACK abstraction: a UDP datagram or a TCP frame.
enum Link {
    Udp(UdpSocket),
    Tcp(std::net::TcpStream),
}

impl Link {
    fn send(&mut self, pkt: &[u8]) -> std::io::Result<()> {
        match self {
            Link::Udp(s) => s.send(pkt).map(|_| ()),
            Link::Tcp(s) => {
                use std::io::Write;
                s.write_all(&proto::tcp_frame(pkt))
            }
        }
    }

    /// Tries to read one packet (blocks, subject to the read timeout).
    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Link::Udp(s) => s.recv(buf),
            Link::Tcp(s) => {
                use std::io::Read;
                let mut hdr = [0u8; 2];
                s.read_exact(&mut hdr)?;
                let len = u16::from_be_bytes(hdr) as usize;
                if len > buf.len() {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too big"));
                }
                s.read_exact(&mut buf[..len])?;
                Ok(len)
            }
        }
    }

    fn set_read_timeout(&self, t: Option<Duration>) -> std::io::Result<()> {
        match self {
            Link::Udp(s) => s.set_read_timeout(t),
            Link::Tcp(s) => s.set_read_timeout(t),
        }
    }

    fn try_clone(&self) -> std::io::Result<Link> {
        Ok(match self {
            Link::Udp(s) => Link::Udp(s.try_clone()?),
            Link::Tcp(s) => Link::Tcp(s.try_clone()?),
        })
    }
}

fn cmd_send(a: SendArgs) -> Result<()> {
    let codec = match a.codec.as_str() {
        "opus" => proto::Codec::Opus,
        "pcm" => proto::Codec::Pcm16,
        other => return Err(anyhow!("unknown codec: {other}")),
    };
    let frame_samples = codec.frame_samples();
    let frame_ms = frame_samples as f64 * 1000.0 / proto::SAMPLE_RATE as f64;
    if a.tcp && (a.loss > 0.0 || a.jitter > 0.0 || a.spike_every > 0.0) {
        eprintln!("note: loss/jitter/stall simulation does not apply with --tcp");
    }

    let mut link = if a.tcp {
        let s = std::net::TcpStream::connect(&a.target)
            .with_context(|| format!("couldn't connect over TCP: {}", a.target))?;
        s.set_nodelay(true)?;
        Link::Tcp(s)
    } else {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.connect(&a.target).with_context(|| format!("couldn't reach: {}", a.target))?;
        Link::Udp(sock)
    };

    let mut rng = Rng::new(a.seed);
    let session: u32 = (rng.next_u64() >> 32) as u32;

    // HELLO / HELLO_ACK handshake
    let hello = proto::build_hello(&proto::Hello {
        session,
        sample_rate: proto::SAMPLE_RATE,
        channels: 1,
        codec,
        frame_ms: frame_ms as u8,
    });
    link.set_read_timeout(Some(Duration::from_millis(300)))?;
    let mut connected = false;
    let mut rbuf = [0u8; 512];
    for i in 0..10 {
        link.send(&hello)?;
        if let Ok(n) = link.recv(&mut rbuf) {
            if let Some(proto::Packet::HelloAck { session: s }) = proto::parse(&rbuf[..n]) {
                if s == session {
                    connected = true;
                    break;
                }
            }
        }
        if i == 9 {
            return Err(anyhow!("the receiver did not answer (no HELLO_ACK)"));
        }
    }
    assert!(connected);
    println!(
        "connected: {} ({}, codec={:?}, session={session:08x})",
        a.target,
        if a.tcp { "TCP" } else { "UDP" },
        codec
    );

    // ACK reader thread (RTT measurement) — over TCP it also keeps the framing in sync
    let t0 = Instant::now();
    let ack_stats = std::sync::Arc::new(std::sync::Mutex::new((0u64, 0f64))); // (hb_acks, rtt_sum_ms)
    {
        let mut rlink = link.try_clone()?;
        rlink.set_read_timeout(Some(Duration::from_millis(500)))?;
        let ack_stats = ack_stats.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                match rlink.recv(&mut buf) {
                    Ok(n) => {
                        if let Some(proto::Packet::HeartbeatAck { session: s, time_ms }) = proto::parse(&buf[..n]) {
                            if s == session {
                                let mut g = ack_stats.lock().unwrap();
                                g.0 += 1;
                                g.1 += t0.elapsed().as_millis() as f64 - time_ms as f64;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
        });
    }

    let mut enc = match codec {
        proto::Codec::Opus => {
            let mut e = opus::Encoder::new(proto::SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip)?;
            e.set_bitrate(opus::Bitrate::Bits(a.bitrate))?;
            e.set_inband_fec(true)?;
            e.set_packet_loss_perc(15)?;
            Some(e)
        }
        proto::Codec::Pcm16 => None,
    };

    let n_frames = (a.duration * 1000.0 / frame_ms).round() as u32;
    // drift > 0: the sender's clock is fast → frames come SHORTER than 20 ms apart
    let interval = Duration::from_secs_f64(frame_ms / 1000.0 * (1.0 - a.drift_ppm * 1e-6));

    let mut phase = 0f64;
    let phase_step = 2.0 * std::f64::consts::PI * a.freq / proto::SAMPLE_RATE as f64;
    let mut pcm = vec![0i16; frame_samples];
    let mut ebuf = vec![0u8; 1500];

    let (loss, jitter) = if a.tcp { (0.0, 0.0) } else { (a.loss, a.jitter) };
    let (spike_every, spike_ms) = if a.tcp { (0.0, 0.0) } else { (a.spike_every, a.spike_ms) };
    let mut heap: BinaryHeap<Reverse<(Instant, u64, Vec<u8>)>> = BinaryHeap::new();
    let mut heap_ctr = 0u64;
    let mut k = 0u32;
    let (mut sim_dropped, mut sent) = (0u64, 0u64);
    let mut last_hb = Instant::now();

    while k < n_frames || !heap.is_empty() {
        let now = Instant::now();

        // Produce the frames that are due
        while k < n_frames && t0 + interval * k <= now {
            for s in pcm.iter_mut() {
                *s = (phase.sin() * 0.5 * 32767.0) as i16;
                phase += phase_step;
            }
            let payload: Vec<u8> = match enc.as_mut() {
                Some(e) => {
                    let n = e.encode(&pcm, &mut ebuf)?;
                    ebuf[..n].to_vec()
                }
                None => pcm.iter().flat_map(|s| s.to_le_bytes()).collect(),
            };
            let pkt = proto::build_audio(session, k, k.wrapping_mul(frame_samples as u32), &payload);
            if rng.unit() * 100.0 < loss {
                sim_dropped += 1;
            } else {
                let delay = Duration::from_secs_f64(rng.unit() * jitter / 1000.0);
                let mut when = now + delay;
                // Inside a stall window nothing gets through until it ends
                if spike_every > 0.0 {
                    let t = now.duration_since(t0).as_secs_f64();
                    let into = t % spike_every;
                    if t >= spike_every && into < spike_ms / 1000.0 {
                        let end = now + Duration::from_secs_f64(spike_ms / 1000.0 - into);
                        when = when.max(end);
                    }
                }
                heap.push(Reverse((when, heap_ctr, pkt)));
                heap_ctr += 1;
            }
            k += 1;
        }

        // Heartbeat (sent without loss/jitter; it is the liveness channel)
        if last_hb.elapsed() >= Duration::from_millis(500) {
            last_hb = Instant::now();
            let ms = t0.elapsed().as_millis() as u32;
            let _ = link.send(&proto::build_heartbeat(session, ms));
        }

        // Send the packets whose time has come
        while let Some(Reverse((when, _, _))) = heap.peek() {
            if *when <= Instant::now() {
                let Reverse((_, _, pkt)) = heap.pop().unwrap();
                let _ = link.send(&pkt);
                sent += 1;
            } else {
                break;
            }
        }

        std::thread::sleep(Duration::from_millis(2));
    }

    for _ in 0..3 {
        let _ = link.send(&proto::build_bye(session));
        std::thread::sleep(Duration::from_millis(20));
    }

    let (hb_acks, rtt_sum_ms) = *ack_stats.lock().unwrap();
    println!(
        "done: {} frames produced, {} sent, {} dropped by the simulation ({:.1}%), {} hb-acks, avg RTT {:.1} ms",
        n_frames,
        sent,
        sim_dropped,
        100.0 * sim_dropped as f64 / n_frames.max(1) as f64,
        hb_acks,
        if hb_acks > 0 { rtt_sum_ms / hb_acks as f64 } else { 0.0 }
    );
    Ok(())
}

fn cmd_analyze(a: AnalyzeArgs) -> Result<()> {
    let mut reader = hound::WavReader::open(&a.wav)?;
    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(anyhow!("expected a mono wav (got {} channels)", spec.channels));
    }
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|v| v as f32 / max)).collect::<Result<_, _>>()?
        }
    };
    let rate = spec.sample_rate as u64;
    let skip = (a.skip_ms * rate / 1000) as usize;
    let tail = (a.tail_ms * rate / 1000) as usize;
    // The natural silence after the stream ended is not a dropout: trim the
    // trailing silence and inspect up to the last active sample.
    const SILENCE: f32 = 1e-5;
    let active_end = samples
        .iter()
        .rposition(|s| s.abs() > SILENCE)
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = active_end.saturating_sub(tail).min(samples.len());
    if end <= skip {
        return Err(anyhow!("no active audio to inspect (active part: {} samples)", active_end));
    }
    let region = &samples[skip..end];
    let ms = |n: usize| n as f64 * 1000.0 / rate as f64;

    let mut gaps: Vec<(usize, usize)> = Vec::new(); // (start, length)
    let mut run = 0usize;
    for (i, &s) in region.iter().enumerate() {
        if s.abs() <= SILENCE {
            run += 1;
        } else {
            if ms(run) >= 5.0 {
                gaps.push((i - run, run));
            }
            run = 0;
        }
    }
    if ms(run) >= 5.0 {
        gaps.push((region.len() - run, run));
    }

    let mut clicks = 0u64;
    for w in region.windows(2) {
        if (w[1] - w[0]).abs() > 0.25 {
            clicks += 1;
        }
    }

    let rms = (region.iter().map(|s| (s * s) as f64).sum::<f64>() / region.len() as f64).sqrt();
    let bad: Vec<_> = gaps.iter().filter(|(_, n)| ms(*n) >= a.gap_ms).collect();
    let longest = gaps.iter().map(|(_, n)| *n).max().unwrap_or(0);

    println!("file            : {} ({:.1} s, {} Hz)", a.wav.display(), ms(samples.len()) / 1000.0, rate);
    println!("inspected       : {:.1} s ({} ms skipped at the start, {} ms at the end)", ms(region.len()) / 1000.0, a.skip_ms, a.tail_ms);
    println!("RMS             : {:.4}", rms);
    println!("≥5 ms silences  : {} (longest {:.1} ms)", gaps.len(), ms(longest));
    println!("clicks (jumps)  : {clicks}");
    for (start, n) in &bad {
        println!("  ❌ dropout at {:.2} s, {:.1} ms", (ms(*start) + a.skip_ms as f64) / 1000.0, ms(*n));
    }
    if bad.is_empty() && rms > 0.05 {
        println!("RESULT: CLEAN ✅ (no dropout over {:.0} ms)", a.gap_ms);
        Ok(())
    } else if rms <= 0.05 {
        println!("RESULT: FAILED ❌ (the recording looks silent, RMS={rms:.4})");
        std::process::exit(1);
    } else {
        println!("RESULT: CHOPPY ❌ ({} gap(s) over {:.0} ms)", bad.len(), a.gap_ms);
        std::process::exit(1);
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Send(a) => cmd_send(a),
        Cmd::Analyze(a) => cmd_analyze(a),
    }
}
