//! arp-receiver — AudioRelayPlus PC alıcısı (komut satırı).
//!
//! Telefondan gelen ses akışını alır, adaptif jitter tamponundan geçirip
//! seçilen ses aygıtına verir. `--headless` modunda ses aygıtı yerine
//! duvar saati ile tüketir (test/CI için, `--dump` ile wav kaydı).
//! Pencereli sürüm için: arp-gui.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arp::engine::{self, Engine, EventLog, Shared};
use arp::protocol as proto;
use arp::resampler::Resampler;

#[derive(Parser, Debug)]
#[command(name = "arp-receiver", version, about = "AudioRelayPlus PC alıcısı")]
struct Args {
    /// Dinlenecek UDP portu
    #[arg(long, default_value_t = proto::DEFAULT_PORT)]
    port: u16,
    /// Çıkış aygıtı adı (alt dizgi eşleşmesi, örn. "cable" veya "pulse")
    #[arg(long)]
    device: Option<String>,
    /// Ses aygıtlarını listele ve çık
    #[arg(long)]
    list_devices: bool,
    /// Keşifte görünecek PC adı
    #[arg(long)]
    name: Option<String>,
    /// Başlangıç hedef gecikmesi (ms)
    #[arg(long, default_value_t = 80)]
    target_ms: u32,
    /// Ses aygıtı yok: duvar saatiyle tüket (test modu)
    #[arg(long)]
    headless: bool,
    /// Çalınan sesi wav dosyasına yaz (yalnızca --headless)
    #[arg(long)]
    dump: Option<PathBuf>,
    /// --headless süre (saniye, 0 = sınırsız)
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Periyodik istatistik satırını gizle
    #[arg(long)]
    quiet: bool,
    /// Çıkış kazancı (1.0 = dokunma; yumuşak sınırlayıcı ile)
    #[arg(long, default_value_t = 1.0)]
    gain: f32,
}

fn run_headless(
    shared: Shared,
    adj_ppm: Arc<AtomicI32>,
    dump: Option<PathBuf>,
    duration: u64,
    gain: f32,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut writer = match &dump {
        Some(p) => Some(hound::WavWriter::create(
            p,
            hound::WavSpec {
                channels: 1,
                sample_rate: proto::SAMPLE_RATE,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )?),
        None => None,
    };
    let mut rs = Resampler::new(proto::SAMPLE_RATE as f64, proto::SAMPLE_RATE as f64);
    let mut last_epoch = 0u64;
    let chunk = 480; // 10 ms
    let mut buf = vec![0f32; chunk];
    let started = Instant::now();
    let mut next = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if duration > 0 && started.elapsed() >= Duration::from_secs(duration) {
            break;
        }
        next += Duration::from_millis(10);
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        engine::pull_mono(&shared, &mut rs, &mut last_epoch, &adj_ppm, gain, &mut buf);
        if let Some(w) = writer.as_mut() {
            for &s in &buf {
                w.write_sample(s)?;
            }
        }
    }
    if let Some(w) = writer {
        w.finalize()?;
        println!("💾 kayıt tamamlandı: {}", dump.unwrap().display());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        println!("Çıkış aygıtları:");
        for name in engine::list_output_devices() {
            println!("  • {name}");
        }
        return Ok(());
    }

    let name = args.name.clone().unwrap_or_else(engine::default_name);
    let sock = std::net::UdpSocket::bind(("0.0.0.0", args.port))
        .with_context(|| format!("UDP {} portu açılamadı", args.port))?;
    println!("🎧 AudioRelayPlus alıcısı: UDP {} dinleniyor, ad: \"{}\"", args.port, name);

    let shared: Shared = Arc::new(Mutex::new(Engine::default()));
    let adj_ppm = Arc::new(AtomicI32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let log = EventLog::new(true); // CLI: her şey stdout'a

    engine::spawn_net(sock, shared.clone(), name, args.target_ms, log.clone());
    engine::spawn_supervisor(shared.clone(), adj_ppm.clone(), log.clone(), !args.quiet);

    if args.headless {
        run_headless(shared, adj_ppm, args.dump.clone(), args.duration, args.gain, stop)?;
    } else {
        let gain_bits = Arc::new(AtomicU32::new(args.gain.to_bits()));
        let (_stream, desc) = engine::run_audio(shared, adj_ppm, &args.device, gain_bits)?;
        println!("🔊 çıkış: {desc}");
        println!("hazır — telefondan bağlanabilirsiniz (çıkış: Ctrl-C)");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    Ok(())
}
