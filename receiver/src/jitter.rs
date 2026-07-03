//! Adaptif jitter tamponu — "kesik kesik ses" sorununun çözüldüğü yer.
//!
//! Zaman tabanı deterministiktir: her şey "çekilen örnek sayısı" üzerinden
//! yürür, duvar saati kullanılmaz. Böylece birim testlerde ağ koşulları
//! birebir simüle edilebilir.

use std::collections::{BTreeMap, VecDeque};

/// Bir çerçeveyi PCM'e çözen soyutlama (Opus veya düz PCM16).
pub trait FrameDecoder {
    /// Normal çözüm. `out` tam bir çerçeve uzunluğundadır.
    fn decode(&mut self, payload: &[u8], out: &mut [f32]);
    /// Kayıp çerçeveyi, onu TAKİP EDEN paketin in-band FEC verisinden kurtar.
    fn decode_fec(&mut self, next_payload: &[u8], out: &mut [f32]);
    /// Paket kaybı gizleme (PLC): elde hiçbir veri yokken çerçeve sentezle.
    fn conceal(&mut self, out: &mut [f32]);
}

#[derive(Debug, Clone)]
pub struct JitterConfig {
    pub sample_rate: u32,
    pub frame_samples: usize,
    pub start_target_ms: u32,
    pub min_target_ms: u32,
    pub max_target_ms: u32,
    /// Üst üste en fazla bu kadar çerçeve PLC ile doldurulur, sonra yeniden tamponlanır.
    pub plc_limit_frames: u32,
    /// Bu boyuta kadar boşluklar PLC ile geçilir; daha büyükse ileri sarılır.
    pub small_gap_frames: u64,
    /// Bu kadar saniye sorunsuz oynatmadan sonra hedef gecikme azaltılır.
    pub decrease_after_s: u32,
    /// Tampon bu sınırı aşarsa en eski veri atlanır (taşma koruması).
    pub hard_cap_ms: u32,
    /// Playing durumundayken bundan büyük seq sıçraması oturum resetine yorulur.
    pub resync_jump_frames: u64,
}

impl Default for JitterConfig {
    fn default() -> Self {
        JitterConfig {
            sample_rate: 48000,
            frame_samples: 960,
            // "Ani gelsin": USB/temiz ağda tampon 30 ms'e kadar iner;
            // dalgalı Wi-Fi'da AIMD yine yukarı taşır.
            start_target_ms: 60,
            min_target_ms: 30,
            max_target_ms: 300,
            plc_limit_frames: 6,
            small_gap_frames: 3,
            decrease_after_s: 10,
            hard_cap_ms: 400,
            resync_jump_frames: 500,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub pushed: u64,
    pub late: u64,
    pub fec_recovered: u64,
    pub plc_frames: u64,
    pub lost_skipped: u64,
    pub underruns: u64,
    pub resyncs: u64,
    pub pulled_samples: u64,
    pub silence_samples: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Buffering,
    Playing,
}

pub struct PullInfo {
    /// Bu çekimde üretilen "gerçek olmayan" (tamponlama sessizliği) örnek sayısı.
    pub silence_samples: usize,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    packets: BTreeMap<u64, Vec<u8>>,
    /// Sarma dayanıklılığı için u32 seq'ler u64'e genişletilir.
    last_ext: Option<u64>,
    /// Çalınacak sıradaki çerçevenin genişletilmiş seq'i.
    next_seq: Option<u64>,
    fifo: VecDeque<f32>,
    scratch: Vec<f32>,
    state: State,
    plc_run: u32,
    target_ms: u32,
    clean_samples: u64,
    pub stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        let target_ms = cfg.start_target_ms.clamp(cfg.min_target_ms, cfg.max_target_ms);
        JitterBuffer {
            cfg,
            packets: BTreeMap::new(),
            last_ext: None,
            next_seq: None,
            fifo: VecDeque::new(),
            scratch: Vec::new(),
            state: State::Buffering,
            plc_run: 0,
            target_ms,
            clean_samples: 0,
            stats: JitterStats::default(),
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn fill_ms(&self) -> u32 {
        (self.fill_samples() as u64 * 1000 / self.cfg.sample_rate as u64) as u32
    }

    fn extend_seq(&mut self, seq: u32) -> u64 {
        let ext = match self.last_ext {
            None => seq as u64,
            Some(last) => {
                let base = last & !0xffff_ffffu64;
                let mut cand = base | seq as u64;
                if cand + 0x8000_0000 < last {
                    cand += 1 << 32;
                } else if cand > last + 0x8000_0000 && cand >= (1 << 32) {
                    cand -= 1 << 32;
                }
                cand
            }
        };
        self.last_ext = Some(ext);
        ext
    }

    fn target_samples(&self) -> usize {
        (self.target_ms as u64 * self.cfg.sample_rate as u64 / 1000) as usize
    }

    pub fn fill_samples(&self) -> usize {
        let map_samples = match (self.next_seq, self.packets.last_key_value()) {
            (Some(ns), Some((&hi, _))) if hi >= ns => ((hi - ns + 1) as usize) * self.cfg.frame_samples,
            _ => 0,
        };
        map_samples + self.fifo.len()
    }

    pub fn push(&mut self, seq: u32, payload: Vec<u8>) {
        let ext = self.extend_seq(seq);
        self.stats.pushed += 1;
        let ns = match self.next_seq {
            None => {
                self.next_seq = Some(ext);
                self.packets.insert(ext, payload);
                return;
            }
            Some(ns) => ns,
        };
        if ext < ns {
            self.stats.late += 1;
            return;
        }
        // Kuruyup tamponlamaya döndüysek eski çerçeveleri beklemek anlamsız:
        // akış nereden devam ediyorsa oraya atla.
        if self.state == State::Buffering && self.packets.is_empty() && self.fifo.is_empty() && ext > ns {
            self.stats.lost_skipped += ext - ns;
            self.next_seq = Some(ext);
            self.packets.insert(ext, payload);
            return;
        }
        // Devasa sıçrama: istemci resetlendi ya da çok uzun kopukluk.
        if ext - ns > self.cfg.resync_jump_frames {
            self.fast_forward(ext);
        }
        self.packets.insert(ext, payload);
        self.enforce_hard_cap();
    }

    fn fast_forward(&mut self, to: u64) {
        if let Some(ns) = self.next_seq {
            if to > ns {
                let already_have: u64 = self.packets.range(ns..to).count() as u64;
                self.stats.lost_skipped += (to - ns) - already_have;
            }
        }
        self.packets = self.packets.split_off(&to);
        self.next_seq = Some(to);
        self.fifo.clear();
        self.plc_run = 0;
        self.stats.resyncs += 1;
    }

    fn enforce_hard_cap(&mut self) {
        let cap = (self.cfg.hard_cap_ms as u64 * self.cfg.sample_rate as u64 / 1000) as usize;
        if self.fill_samples() <= cap {
            return;
        }
        if let Some((&hi, _)) = self.packets.last_key_value() {
            let keep_frames = (self.target_samples() / self.cfg.frame_samples).max(1) as u64;
            let to = hi.saturating_sub(keep_frames - 1);
            if Some(to) > self.next_seq {
                self.fast_forward(to);
            }
        }
    }

    fn bump_target(&mut self, add_ms: u32) {
        self.target_ms = (self.target_ms + add_ms).min(self.cfg.max_target_ms);
        self.clean_samples = 0;
    }

    /// Bir çerçeve üretip fifo'ya ekler. Üretemezse (kuru) false döner.
    fn produce_frame(&mut self, dec: &mut dyn FrameDecoder) -> bool {
        let ns = match self.next_seq {
            Some(n) => n,
            None => return false,
        };
        let f = self.cfg.frame_samples;
        self.scratch.resize(f, 0.0);
        if let Some(p) = self.packets.remove(&ns) {
            dec.decode(&p, &mut self.scratch);
            self.plc_run = 0;
            self.next_seq = Some(ns + 1);
        } else if let Some(p) = self.packets.get(&(ns + 1)) {
            // Tek paketlik kayıp: sonraki paketin FEC verisinden kurtar.
            let p = p.clone();
            dec.decode_fec(&p, &mut self.scratch);
            self.stats.fec_recovered += 1;
            self.plc_run = 0;
            self.next_seq = Some(ns + 1);
        } else if self.packets.is_empty() {
            // Tampon tamamen kuru: sınırlı PLC, sonra yeniden tamponla.
            if self.plc_run < self.cfg.plc_limit_frames {
                dec.conceal(&mut self.scratch);
                self.plc_run += 1;
                self.stats.plc_frames += 1;
                self.next_seq = Some(ns + 1);
                if self.plc_run == 3 {
                    self.bump_target(10);
                }
            } else {
                self.state = State::Buffering;
                self.stats.underruns += 1;
                self.plc_run = 0;
                self.bump_target(20);
                return false;
            }
        } else {
            let low = *self.packets.keys().next().unwrap();
            let gap = low - ns;
            if gap <= self.cfg.small_gap_frames {
                // Küçük boşluk: PLC ile geç (arkası zaten elimizde).
                dec.conceal(&mut self.scratch);
                self.plc_run += 1;
                self.stats.plc_frames += 1;
                self.next_seq = Some(ns + 1);
                if self.plc_run == 3 {
                    self.bump_target(10);
                }
            } else {
                // Büyük boşluk: robot sesiyle oyalanma, ileri sar.
                self.stats.lost_skipped += gap;
                self.stats.resyncs += 1;
                let p = self.packets.remove(&low).unwrap();
                dec.decode(&p, &mut self.scratch);
                self.plc_run = 0;
                self.next_seq = Some(low + 1);
            }
        }
        self.fifo.extend(self.scratch.iter().copied());
        true
    }

    pub fn pull(&mut self, out: &mut [f32], dec: &mut dyn FrameDecoder) -> PullInfo {
        self.stats.pulled_samples += out.len() as u64;
        if self.state == State::Buffering {
            if self.next_seq.is_some() && self.fill_samples() >= self.target_samples() {
                self.state = State::Playing;
            } else {
                out.fill(0.0);
                self.stats.silence_samples += out.len() as u64;
                return PullInfo { silence_samples: out.len() };
            }
        }
        let mut idx = 0;
        let mut silence = 0usize;
        while idx < out.len() {
            if self.fifo.is_empty() && !self.produce_frame(dec) {
                for o in &mut out[idx..] {
                    *o = 0.0;
                }
                silence = out.len() - idx;
                self.stats.silence_samples += silence as u64;
                break;
            }
            let n = self.fifo.len().min(out.len() - idx);
            for i in 0..n {
                out[idx + i] = self.fifo.pop_front().unwrap();
            }
            idx += n;
        }
        // Uzun süre sorunsuzsa hedef gecikmeyi yavaşça düşür (AIMD'nin "AI"si ters yönde).
        if silence == 0 {
            self.clean_samples += out.len() as u64;
            let need = self.cfg.decrease_after_s as u64 * self.cfg.sample_rate as u64;
            if self.clean_samples >= need
                && self.target_ms > self.cfg.min_target_ms
                && self.fill_samples() >= self.target_samples()
            {
                self.target_ms = (self.target_ms - 10).max(self.cfg.min_target_ms);
                self.clean_samples = 0;
            }
        } else {
            self.clean_samples = 0;
        }
        PullInfo { silence_samples: silence }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test çözücüsü: decode → payload[0], FEC → next_payload[0] - 1 + 0.25,
    /// PLC → -1.0 değeriyle doldurur. Böylece çıktıdan hangi yolun
    /// kullanıldığı okunabilir.
    struct Mock;

    impl FrameDecoder for Mock {
        fn decode(&mut self, payload: &[u8], out: &mut [f32]) {
            out.fill(payload[0] as f32);
        }
        fn decode_fec(&mut self, next_payload: &[u8], out: &mut [f32]) {
            out.fill(next_payload[0] as f32 - 1.0 + 0.25);
        }
        fn conceal(&mut self, out: &mut [f32]) {
            out.fill(-1.0);
        }
    }

    const F: usize = 960;

    fn cfg() -> JitterConfig {
        JitterConfig { start_target_ms: 80, ..Default::default() } // 80 ms = 4 çerçeve
    }

    fn pkt(k: u32) -> Vec<u8> {
        vec![k as u8; 4]
    }

    /// Bir çerçevelik çıktı çekip ilk örneğini döndürür.
    fn pull_frame(jb: &mut JitterBuffer, dec: &mut Mock) -> f32 {
        let mut buf = vec![9.9f32; F];
        jb.pull(&mut buf, dec);
        buf[0]
    }

    #[test]
    fn in_order_playback() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..8u32 {
            jb.push(k, pkt(k));
        }
        for k in 0..8u32 {
            assert_eq!(pull_frame(&mut jb, &mut dec), k as f32, "çerçeve {k}");
        }
        assert_eq!(jb.stats.plc_frames, 0);
        assert_eq!(jb.stats.fec_recovered, 0);
        assert_eq!(jb.stats.underruns, 0);
    }

    #[test]
    fn buffering_until_target() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        jb.push(0, pkt(10)); // 1 çerçeve < 4 çerçevelik hedef
        assert_eq!(pull_frame(&mut jb, &mut dec), 0.0); // sessizlik
        assert_eq!(jb.state(), State::Buffering);
        for k in 1..4u32 {
            jb.push(k, pkt(k + 10));
        }
        assert_eq!(pull_frame(&mut jb, &mut dec), 10.0); // artık çalıyor: çerçeve 0
        assert_eq!(jb.state(), State::Playing);
        assert_eq!(pull_frame(&mut jb, &mut dec), 11.0);
    }

    #[test]
    fn single_loss_recovered_via_fec() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..10u32 {
            if k != 5 {
                jb.push(k, pkt(k));
            }
        }
        let mut got = Vec::new();
        for _ in 0..9 {
            got.push(pull_frame(&mut jb, &mut dec));
        }
        // 5 numaralı çerçeve FEC ile kurtarılmış olmalı: 6 - 1 + 0.25 = 5.25
        assert_eq!(got[5], 5.25);
        assert_eq!(jb.stats.fec_recovered, 1);
        assert_eq!(jb.stats.plc_frames, 0);
    }

    #[test]
    fn dry_buffer_plc_then_late_drop() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..5u32 {
            jb.push(k, pkt(k));
        }
        for k in 0..5 {
            assert_eq!(pull_frame(&mut jb, &mut dec), k as f32);
        }
        // Tampon kuru: PLC devrede
        assert_eq!(pull_frame(&mut jb, &mut dec), -1.0);
        assert!(jb.stats.plc_frames >= 1);
        // 5. çerçeve PLC ile dolduruldu; şimdi geç gelirse atılmalı
        jb.push(5, pkt(5));
        assert_eq!(jb.stats.late, 1);
        // 6'dan itibaren normal devam
        for k in 6..10u32 {
            jb.push(k, pkt(k));
        }
        assert_eq!(pull_frame(&mut jb, &mut dec), 6.0);
    }

    #[test]
    fn small_gap_concealed() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..12u32 {
            if k != 4 && k != 5 {
                jb.push(k, pkt(k));
            }
        }
        let mut got = Vec::new();
        for _ in 0..10 {
            got.push(pull_frame(&mut jb, &mut dec));
        }
        // 4 → PLC (-1.0); 5 → ardılı elimizde olduğundan FEC ile kurtarılır (5.25); 6 normal
        assert_eq!(got[4], -1.0);
        assert_eq!(got[5], 5.25);
        assert_eq!(got[6], 6.0);
        assert_eq!(jb.stats.plc_frames, 1);
        assert_eq!(jb.stats.fec_recovered, 1);
        assert_eq!(jb.stats.resyncs, 0);
    }

    #[test]
    fn big_gap_fast_forwards() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        // 4-7 kayıp: small_gap_frames(3)'ten büyük boşluk → PLC yerine ileri sarma
        for k in 0..4u32 {
            jb.push(k, pkt(k));
        }
        for k in 8..14u32 {
            jb.push(k, pkt(k));
        }
        let mut got = Vec::new();
        for _ in 0..6 {
            got.push(pull_frame(&mut jb, &mut dec));
        }
        assert_eq!(&got[0..4], &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(got[4], 8.0, "ileri sarmalıydı");
        assert_eq!(jb.stats.resyncs, 1);
        assert_eq!(jb.stats.lost_skipped, 4);
    }

    #[test]
    fn huge_span_jumps_to_live_edge() {
        // Uzun kopukluk sonrası paket patlaması: span hard_cap'i aşar →
        // canlı uca atlanır (eski ses çalınmaz, gecikme birikmez).
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..4u32 {
            jb.push(k, pkt(k));
        }
        for k in 30..40u32 {
            jb.push(k, pkt(k));
        }
        // hard cap devrede: doluluk 400 ms'i aşmamalı
        assert!(jb.fill_ms() <= 400);
        // İlk çekilen sesler artık canlı uca yakın olmalı (0..4 çalınmaz)
        let v = pull_frame(&mut jb, &mut dec);
        assert!(v >= 30.0 || v == -1.0 || v.fract() == 0.25, "canlı uca atlamalıydı, geldi: {v}");
        assert!(jb.stats.resyncs >= 1);
    }

    #[test]
    fn underrun_rebuffers_and_raises_target() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        let t0 = jb.target_ms();
        for k in 0..4u32 {
            jb.push(k, pkt(k));
        }
        // 4 gerçek çerçeve + plc_limit(6) PLC + sonrasında underrun
        for _ in 0..12 {
            pull_frame(&mut jb, &mut dec);
        }
        assert_eq!(jb.state(), State::Buffering);
        assert_eq!(jb.stats.underruns, 1);
        assert!(jb.target_ms() > t0, "hedef artmalıydı: {} → {}", t0, jb.target_ms());
        // Akış çok ileriden devam ediyor (istemci göndermeye devam etmişti)
        for k in 30..40u32 {
            jb.push(k, pkt(k));
        }
        // Yeterli doluluk → tekrar çalmaya başlar, 30'dan devam eder
        let v = pull_frame(&mut jb, &mut dec);
        assert_eq!(v, 30.0);
        assert_eq!(jb.state(), State::Playing);
    }

    #[test]
    fn target_decreases_after_clean_run() {
        let mut c = cfg();
        c.decrease_after_s = 1;
        let mut jb = JitterBuffer::new(c);
        let mut dec = Mock;
        let t0 = jb.target_ms();
        let mut next_push = 0u32;
        // Doluluğu hedefin üstünde tutarak 2+ saniye temiz oynat
        for _ in 0..8u32 {
            jb.push(next_push, pkt(next_push));
            next_push += 1;
        }
        for _ in 0..120 {
            jb.push(next_push, pkt(next_push));
            next_push += 1;
            pull_frame(&mut jb, &mut dec);
        }
        assert!(jb.target_ms() < t0, "hedef düşmeliydi: {} → {}", t0, jb.target_ms());
        assert_eq!(jb.stats.underruns, 0);
    }

    #[test]
    fn hard_cap_limits_fill() {
        let mut jb = JitterBuffer::new(cfg());
        for k in 0..2000u32 {
            jb.push(k, pkt(k));
        }
        assert!(jb.fill_ms() <= 400, "doluluk {}ms > 400ms", jb.fill_ms());
        assert!(jb.stats.resyncs > 0);
    }

    #[test]
    fn seq_wraparound_survives() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        let start = u32::MAX - 3;
        for i in 0..8u32 {
            let seq = start.wrapping_add(i);
            jb.push(seq, pkt(i + 100));
        }
        for i in 0..8u32 {
            assert_eq!(pull_frame(&mut jb, &mut dec), (i + 100) as f32);
        }
        assert_eq!(jb.stats.resyncs, 0);
        assert_eq!(jb.stats.late, 0);
    }
}
