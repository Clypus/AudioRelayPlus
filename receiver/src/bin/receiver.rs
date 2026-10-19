//! arp-receiver — AudioRelayPlus PC receiver (command line).
//!
//! Takes the stream coming from the phone, runs it through the adaptive
//! jitter buffer and plays it on the chosen sound device. With `--headless`
//! it consumes the audio on the wall clock instead of a sound device
//! (tests/CI; `--dump` records a wav). The windowed version is arp-gui.

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arp::engine::{self, AudioCtl, Engine, EventLog, NetCtx, OutputManager, Shared};
use arp::protocol as proto;
use arp::resampler::Resampler;
use arp::soundpad::Soundpad;
use arp::t;

#[derive(Parser, Debug)]
#[command(name = "arp-receiver", version, about = "AudioRelayPlus PC receiver")]
struct Args {
    /// Port to listen on (UDP = Wi-Fi and TCP = USB, both on this port)
    #[arg(long, default_value_t = proto::DEFAULT_PORT)]
    port: u16,
    /// Output device name (substring match, e.g. "cable" or "pulse")
    #[arg(long)]
    device: Option<String>,
    /// List the sound devices and exit
    #[arg(long)]
    list_devices: bool,
    /// PC name shown in the phone's list
    #[arg(long)]
    name: Option<String>,
    /// Initial target delay (ms)
    #[arg(long, default_value_t = 60)]
    target_ms: u32,
    /// No sound device: consume on the wall clock (test mode)
    #[arg(long)]
    headless: bool,
    /// Write what is played to a wav file (--headless only)
    #[arg(long)]
    dump: Option<PathBuf>,
    /// --headless duration in seconds (0 = unlimited)
    #[arg(long, default_value_t = 0)]
    duration: u64,
    /// Hide the periodic stats line
    #[arg(long)]
    quiet: bool,
    /// Output gain (1.0 = untouched; with a soft limiter)
    #[arg(long, default_value_t = 1.0)]
    gain: f32,
    /// AI noise suppression (RNNoise)
    #[arg(long)]
    denoise: bool,
    /// Echo cancellation (experimental): the speaker loopback is the reference
    #[arg(long)]
    aec: bool,
    /// Soundpad folder (mp3/ogg/wav/flac), watched for changes
    #[arg(long, default_value = "soundpad")]
    soundpad: PathBuf,
    /// Plug-and-play USB: keep an `adb reverse` tunnel on attached phones
    #[arg(long)]
    auto_usb: bool,
}

fn run_headless(
    shared: Shared,
    adj_ppm: Arc<AtomicI32>,
    ctl: AudioCtl,
    dump: Option<PathBuf>,
    duration: u64,
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
        engine::pull_mono(&shared, &mut rs, &mut last_epoch, &adj_ppm, &ctl, &mut buf);
        if let Some(w) = writer.as_mut() {
            for &s in &buf {
                w.write_sample(s)?;
            }
        }
    }
    if let Some(w) = writer {
        w.finalize()?;
        let path = dump.unwrap();
        let path = path.display();
        println!("{}", t!("💾 recording saved: {path}", "💾 kayıt tamamlandı: {path}"));
    }
    // A session still running at the end gets its summary too
    if let Some(s) = shared.lock().unwrap().session.as_ref() {
        println!("{}", s.summary());
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_devices {
        println!("{}", t!("Output devices:", "Çıkış aygıtları:"));
        for name in engine::list_output_devices() {
            println!("  • {name}");
        }
        return Ok(());
    }

    let name = args.name.clone().unwrap_or_else(engine::default_name);
    let port = args.port;
    let sock = std::net::UdpSocket::bind(("0.0.0.0", port))
        .with_context(|| t!("couldn't open UDP port {port}", "UDP {port} portu açılamadı"))?;
    println!(
        "{}",
        t!(
            "🎧 AudioRelayPlus receiver: listening on UDP+TCP {port}, name: \"{name}\"",
            "🎧 AudioRelayPlus alıcısı: UDP+TCP {port} dinleniyor, ad: \"{name}\""
        )
    );

    let shared: Shared = Arc::new(Mutex::new(Engine::default()));
    let adj_ppm = Arc::new(AtomicI32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let log = EventLog::new(true); // CLI: everything goes to stdout

    let pad = Arc::new(Soundpad::load_dir(&args.soundpad, &log));
    pad.spawn_watcher(log.clone());
    let ctl = AudioCtl::new(pad.clone());
    ctl.set_gain(args.gain);
    if args.denoise {
        ctl.dsp.lock().unwrap().set_denoise(true);
        println!("{}", t!("✨ noise suppression on (RNNoise)", "✨ gürültü engelleme açık (RNNoise)"));
    }

    // Echo cancellation: active while the loopback reference stream lives
    let _loopback = if args.aec {
        match arp::aec::spawn_loopback_reference(&log) {
            Ok((stream, ring)) => {
                ctl.dsp.lock().unwrap().set_aec(Some(arp::aec::Aec::new(ring)));
                println!("{}", t!("🔇 echo cancellation on (experimental)", "🔇 yankı engelleme açık (deneysel)"));
                Some(stream)
            }
            Err(e) => {
                eprintln!("{}", t!("⚠ couldn't start echo cancellation: {e}", "⚠ yankı engelleme açılamadı: {e}"));
                None
            }
        }
    } else {
        None
    };

    let ctx = NetCtx::new(shared.clone(), name, port, args.target_ms, log.clone(), pad.clone());
    engine::spawn_net(sock, ctx.clone());
    if let Err(e) = engine::spawn_tcp(ctx) {
        eprintln!("{}", t!("⚠ couldn't start the USB/TCP listener: {e}", "⚠ USB/TCP dinleyicisi açılamadı: {e}"));
    }
    engine::spawn_supervisor(shared.clone(), adj_ppm.clone(), log.clone(), pad.clone(), !args.quiet);
    let _usb = args.auto_usb.then(|| arp::usb::spawn(port, true, log.clone()));

    if args.headless {
        run_headless(shared, adj_ppm, ctl, args.dump.clone(), args.duration, stop)?;
    } else {
        let output = OutputManager::spawn(t!("output", "çıkış"), engine::mic_render(shared, adj_ppm, ctl), log);
        output.open(args.device.clone());
        println!("{}", t!("ready — connect from the phone (quit: Ctrl-C)", "hazır — telefondan bağlanabilirsiniz (çıkış: Ctrl-C)"));
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    Ok(())
}
