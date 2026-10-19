//! Microphone processing between the jitter buffer and the soundpad mix,
//! in 10 ms blocks: optional echo cancellation (speex, see `aec`) and
//! optional neural noise suppression (RNNoise via `nnnoiseless`).
//!
//! With both off this is a pass-through that adds no latency. With either on
//! it adds exactly one block (10 ms): the output fifo is primed with one block
//! of silence, so it can never run dry whatever buffer sizes the caller uses.

use std::collections::VecDeque;

use nnnoiseless::DenoiseState;

use crate::aec::Aec;

/// Block size shared by speex and RNNoise: 10 ms at 48 kHz.
pub const BLOCK: usize = 480;

pub struct MicDsp {
    aec: Option<Aec>,
    denoise: Option<Box<DenoiseState<'static>>>,
    fifo_in: VecDeque<f32>,
    fifo_out: VecDeque<f32>,
    primed: bool,
    block: [f32; BLOCK],
    scratch: [f32; BLOCK],
}

impl Default for MicDsp {
    fn default() -> Self {
        Self::new()
    }
}

impl MicDsp {
    pub fn new() -> MicDsp {
        MicDsp {
            aec: None,
            denoise: None,
            fifo_in: VecDeque::new(),
            fifo_out: VecDeque::new(),
            primed: false,
            block: [0.0; BLOCK],
            scratch: [0.0; BLOCK],
        }
    }

    pub fn set_aec(&mut self, aec: Option<Aec>) {
        self.aec = aec;
    }

    pub fn aec_on(&self) -> bool {
        self.aec.is_some()
    }

    pub fn set_denoise(&mut self, on: bool) {
        if on && self.denoise.is_none() {
            self.denoise = Some(DenoiseState::new());
        } else if !on {
            self.denoise = None;
        }
    }

    pub fn denoise_on(&self) -> bool {
        self.denoise.is_some()
    }

    /// Processes 48 kHz mono samples in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        if self.aec.is_none() && self.denoise.is_none() {
            if self.primed {
                self.fifo_in.clear();
                self.fifo_out.clear();
                self.primed = false;
            }
            return;
        }
        if !self.primed {
            self.fifo_out.extend(std::iter::repeat_n(0.0, BLOCK));
            self.primed = true;
        }
        self.fifo_in.extend(buf.iter().copied());
        while self.fifo_in.len() >= BLOCK {
            for (d, s) in self.block.iter_mut().zip(self.fifo_in.drain(..BLOCK)) {
                *d = s;
            }
            if let Some(a) = self.aec.as_mut() {
                a.process_block(&mut self.block);
            }
            if let Some(d) = self.denoise.as_mut() {
                // RNNoise expects samples on the i16 scale
                for x in self.block.iter_mut() {
                    *x *= 32768.0;
                }
                d.process_frame(&mut self.scratch, &self.block);
                for (b, &o) in self.block.iter_mut().zip(self.scratch.iter()) {
                    *b = (o / 32768.0).clamp(-1.0, 1.0);
                }
            }
            self.fifo_out.extend(self.block.iter().copied());
        }
        // fifo_in + fifo_out always hold exactly one block once primed, so
        // there are at least buf.len() samples waiting here.
        let n = buf.len();
        for (o, s) in buf.iter_mut().zip(self.fifo_out.drain(..n)) {
            *o = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_off() {
        let mut dsp = MicDsp::new();
        let mut buf: Vec<f32> = (0..300).map(|i| i as f32 / 1000.0).collect();
        let orig = buf.clone();
        dsp.process(&mut buf);
        assert_eq!(buf, orig);
    }

    #[test]
    fn constant_one_block_latency_with_odd_chunk_sizes() {
        let mut dsp = MicDsp::new();
        dsp.set_denoise(true);
        let mut n = 0usize;
        let mut total_out = 0usize;
        // Feed a loud tone in awkward chunk sizes: no panics, no missing output
        for &chunk in [7usize, 480, 1000, 33, 441, 960, 1].iter().cycle().take(200) {
            let mut buf: Vec<f32> = (0..chunk)
                .map(|i| ((n + i) as f32 * 0.05).sin() * 0.5)
                .collect();
            n += chunk;
            dsp.process(&mut buf);
            total_out += buf.len();
            assert!(buf.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        }
        assert_eq!(total_out, n);
        assert_eq!(dsp.fifo_in.len() + dsp.fifo_out.len(), BLOCK);
        // Switching off drops the block delay again
        dsp.set_denoise(false);
        let mut buf = vec![0.25f32; 100];
        dsp.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.25));
    }

    #[test]
    fn denoise_attenuates_steady_noise() {
        let mut dsp = MicDsp::new();
        dsp.set_denoise(true);
        // Low-passed noise from an LCG (fan/hum-like, like most real room
        // noise); RNNoise should squash nearly all of it
        let mut x = 12345u32;
        let mut lp = 0f32;
        let mut noise = || {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            lp = 0.9 * lp + 0.1 * ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5);
            lp * 3.0
        };
        let (mut e_in, mut e_out) = (0f64, 0f64);
        for i in 0..300 {
            let mut buf: Vec<f32> = (0..BLOCK).map(|_| noise() * 0.2).collect();
            let before: f64 = buf.iter().map(|&s| (s * s) as f64).sum();
            dsp.process(&mut buf);
            if i >= 100 {
                // let the network settle first
                e_in += before;
                e_out += buf.iter().map(|&s| (s * s) as f64).sum::<f64>();
            }
        }
        assert!(e_out < e_in * 0.1, "noise energy only went {e_in:.1} → {e_out:.1}");
    }
}
