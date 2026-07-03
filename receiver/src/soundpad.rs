//! Soundpad: bir klasördeki ses dosyalarını (mp3/ogg/wav/flac) belleğe çözer,
//! GUI düğmesinden ya da telefondan gelen komutla mikrofon akışına karıştırır.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::engine::EventLog;

const RATE: u32 = 48000;
/// Tek dosya için üst sınır (60 sn) — bellek bombasına karşı.
const MAX_SAMPLES: usize = (RATE as usize) * 60;
pub const MAX_SOUNDS: usize = 32;

struct Sound {
    name: String,
    data: Arc<Vec<f32>>,
}

struct Voice {
    data: Arc<Vec<f32>>,
    pos: usize,
}

pub struct Soundpad {
    dir: PathBuf,
    sounds: Vec<Sound>,
    active: Mutex<Vec<Voice>>,
}

impl Soundpad {
    pub fn empty() -> Soundpad {
        Soundpad { dir: PathBuf::new(), sounds: Vec::new(), active: Mutex::new(Vec::new()) }
    }

    /// Klasördeki desteklenen dosyaları ada göre sıralı yükler.
    pub fn load_dir(dir: &Path, log: &EventLog) -> Soundpad {
        let mut sounds = Vec::new();
        let mut entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    matches!(
                        p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref(),
                        Some("mp3") | Some("ogg") | Some("wav") | Some("flac")
                    )
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        entries.sort();
        for p in entries.into_iter().take(MAX_SOUNDS) {
            let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            match decode_file(&p) {
                Ok(data) if !data.is_empty() => {
                    sounds.push(Sound { name: name.clone(), data: Arc::new(data) });
                }
                Ok(_) => log.push(format!("⚠ soundpad: {name} boş, atlandı")),
                Err(e) => log.push(format!("⚠ soundpad: {name} çözülemedi: {e}")),
            }
        }
        if !sounds.is_empty() {
            log.push(format!("🔔 soundpad: {} ses yüklendi ({})", sounds.len(), dir.display()));
        }
        Soundpad { dir: dir.to_path_buf(), sounds, active: Mutex::new(Vec::new()) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn names(&self) -> Vec<String> {
        self.sounds.iter().map(|s| s.name.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.sounds.is_empty()
    }

    pub fn play(&self, id: usize) {
        if let Some(s) = self.sounds.get(id) {
            let mut act = self.active.lock().unwrap();
            // Aynı anda en fazla 4 ses; doluysa en eskisi düşer.
            if act.len() >= 4 {
                act.remove(0);
            }
            act.push(Voice { data: s.data.clone(), pos: 0 });
        }
    }

    pub fn stop_all(&self) {
        self.active.lock().unwrap().clear();
    }

    pub fn playing(&self) -> bool {
        !self.active.lock().unwrap().is_empty()
    }

    /// 48 kHz mono tampona karıştırır (yumuşak sınırlayıcıyla).
    pub fn mix_into(&self, out: &mut [f32]) {
        let mut act = self.active.lock().unwrap();
        if act.is_empty() {
            return;
        }
        for v in act.iter_mut() {
            let n = out.len().min(v.data.len() - v.pos);
            for i in 0..n {
                out[i] += v.data[v.pos + i] * 0.9;
            }
            v.pos += n;
        }
        act.retain(|v| v.pos < v.data.len());
        for s in out.iter_mut() {
            let x = s.clamp(-1.5, 1.5);
            *s = x - x * x * x / 6.75;
        }
    }
}

/// Dosyayı 48 kHz mono f32'ye çözer.
fn decode_file(path: &Path) -> Result<Vec<f32>> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow!("format tanınmadı: {e}"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("ses izi yok"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| anyhow!("çözücü açılamadı: {e}"))?;

    let mut mono: Vec<f32> = Vec::new();
    let mut rate = track.codec_params.sample_rate.unwrap_or(RATE);
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(_) => break, // dosya sonu
        };
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
                    break; // kaba sınır (yeniden örneklemeden önce)
                }
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }
    let mut out = if rate == RATE { mono } else { linear_resample(&mono, rate, RATE) };
    out.truncate(MAX_SAMPLES);
    Ok(out)
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

pub type SharedPad = Arc<Soundpad>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::EventLog;

    #[test]
    fn loads_wav_and_mixes() {
        let dir = std::env::temp_dir().join(format!("arp-pad-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // 0,5 sn 1 kHz sinüs, 44100 Hz stereo → çözümde 48k monoya inmeli
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(dir.join("test-sesi.wav"), spec).unwrap();
        for i in 0..22050 {
            let v = ((i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 44100.0).sin() * 12000.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();

        let log = EventLog::new(false);
        let pad = Soundpad::load_dir(&dir, &log);
        assert_eq!(pad.names(), vec!["test-sesi".to_string()]);

        let mut out = vec![0f32; 960];
        pad.mix_into(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "çalınmadan ses karışmamalı");

        pad.play(0);
        assert!(pad.playing());
        let mut heard = 0f32;
        for _ in 0..30 {
            out.fill(0.0);
            pad.mix_into(&mut out);
            heard = heard.max(out.iter().fold(0f32, |m, &s| m.max(s.abs())));
        }
        assert!(heard > 0.1, "ses duyulmalıydı, tepe={heard}");

        pad.stop_all();
        assert!(!pad.playing());
        std::fs::remove_dir_all(&dir).ok();
    }
}
