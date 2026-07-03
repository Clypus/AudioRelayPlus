//! Servo kontrollü lineer yeniden örnekleyici.
//!
//! İki işi birden yapar:
//! 1. Kaynak (48 kHz) ile ses aygıtının hızı farklıysa dönüştürür.
//! 2. `set_adj_ppm` ile tampon doluluğuna bağlı mikro hız ayarı uygular —
//!    telefon ve PC saatlerinin ppm farkını sürekli ve duyulmaz biçimde emer.

use std::collections::VecDeque;

pub struct Resampler {
    step_base: f64,
    adj: f64,
    /// carry ile fifo[0] arasındaki kesirli konum.
    frac: f64,
    /// Tüketilen son kaynak örneği (interpolasyon sol ucu).
    carry: f32,
    fifo: VecDeque<f32>,
    scratch: Vec<f32>,
}

impl Resampler {
    pub fn new(src_hz: f64, dst_hz: f64) -> Self {
        Resampler {
            step_base: src_hz / dst_hz,
            adj: 0.0,
            frac: 0.0,
            carry: 0.0,
            fifo: VecDeque::new(),
            scratch: Vec::new(),
        }
    }

    pub fn reset(&mut self) {
        self.frac = 0.0;
        self.carry = 0.0;
        self.fifo.clear();
    }

    /// Pozitif ppm = kaynağı hızlı tüket (tamponu erit), negatif = yavaş tüket.
    pub fn set_adj_ppm(&mut self, ppm: f64) {
        self.adj = (ppm * 1e-6).clamp(-0.005, 0.005);
    }

    /// `out`'u doldurur; eksik kaynak örneklerini `pull` ile ister.
    pub fn process(&mut self, out: &mut [f32], mut pull: impl FnMut(&mut [f32])) {
        if out.is_empty() {
            return;
        }
        let step = self.step_base * (1.0 + self.adj);
        let end = self.frac + step * out.len() as f64;
        let needed = end.ceil() as usize;
        if self.fifo.len() < needed {
            let deficit = needed - self.fifo.len();
            self.scratch.resize(deficit, 0.0);
            pull(&mut self.scratch);
            self.fifo.extend(self.scratch.iter().copied());
        }
        let mut p = self.frac;
        for o in out.iter_mut() {
            let i = p as usize;
            let a = if i == 0 { self.carry } else { self.fifo[i - 1] };
            let b = self.fifo[i];
            let t = (p - i as f64) as f32;
            *o = a + (b - a) * t;
            p += step;
        }
        let consumed = p as usize;
        if consumed > 0 {
            self.carry = self.fifo[consumed - 1];
            self.fifo.drain(..consumed);
        }
        self.frac = p - consumed as f64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp_source() -> impl FnMut(&mut [f32]) {
        let mut n = 0f32;
        move |buf: &mut [f32]| {
            for b in buf.iter_mut() {
                *b = n;
                n += 1.0;
            }
        }
    }

    #[test]
    fn identity_passthrough_is_delayed_copy() {
        let mut rs = Resampler::new(48000.0, 48000.0);
        let mut src = ramp_source();
        let mut out = vec![0f32; 100];
        rs.process(&mut out, &mut src);
        // Bir örneklik gecikmeyle birebir kopya: out[i] ≈ i-1 (out[0] = carry = 0)
        for (i, &v) in out.iter().enumerate().skip(1) {
            assert!((v - (i as f32 - 1.0)).abs() < 1e-4, "i={i} v={v}");
        }
    }

    #[test]
    fn chunked_equals_single_call() {
        let mut rs1 = Resampler::new(48000.0, 44100.0);
        let mut rs2 = Resampler::new(48000.0, 44100.0);
        let mut a = vec![0f32; 441];
        let mut b = vec![0f32; 441];
        rs1.process(&mut a, ramp_source());
        let mut src2 = ramp_source();
        for chunk in b.chunks_mut(37) {
            rs2.process(chunk, &mut src2);
        }
        for i in 0..a.len() {
            assert!((a[i] - b[i]).abs() < 1e-3, "i={i} {} != {}", a[i], b[i]);
        }
    }

    #[test]
    fn consumption_rate_follows_adj() {
        let mut rs = Resampler::new(48000.0, 48000.0);
        rs.set_adj_ppm(2000.0); // %0,2 hızlı tüketim
        let mut consumed = 0usize;
        let mut out = vec![0f32; 48000];
        rs.process(&mut out, |buf| {
            consumed += buf.len();
            for b in buf.iter_mut() {
                *b = 0.5;
            }
        });
        // 48000 çıkış için ~48096 kaynak tüketilmeli
        assert!((consumed as i64 - 48096).abs() < 4, "consumed={consumed}");
    }
}
