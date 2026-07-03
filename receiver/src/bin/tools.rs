//! arp-tools — geliştirme/test araçları.
//!
//! `send`    : sinüs dalgasını Opus/PCM ile kodlayıp alıcıya yollar; kayıp,
//!             jitter (yeniden sıralama dahil) ve saat kayması simüle eder.
//! `analyze` : alıcının `--dump` çıktısındaki wav'da kesinti (sessizlik
//!             boşluğu) ve klik arar — "kesik kesik" için objektif ölçüm.

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use arp::protocol as proto;

#[derive(Parser)]
#[command(name = "arp-tools", version, about = "AudioRelayPlus test araçları")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Simüle ağ koşullarında test sesi gönder
    Send(SendArgs),
    /// Kaydedilen wav'da kesinti/klik analizi yap
    Analyze(AnalyzeArgs),
}

#[derive(Parser, Debug)]
struct SendArgs {
    /// Alıcı adresi
    #[arg(long, default_value = "127.0.0.1:48222")]
    target: String,
    /// Codec: opus | pcm
    #[arg(long, default_value = "opus")]
    codec: String,
    /// Süre (saniye)
    #[arg(long, default_value_t = 10.0)]
    duration: f64,
    /// Paket kaybı yüzdesi (0-100)
    #[arg(long, default_value_t = 0.0)]
    loss: f64,
    /// Pakete eklenen rastgele gecikme üst sınırı (ms) — yeniden sıralamaya yol açar
    #[arg(long, default_value_t = 0.0)]
    jitter: f64,
    /// Gönderici saat kayması (ppm; +200 = telefon saati %0,02 hızlı)
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    drift_ppm: f64,
    /// Opus bitrate (b/s)
    #[arg(long, default_value_t = 64000)]
    bitrate: i32,
    /// Test tonu frekansı (Hz)
    #[arg(long, default_value_t = 440.0)]
    freq: f64,
    /// Rastgelelik tohumu (tekrarlanabilir testler için)
    #[arg(long, default_value_t = 1)]
    seed: u64,
    /// UDP yerine TCP kullan (USB/adb yolunun testi; kayıp/jitter yok sayılır)
    #[arg(long)]
    tcp: bool,
}

#[derive(Parser, Debug)]
struct AnalyzeArgs {
    /// İncelenecek wav dosyası
    wav: PathBuf,
    /// Baştan atlanacak süre (başlangıç tamponlaması, ms)
    #[arg(long, default_value_t = 1000)]
    skip_ms: u64,
    /// Sondan atlanacak süre (ms)
    #[arg(long, default_value_t = 300)]
    tail_ms: u64,
    /// Bu süreden uzun sessizlik = KESİNTİ sayılır (ms)
    #[arg(long, default_value_t = 20.0)]
    gap_ms: f64,
}

/// xorshift64* — bağımlılıksız, tohumlu RNG
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

/// Gönderim + ACK okuma soyutlaması: UDP datagram ya da TCP çerçeve.
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

    /// Bir paket okumaya çalışır (zaman aşımı ayarına tabi, bloklar).
    fn recv(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Link::Udp(s) => s.recv(buf),
            Link::Tcp(s) => {
                use std::io::Read;
                let mut hdr = [0u8; 2];
                s.read_exact(&mut hdr)?;
                let len = u16::from_be_bytes(hdr) as usize;
                if len > buf.len() {
                    return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "çerçeve büyük"));
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
        other => return Err(anyhow!("bilinmeyen codec: {other}")),
    };
    let frame_samples = codec.frame_samples();
    let frame_ms = frame_samples as f64 * 1000.0 / proto::SAMPLE_RATE as f64;
    if a.tcp && (a.loss > 0.0 || a.jitter > 0.0) {
        eprintln!("not: --tcp modunda kayıp/jitter simülasyonu uygulanmaz");
    }

    let mut link = if a.tcp {
        let s = std::net::TcpStream::connect(&a.target)
            .with_context(|| format!("TCP hedefe bağlanılamadı: {}", a.target))?;
        s.set_nodelay(true)?;
        Link::Tcp(s)
    } else {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.connect(&a.target).with_context(|| format!("hedefe bağlanılamadı: {}", a.target))?;
        Link::Udp(sock)
    };

    let mut rng = Rng::new(a.seed);
    let session: u32 = (rng.next_u64() >> 32) as u32;

    // HELLO / HELLO_ACK el sıkışması
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
            return Err(anyhow!("alıcı yanıt vermedi (HELLO_ACK yok)"));
        }
    }
    assert!(connected);
    println!(
        "bağlandı: {} ({}, codec={:?}, session={session:08x})",
        a.target,
        if a.tcp { "TCP" } else { "UDP" },
        codec
    );

    // ACK okuyucu iş parçacığı (RTT ölçümü) — TCP'de çerçeve senkronu için şart
    let t0 = Instant::now();
    let ack_stats = std::sync::Arc::new(std::sync::Mutex::new((0u64, 0f64))); // (hb_acks, rtt_toplam_ms)
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
    // drift > 0: gönderici saati hızlı → çerçeveler gerçek 20 ms'den KISA aralıkla gelir
    let interval = Duration::from_secs_f64(frame_ms / 1000.0 * (1.0 - a.drift_ppm * 1e-6));

    let mut phase = 0f64;
    let phase_step = 2.0 * std::f64::consts::PI * a.freq / proto::SAMPLE_RATE as f64;
    let mut pcm = vec![0i16; frame_samples];
    let mut ebuf = vec![0u8; 1500];

    let (loss, jitter) = if a.tcp { (0.0, 0.0) } else { (a.loss, a.jitter) };
    let mut heap: BinaryHeap<Reverse<(Instant, u64, Vec<u8>)>> = BinaryHeap::new();
    let mut heap_ctr = 0u64;
    let mut k = 0u32;
    let (mut sim_dropped, mut sent) = (0u64, 0u64);
    let mut last_hb = Instant::now();

    while k < n_frames || !heap.is_empty() {
        let now = Instant::now();

        // Sırası gelen çerçeveleri üret
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
                heap.push(Reverse((now + delay, heap_ctr, pkt)));
                heap_ctr += 1;
            }
            k += 1;
        }

        // Kalp atışı (kayıpsız/jittersız gönderilir; canlılık kanalı)
        if last_hb.elapsed() >= Duration::from_millis(500) {
            last_hb = Instant::now();
            let ms = t0.elapsed().as_millis() as u32;
            let _ = link.send(&proto::build_heartbeat(session, ms));
        }

        // Zamanı gelen paketleri gönder
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
        "bitti: {} çerçeve üretildi, {} gönderildi, {} simüle kayıp (%{:.1}), {} hb-ack, ort. RTT {:.1} ms",
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
        return Err(anyhow!("mono wav bekleniyordu ({} kanal geldi)", spec.channels));
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
    // Yayın bittikten sonraki doğal sessizliği kesinti sayma: sondaki
    // sessizliği kırp, son aktif örneğe kadar incele.
    const SILENCE: f32 = 1e-5;
    let active_end = samples
        .iter()
        .rposition(|s| s.abs() > SILENCE)
        .map(|i| i + 1)
        .unwrap_or(0);
    let end = active_end.saturating_sub(tail).min(samples.len());
    if end <= skip {
        return Err(anyhow!("incelenecek aktif ses yok (aktif bölüm {} örnek)", active_end));
    }
    let region = &samples[skip..end];
    let ms = |n: usize| n as f64 * 1000.0 / rate as f64;

    let mut gaps: Vec<(usize, usize)> = Vec::new(); // (başlangıç, uzunluk)
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

    println!("dosya           : {} ({:.1} sn, {} Hz)", a.wav.display(), ms(samples.len()) / 1000.0, rate);
    println!("incelenen bölge : {:.1} sn (baştan {} ms, sondan {} ms atlandı)", ms(region.len()) / 1000.0, a.skip_ms, a.tail_ms);
    println!("RMS             : {:.4}", rms);
    println!("≥5ms sessizlik  : {} adet (en uzunu {:.1} ms)", gaps.len(), ms(longest));
    println!("klik (ani atlama): {clicks}");
    for (start, n) in &bad {
        println!("  ❌ kesinti: {:.2} sn konumunda, {:.1} ms", (ms(*start) + a.skip_ms as f64) / 1000.0, ms(*n));
    }
    if bad.is_empty() && rms > 0.05 {
        println!("SONUÇ: TEMİZ ✅ ({:.0} ms üzeri kesinti yok)", a.gap_ms);
        Ok(())
    } else if rms <= 0.05 {
        println!("SONUÇ: BAŞARISIZ ❌ (kayıtta ses yok gibi görünüyor, RMS={rms:.4})");
        std::process::exit(1);
    } else {
        println!("SONUÇ: KESİNTİLİ ❌ ({} adet {:.0} ms üzeri boşluk)", bad.len(), a.gap_ms);
        std::process::exit(1);
    }
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Send(a) => cmd_send(a),
        Cmd::Analyze(a) => cmd_analyze(a),
    }
}
