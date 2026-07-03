//! Yankı iptali (deneysel).
//!
//! Topoloji: arkadaşın sesi PC hoparlöründen çıkar → telefonun mikrofonu
//! duyar → akışla PC'ye geri gelir. Referans (hoparlörde çalan) ile mikrofon
//! sinyali AYNI makinede (PC'de) buluştuğu için iptal burada yapılır:
//! sistem çıkışının loopback kaydı referans alınır, telefondan gelen sinyalden
//! speex MDF ile çıkarılır. Kuyruk 240 ms — akustik + ağ + tampon gecikmesini
//! kapsar. Kesin çözümün kulaklık olduğunu unutmayın; bu "deneysel"dir.

use std::collections::VecDeque;
use std::os::raw::c_void;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

use crate::engine::EventLog;

const RATE: u32 = 48000;
const FRAME: usize = 480; // 10 ms
const TAIL: i32 = (FRAME as i32) * 24; // ~240 ms kuyruk

/// Loopback yakalayıcısının doldurduğu referans halkası (48 kHz mono i16).
pub type RefRing = Arc<Mutex<VecDeque<i16>>>;

const RING_CAP: usize = 48000; // 1 sn üst sınır

pub struct Aec {
    st: *mut aec_rs_sys::SpeexEchoState,
    pre: *mut aec_rs_sys::SpeexPreprocessState,
    reference: RefRing,
    mic_fifo: VecDeque<i16>,
    out_fifo: VecDeque<f32>,
    primed: bool,
}

// speex durumları tek iş parçacığından kullanılıyor; ham işaretçi Send engelini kaldır.
unsafe impl Send for Aec {}

impl Aec {
    pub fn new(reference: RefRing) -> Aec {
        unsafe {
            let st = aec_rs_sys::speex_echo_state_init(FRAME as i32, TAIL);
            let mut rate: i32 = RATE as i32;
            aec_rs_sys::speex_echo_ctl(
                st,
                aec_rs_sys::SPEEX_ECHO_SET_SAMPLING_RATE as i32,
                &mut rate as *mut _ as *mut c_void,
            );
            let pre = aec_rs_sys::speex_preprocess_state_init(FRAME as i32, RATE as i32);
            aec_rs_sys::speex_preprocess_ctl(
                pre,
                aec_rs_sys::SPEEX_PREPROCESS_SET_ECHO_STATE as i32,
                st as *mut c_void,
            );
            Aec { st, pre, reference, mic_fifo: VecDeque::new(), out_fifo: VecDeque::new(), primed: false }
        }
    }

    /// 48 kHz mono mikrofon örneklerini yerinde temizler (+10 ms blok gecikmesi).
    pub fn process(&mut self, buf: &mut [f32]) {
        for &s in buf.iter() {
            self.mic_fifo.push_back((s.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
        let mut mic = [0i16; FRAME];
        let mut echo = [0i16; FRAME];
        let mut out = [0i16; FRAME];
        while self.mic_fifo.len() >= FRAME {
            for m in mic.iter_mut() {
                *m = self.mic_fifo.pop_front().unwrap();
            }
            {
                let mut r = self.reference.lock().unwrap();
                // Başlangıçta 30 ms birikmeden tüketme (hiza için ufak pay)
                if !self.primed && r.len() >= FRAME * 3 {
                    self.primed = true;
                }
                if self.primed && r.len() >= FRAME {
                    for e in echo.iter_mut() {
                        *e = r.pop_front().unwrap();
                    }
                } else {
                    echo.fill(0);
                }
                // Halka aşırı doldu ise (üretici hızlı): hizayı yaklaştır
                while r.len() > RING_CAP / 2 {
                    r.pop_front();
                }
            }
            unsafe {
                aec_rs_sys::speex_echo_cancellation(self.st, mic.as_ptr(), echo.as_ptr(), out.as_mut_ptr());
                aec_rs_sys::speex_preprocess_run(self.pre, out.as_mut_ptr());
            }
            for &o in out.iter() {
                self.out_fifo.push_back(o as f32 / 32768.0);
            }
        }
        // Çıkışı yaz: ilk çağrılarda eksik kalan kısım sessizlik olur (blok gecikmesi)
        for s in buf.iter_mut() {
            *s = self.out_fifo.pop_front().unwrap_or(0.0);
        }
    }
}

impl Drop for Aec {
    fn drop(&mut self) {
        unsafe {
            aec_rs_sys::speex_preprocess_state_destroy(self.pre);
            aec_rs_sys::speex_echo_state_destroy(self.st);
        }
    }
}

/// Basit itme-tabanlı yeniden örnekleyici (loopback callback'i içinde kullanılır).
struct PushResampler {
    step: f64,
    pos: f64,
    prev: f32,
}

impl PushResampler {
    fn new(from: u32, to: u32) -> Self {
        PushResampler { step: to as f64 / from as f64, pos: 0.0, prev: 0.0 }
    }

    fn push(&mut self, x: f32, mut emit: impl FnMut(f32)) {
        // prev ile x arasında, çıkış ızgarasına düşen örnekleri üret
        self.pos += self.step;
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            let t = if self.step > 0.0 { 1.0 - (self.pos / self.step) } else { 0.0 };
            emit(self.prev + (x - self.prev) * t as f32);
        }
        self.prev = x;
    }
}

/// Sistem çıkışını (hoparlörde çalanı) yakalayıp referans halkasını doldurur.
/// Windows: WASAPI loopback (çıkış aygıtı giriş gibi açılır).
/// Linux: PULSE_SOURCE=<varsayılan sink>.monitor ile "pulse" giriş aygıtı.
pub fn spawn_loopback_reference(log: &Arc<EventLog>) -> Result<(cpal::Stream, RefRing)> {
    let ring: RefRing = Arc::new(Mutex::new(VecDeque::new()));
    let host = cpal::default_host();

    #[cfg(target_os = "linux")]
    let device = {
        let sink = std::process::Command::new("pactl")
            .args(["get-default-sink"])
            .output()
            .ok()
            .and_then(|o| if o.status.success() { Some(String::from_utf8_lossy(&o.stdout).trim().to_string()) } else { None })
            .ok_or_else(|| anyhow!("varsayılan ses çıkışı öğrenilemedi (pactl)"))?;
        std::env::set_var("PULSE_SOURCE", format!("{sink}.monitor"));
        host.input_devices()?
            .find(|d| d.name().map(|n| n.to_lowercase().contains("pulse")).unwrap_or(false))
            .ok_or_else(|| anyhow!("'pulse' giriş aygıtı yok (pipewire-pulse kurulu mu?)"))?
    };

    #[cfg(target_os = "windows")]
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("varsayılan çıkış aygıtı yok"))?;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("bu platformda loopback desteklenmiyor"))?;

    let config = device.default_input_config().context("loopback formatı alınamadı")?;
    let rate = config.sample_rate().0;
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_loopback::<f32>(&device, &config.clone().into(), rate, ring.clone())?,
        cpal::SampleFormat::I16 => build_loopback::<i16>(&device, &config.clone().into(), rate, ring.clone())?,
        cpal::SampleFormat::U16 => build_loopback::<u16>(&device, &config.clone().into(), rate, ring.clone())?,
        f => return Err(anyhow!("desteklenmeyen loopback formatı: {f:?}")),
    };
    stream.play()?;
    log.push(format!("🔁 yankı referansı: {} @ {} Hz", device.name().unwrap_or_else(|_| "?".into()), rate));
    Ok((stream, ring))
}

fn build_loopback<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rate: u32,
    ring: RefRing,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = (config.channels as usize).max(1);
    let mut rs = PushResampler::new(rate, RATE);
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut r = ring.lock().unwrap();
            for frame in data.chunks(channels) {
                let mono: f32 =
                    frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                rs.push(mono, |y| {
                    if r.len() < RING_CAP {
                        r.push_back((y.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                });
            }
        },
        |e| eprintln!("⚠ loopback hatası: {e}"),
        None,
    )?;
    Ok(stream)
}
