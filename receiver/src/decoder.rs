//! Codec uygulamaları: Opus (FEC + PLC ile) ve düz PCM16.

use crate::jitter::FrameDecoder;
use anyhow::Result;

pub struct OpusDec {
    dec: opus::Decoder,
}

impl OpusDec {
    pub fn new() -> Result<Self> {
        Ok(OpusDec { dec: opus::Decoder::new(48000, opus::Channels::Mono)? })
    }
}

impl FrameDecoder for OpusDec {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) {
        if self.dec.decode_float(payload, out, false).is_err() {
            out.fill(0.0);
        }
    }

    fn decode_fec(&mut self, next_payload: &[u8], out: &mut [f32]) {
        // fec=true: bir SONRAKİ paketin içindeki LBRR verisinden kayıp çerçeveyi üretir.
        if self.dec.decode_float(next_payload, out, true).is_err() {
            // FEC verisi yoksa libopus zaten PLC üretir; yine de hata olursa PLC dene.
            if self.dec.decode_float(&[], out, false).is_err() {
                out.fill(0.0);
            }
        }
    }

    fn conceal(&mut self, out: &mut [f32]) {
        // Boş girdi = paket kaybı → libopus PLC sentezi.
        if self.dec.decode_float(&[], out, false).is_err() {
            out.fill(0.0);
        }
    }
}

/// PCM16'da FEC yok; gizleme, son çerçevenin sönümlenerek tekrarı.
pub struct PcmDec {
    last: Vec<f32>,
}

impl PcmDec {
    pub fn new() -> Self {
        PcmDec { last: Vec::new() }
    }
}

impl Default for PcmDec {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameDecoder for PcmDec {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) {
        let n = (payload.len() / 2).min(out.len());
        for i in 0..n {
            let s = i16::from_le_bytes([payload[2 * i], payload[2 * i + 1]]);
            out[i] = s as f32 / 32768.0;
        }
        out[n..].fill(0.0);
        self.last.clear();
        self.last.extend_from_slice(out);
    }

    fn decode_fec(&mut self, _next_payload: &[u8], out: &mut [f32]) {
        self.conceal(out);
    }

    fn conceal(&mut self, out: &mut [f32]) {
        if self.last.len() == out.len() {
            for (o, &l) in out.iter_mut().zip(self.last.iter()) {
                *o = l * 0.5;
            }
            let snapshot: Vec<f32> = out.to_vec();
            self.last.copy_from_slice(&snapshot);
        } else {
            out.fill(0.0);
        }
    }
}
