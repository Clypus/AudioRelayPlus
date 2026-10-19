//! Echo cancellation (experimental).
//!
//! Topology: your friend's voice comes out of the PC speakers → the phone's
//! microphone hears it → it streams back to the PC. The reference (what the
//! speakers play) and the microphone signal meet on the SAME machine (the PC),
//! so cancellation happens here: a loopback capture of the system output is the
//! reference, and speex MDF subtracts it from the phone's signal. The 240 ms
//! tail covers acoustic + network + buffer delay. Headphones remain the real
//! fix; this is "experimental".

use std::collections::VecDeque;
use std::os::raw::c_void;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

use crate::dsp::BLOCK;
use crate::engine::EventLog;
use crate::t;

const RATE: u32 = 48000;
const TAIL: i32 = (BLOCK as i32) * 24; // ~240 ms tail

/// Reference ring filled by the loopback capture (48 kHz mono i16).
pub type RefRing = Arc<Mutex<VecDeque<i16>>>;

const RING_CAP: usize = 48000; // 1 s upper bound

pub struct Aec {
    st: *mut aec_rs_sys::SpeexEchoState,
    pre: *mut aec_rs_sys::SpeexPreprocessState,
    reference: RefRing,
    primed: bool,
}

// The speex states are only ever used from one thread at a time; the raw
// pointers just keep the compiler from seeing that.
unsafe impl Send for Aec {}

impl Aec {
    pub fn new(reference: RefRing) -> Aec {
        unsafe {
            let st = aec_rs_sys::speex_echo_state_init(BLOCK as i32, TAIL);
            let mut rate: i32 = RATE as i32;
            aec_rs_sys::speex_echo_ctl(
                st,
                aec_rs_sys::SPEEX_ECHO_SET_SAMPLING_RATE as i32,
                &mut rate as *mut _ as *mut c_void,
            );
            let pre = aec_rs_sys::speex_preprocess_state_init(BLOCK as i32, RATE as i32);
            aec_rs_sys::speex_preprocess_ctl(
                pre,
                aec_rs_sys::SPEEX_PREPROCESS_SET_ECHO_STATE as i32,
                st as *mut c_void,
            );
            Aec { st, pre, reference, primed: false }
        }
    }

    /// Cleans one 10 ms block of 48 kHz mono microphone audio in place.
    pub fn process_block(&mut self, block: &mut [f32; BLOCK]) {
        let mut mic = [0i16; BLOCK];
        let mut echo = [0i16; BLOCK];
        let mut out = [0i16; BLOCK];
        for (m, &s) in mic.iter_mut().zip(block.iter()) {
            *m = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        }
        {
            let mut r = self.reference.lock().unwrap();
            // At the start, wait for 30 ms of reference (a little slack for alignment)
            if !self.primed && r.len() >= BLOCK * 3 {
                self.primed = true;
            }
            if self.primed && r.len() >= BLOCK {
                for (e, s) in echo.iter_mut().zip(r.drain(..BLOCK)) {
                    *e = s;
                }
            }
            // Ring overfilled (producer running fast): pull the alignment back in
            while r.len() > RING_CAP / 2 {
                r.pop_front();
            }
        }
        unsafe {
            aec_rs_sys::speex_echo_cancellation(self.st, mic.as_ptr(), echo.as_ptr(), out.as_mut_ptr());
            aec_rs_sys::speex_preprocess_run(self.pre, out.as_mut_ptr());
        }
        for (b, &o) in block.iter_mut().zip(out.iter()) {
            *b = o as f32 / 32768.0;
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

/// Simple push-based resampler (used inside the loopback callback).
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
        // Emit the output-grid samples that fall between prev and x
        self.pos += self.step;
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            let t = if self.step > 0.0 { 1.0 - (self.pos / self.step) } else { 0.0 };
            emit(self.prev + (x - self.prev) * t as f32);
        }
        self.prev = x;
    }
}

/// Captures the system output (what the speakers play) into the reference ring.
/// Windows: WASAPI loopback (the output device opened as an input).
/// Linux: the "pulse" input device with PULSE_SOURCE=<default sink>.monitor.
pub fn spawn_loopback_reference(log: &Arc<EventLog>) -> Result<(cpal::Stream, RefRing)> {
    let ring: RefRing = Arc::new(Mutex::new(VecDeque::new()));
    let host = cpal::default_host();

    #[cfg(target_os = "linux")]
    let device = {
        let sink = crate::platform::quiet_command("pactl")
            .args(["get-default-sink"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .ok_or_else(|| anyhow!(t!("couldn't find the default sound output (pactl)", "varsayılan ses çıkışı öğrenilemedi (pactl)")))?;
        std::env::set_var("PULSE_SOURCE", format!("{sink}.monitor"));
        host.input_devices()?
            .find(|d| d.name().map(|n| n.to_lowercase().contains("pulse")).unwrap_or(false))
            .ok_or_else(|| {
                anyhow!(t!(
                    "no 'pulse' input device (is pipewire-pulse installed?)",
                    "'pulse' giriş aygıtı yok (pipewire-pulse kurulu mu?)"
                ))
            })?
    };

    #[cfg(target_os = "windows")]
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!(t!("no default output device", "varsayılan çıkış aygıtı yok")))?;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!(t!("loopback isn't supported on this platform", "bu platformda loopback desteklenmiyor")))?;

    let config = device
        .default_input_config()
        .context(t!("couldn't get the loopback format", "loopback formatı alınamadı"))?;
    let rate = config.sample_rate().0;
    let errlog = log.clone();
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => build_loopback::<f32>(&device, &config.clone().into(), rate, ring.clone(), errlog)?,
        cpal::SampleFormat::I16 => build_loopback::<i16>(&device, &config.clone().into(), rate, ring.clone(), errlog)?,
        cpal::SampleFormat::U16 => build_loopback::<u16>(&device, &config.clone().into(), rate, ring.clone(), errlog)?,
        f => return Err(anyhow!(t!("unsupported loopback format: {f:?}", "desteklenmeyen loopback formatı: {f:?}"))),
    };
    stream.play()?;
    let name = device.name().unwrap_or_else(|_| "?".into());
    log.push(t!("🔁 echo reference: {name} @ {rate} Hz", "🔁 yankı referansı: {name} @ {rate} Hz"));
    Ok((stream, ring))
}

fn build_loopback<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    rate: u32,
    ring: RefRing,
    log: Arc<EventLog>,
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
                let mono: f32 = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                rs.push(mono, |y| {
                    if r.len() < RING_CAP {
                        r.push_back((y.clamp(-1.0, 1.0) * 32767.0) as i16);
                    }
                });
            }
        },
        move |e| log.push(t!("⚠ echo reference error: {e}", "⚠ yankı referansı hatası: {e}")),
        None,
    )?;
    Ok(stream)
}
