//! Soundpad: decodes the audio files of a folder (mp3/ogg/wav/flac) into
//! memory and mixes them into the microphone stream when a GUI button or the
//! phone asks for it.
//!
//! The folder is watched: drop a file in (or delete one) and the list updates
//! within a couple of seconds — on the PC and on the phone. Sounds can also be
//! played on the local speakers ("monitor") so you hear what you trigger.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::engine::EventLog;
use crate::t;

const RATE: u32 = 48000;
/// Per-file limit (60 s) — protection against memory bombs.
const MAX_SAMPLES: usize = (RATE as usize) * 60;
pub const MAX_SOUNDS: usize = 32;
/// At most this many sounds play at the same time; the oldest one gives way.
const MAX_VOICES: usize = 4;
pub const EXTENSIONS: [&str; 4] = ["mp3", "ogg", "wav", "flac"];

/// What a file looked like when it was decoded (to skip re-decoding it).
#[derive(Clone, PartialEq, Debug)]
struct Stamp {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

struct Sound {
    name: String,
    stamp: Stamp,
    /// 48 kHz mono, i16 to halve the memory of a full pad.
    data: Arc<Vec<i16>>,
}

struct Voice {
    data: Arc<Vec<i16>>,
    pos: usize,
}

pub struct Soundpad {
    dir: PathBuf,
    sounds: RwLock<Vec<Sound>>,
    /// Voices mixed into the microphone stream.
    active: Mutex<Vec<Voice>>,
    /// The same voices for the local speakers (only filled while monitoring).
    monitor: Mutex<Vec<Voice>>,
    monitor_on: AtomicBool,
    volume_bits: AtomicU32,
    /// Bumped on every (re)load, so listeners can tell the list changed.
    version: AtomicU64,
    /// The folder as of the last refresh (incl. files that failed to decode,
    /// so a broken file isn't retried — and reported — every 2 s).
    last_scan: Mutex<Option<Vec<Stamp>>>,
}

impl Soundpad {
    /// A pad for `dir` (which may not exist yet). Call `refresh` to load it.
    pub fn new(dir: PathBuf) -> Soundpad {
        Soundpad {
            dir,
            sounds: RwLock::new(Vec::new()),
            active: Mutex::new(Vec::new()),
            monitor: Mutex::new(Vec::new()),
            monitor_on: AtomicBool::new(false),
            volume_bits: AtomicU32::new(0.9f32.to_bits()),
            version: AtomicU64::new(0),
            last_scan: Mutex::new(None),
        }
    }

    pub fn empty() -> Soundpad {
        Soundpad::new(PathBuf::new())
    }

    /// Creates a pad and loads `dir` right away.
    pub fn load_dir(dir: &Path, log: &EventLog) -> Soundpad {
        let pad = Soundpad::new(dir.to_path_buf());
        pad.refresh(log);
        pad
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Supported files in the folder, sorted by name, capped at MAX_SOUNDS.
    fn scan(&self) -> Vec<Stamp> {
        if self.dir.as_os_str().is_empty() {
            return Vec::new();
        }
        let mut stamps: Vec<Stamp> = match std::fs::read_dir(&self.dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let ext = e.path().extension().and_then(|x| x.to_str()).map(|x| x.to_lowercase());
                    matches!(ext.as_deref(), Some(x) if EXTENSIONS.contains(&x))
                })
                .filter_map(|e| {
                    let meta = e.metadata().ok()?;
                    meta.is_file().then(|| Stamp { path: e.path(), len: meta.len(), modified: meta.modified().ok() })
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        stamps.sort_by(|a, b| a.path.cmp(&b.path));
        stamps.truncate(MAX_SOUNDS);
        stamps
    }

    /// Rescans the folder; decodes new or changed files and swaps the list in.
    /// Returns true if the list changed.
    pub fn refresh(&self, log: &EventLog) -> bool {
        let stamps = self.scan();
        {
            let mut last = self.last_scan.lock().unwrap();
            if last.as_ref() == Some(&stamps) {
                return false;
            }
            *last = Some(stamps.clone());
        }
        let mut next = Vec::with_capacity(stamps.len());
        for st in stamps {
            let name = st.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            // Unchanged file: reuse the decoded audio
            let reuse = self.sounds.read().unwrap().iter().find(|s| s.stamp == st).map(|s| s.data.clone());
            let data = match reuse {
                Some(d) => d,
                None => match decode_file(&st.path) {
                    Ok(d) if !d.is_empty() => Arc::new(d),
                    Ok(_) => {
                        log.push(t!("⚠ soundpad: {name} is empty, skipped", "⚠ soundpad: {name} boş, atlandı"));
                        continue;
                    }
                    Err(e) => {
                        log.push(t!("⚠ soundpad: couldn't decode {name}: {e}", "⚠ soundpad: {name} çözülemedi: {e}"));
                        continue;
                    }
                },
            };
            next.push(Sound { name, stamp: st, data });
        }
        let count = next.len();
        let had = std::mem::replace(&mut *self.sounds.write().unwrap(), next).len();
        self.version.fetch_add(1, Ordering::Relaxed);
        if count > 0 || had > 0 {
            let dir = self.dir.display();
            log.push(t!("🔔 soundpad: {count} sound(s) loaded ({dir})", "🔔 soundpad: {count} ses yüklendi ({dir})"));
        }
        true
    }

    /// Keeps the pad in sync with its folder (checks every 2 s).
    pub fn spawn_watcher(self: &Arc<Self>, log: Arc<EventLog>) {
        let pad = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            match pad.upgrade() {
                Some(p) => {
                    p.refresh(&log);
                }
                None => return,
            }
        });
    }

    pub fn names(&self) -> Vec<String> {
        self.sounds.read().unwrap().iter().map(|s| s.name.clone()).collect()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn is_empty(&self) -> bool {
        self.sounds.read().unwrap().is_empty()
    }

    pub fn play(&self, id: usize) {
        let data = match self.sounds.read().unwrap().get(id) {
            Some(s) => s.data.clone(),
            None => return,
        };
        let start = |list: &Mutex<Vec<Voice>>| {
            let mut v = list.lock().unwrap();
            if v.len() >= MAX_VOICES {
                v.remove(0);
            }
            v.push(Voice { data: data.clone(), pos: 0 });
        };
        start(&self.active);
        if self.monitor_on.load(Ordering::Relaxed) {
            start(&self.monitor);
        }
    }

    pub fn stop_all(&self) {
        self.active.lock().unwrap().clear();
        self.monitor.lock().unwrap().clear();
    }

    pub fn playing(&self) -> bool {
        !self.active.lock().unwrap().is_empty()
    }

    pub fn set_volume(&self, v: f32) {
        self.volume_bits.store(v.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume_bits.load(Ordering::Relaxed))
    }

    /// Also play triggered sounds on the local speakers.
    pub fn set_monitor(&self, on: bool) {
        self.monitor_on.store(on, Ordering::Relaxed);
        if !on {
            self.monitor.lock().unwrap().clear();
        }
    }

    pub fn monitor(&self) -> bool {
        self.monitor_on.load(Ordering::Relaxed)
    }

    /// Mixes into a 48 kHz mono buffer of the microphone stream.
    pub fn mix_into(&self, out: &mut [f32]) {
        mix_voices(&self.active, self.volume(), out);
    }

    /// Mixes the monitor copy into a 48 kHz mono buffer for the speakers.
    pub fn mix_monitor_into(&self, out: &mut [f32]) {
        mix_voices(&self.monitor, self.volume(), out);
    }
}

fn mix_voices(list: &Mutex<Vec<Voice>>, volume: f32, out: &mut [f32]) {
    let mut act = list.lock().unwrap();
    if act.is_empty() {
        return;
    }
    let scale = volume / 32768.0;
    for v in act.iter_mut() {
        let n = out.len().min(v.data.len() - v.pos);
        for (o, &s) in out[..n].iter_mut().zip(&v.data[v.pos..v.pos + n]) {
            *o += s as f32 * scale;
        }
        v.pos += n;
    }
    act.retain(|v| v.pos < v.data.len());
    // Soft limiter: overlapping sounds saturate smoothly instead of clipping
    for s in out.iter_mut() {
        let x = s.clamp(-1.5, 1.5);
        *s = x - x * x * x / 6.75;
    }
}

/// Decodes a file into 48 kHz mono i16.
fn decode_file(path: &Path) -> Result<Vec<i16>> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!(t!("unknown format: {e}", "format tanınmadı: {e}")))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!(t!("no audio track", "ses izi yok")))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!(t!("no decoder: {e}", "çözücü açılamadı: {e}")))?;

    let mut mono: Vec<f32> = Vec::new();
    let mut rate = track.codec_params.sample_rate.unwrap_or(RATE);
    // Stop decoding a little past the limit (it is applied after resampling)
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio) => {
                let spec = *audio.spec();
                rate = spec.rate;
                let ch = spec.channels.count().max(1);
                let mut sbuf = SampleBuffer::<f32>::new(audio.capacity() as u64, spec);
                sbuf.copy_interleaved_ref(audio);
                for frame in sbuf.samples().chunks(ch) {
                    mono.push(frame.iter().sum::<f32>() / ch as f32);
                }
                if mono.len() > MAX_SAMPLES * 2 {
                    break;
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }
    let mut out = if rate == RATE { mono } else { linear_resample(&mono, rate, RATE) };
    out.truncate(MAX_SAMPLES);
    Ok(out.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16).collect())
}

fn linear_resample(src: &[f32], from: u32, to: u32) -> Vec<f32> {
    if src.is_empty() || from == 0 {
        return Vec::new();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((src.len() as f64) / ratio) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let p = i as f64 * ratio;
        let idx = p as usize;
        let t = (p - idx as f64) as f32;
        let a = src[idx.min(src.len() - 1)];
        let b = src[(idx + 1).min(src.len() - 1)];
        out.push(a + (b - a) * t);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EventLog;

    fn write_tone(path: &Path, seconds: f32) {
        // 1 kHz sine, 44100 Hz stereo → must decode to 48k mono
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for i in 0..(44100.0 * seconds) as usize {
            let v = ((i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 44100.0).sin() * 12000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("arp-pad-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_wav_and_mixes() {
        let dir = temp_dir("mix");
        write_tone(&dir.join("test-sound.wav"), 0.5);

        let log = EventLog::new(false);
        let pad = Soundpad::load_dir(&dir, &log);
        assert_eq!(pad.names(), vec!["test-sound".to_string()]);

        let mut out = vec![0f32; 960];
        pad.mix_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "nothing may be mixed before playing");

        pad.play(0);
        assert!(pad.playing());
        let mut heard = 0f32;
        for _ in 0..30 {
            out.fill(0.0);
            pad.mix_into(&mut out);
            heard = heard.max(out.iter().fold(0f32, |m, &s| m.max(s.abs())));
        }
        assert!(heard > 0.1, "the sound should be audible, peak={heard}");

        pad.stop_all();
        assert!(!pad.playing());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn refresh_picks_up_changes_and_volume_scales() {
        let dir = temp_dir("refresh");
        let log = EventLog::new(false);
        let pad = Soundpad::load_dir(&dir, &log);
        assert!(pad.is_empty());
        let v0 = pad.version();
        assert!(!pad.refresh(&log), "nothing changed yet");

        write_tone(&dir.join("b.wav"), 0.1);
        write_tone(&dir.join("a.wav"), 0.1);
        assert!(pad.refresh(&log));
        assert_eq!(pad.names(), vec!["a".to_string(), "b".to_string()]);
        assert!(pad.version() > v0);

        std::fs::remove_file(dir.join("a.wav")).unwrap();
        assert!(pad.refresh(&log));
        assert_eq!(pad.names(), vec!["b".to_string()]);

        // Volume 0 → silence; monitor only gets voices while enabled
        pad.set_volume(0.0);
        pad.play(0);
        let mut out = vec![0f32; 480];
        pad.mix_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        let mut mon = vec![0f32; 480];
        pad.mix_monitor_into(&mut mon);
        assert!(mon.iter().all(|&s| s == 0.0));
        pad.set_volume(1.0);
        pad.set_monitor(true);
        pad.play(0);
        pad.mix_monitor_into(&mut mon);
        assert!(mon.iter().any(|&s| s.abs() > 0.05), "monitor should hear it");
        std::fs::remove_dir_all(&dir).ok();
    }
}
