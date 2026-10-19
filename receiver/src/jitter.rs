//! Adaptive jitter buffer — where "choppy audio" gets fixed.
//!
//! The time base is deterministic: everything runs on "samples pulled", never
//! on the wall clock, so unit tests can replay network conditions exactly.
//!
//! Adaptation in one paragraph: the buffer aims for `target_ms` of audio.
//! When it runs dry it conceals (PLC) but keeps waiting for the missing packet
//! instead of skipping it ("elastic"): a merely late packet is still played and
//! the buffer has grown by the concealed time, which then becomes the new
//! target. Packets that arrive after their slot was already concealed also
//! raise the target. A long clean run lowers it again (AIMD).

use std::collections::{BTreeMap, VecDeque};

/// Turns a frame into PCM (Opus or raw PCM16).
pub trait FrameDecoder {
    /// Normal decode. `out` is exactly one frame long.
    fn decode(&mut self, payload: &[u8], out: &mut [f32]);
    /// Rebuild a lost frame from the in-band FEC data of the packet that FOLLOWS it.
    fn decode_fec(&mut self, next_payload: &[u8], out: &mut [f32]);
    /// Packet loss concealment (PLC): synthesize a frame when there is no data at all.
    fn conceal(&mut self, out: &mut [f32]);
}

#[derive(Debug, Clone)]
pub struct JitterConfig {
    pub sample_rate: u32,
    pub frame_samples: usize,
    pub start_target_ms: u32,
    pub min_target_ms: u32,
    pub max_target_ms: u32,
    /// At most this many consecutive frames are concealed, then we rebuffer.
    pub plc_limit_frames: u32,
    /// Gaps up to this size are concealed; bigger ones are skipped over.
    pub small_gap_frames: u64,
    /// After this many seconds of clean playback the target is lowered.
    pub decrease_after_s: u32,
    /// If the buffer grows past this, the oldest audio is dropped (overflow guard).
    pub hard_cap_ms: u32,
    /// While playing, a seq jump bigger than this is treated as a sender reset.
    pub resync_jump_frames: u64,
    /// Target increase when a packet arrives after its slot was concealed.
    pub late_bump_ms: u32,
    /// Upper bound for the target increase after a single stall.
    pub max_stall_bump_ms: u32,
}

impl Default for JitterConfig {
    fn default() -> Self {
        JitterConfig {
            sample_rate: 48000,
            frame_samples: 960,
            // Low latency first: on USB / a clean network the buffer settles at
            // 30 ms; on a choppy Wi-Fi the adaptation raises it as needed.
            start_target_ms: 60,
            min_target_ms: 30,
            max_target_ms: 300,
            plc_limit_frames: 6,
            small_gap_frames: 3,
            decrease_after_s: 10,
            hard_cap_ms: 400,
            resync_jump_frames: 500,
            late_bump_ms: 10,
            max_stall_bump_ms: 40,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub pushed: u64,
    /// Frames decoded from their own packet (the normal path).
    pub decoded: u64,
    pub late: u64,
    pub fec_recovered: u64,
    pub plc_frames: u64,
    /// Times the buffer ran dry and then continued with the waited-for audio.
    pub stalls: u64,
    pub lost_skipped: u64,
    pub underruns: u64,
    pub resyncs: u64,
    pub pulled_samples: u64,
    pub silence_samples: u64,
}

impl JitterStats {
    /// Share of played frames that were not decoded from their own packet.
    pub fn concealed_ratio(&self) -> f64 {
        let total = self.decoded + self.fec_recovered + self.plc_frames;
        if total == 0 {
            0.0
        } else {
            (self.fec_recovered + self.plc_frames) as f64 / total as f64
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Buffering,
    Playing,
}

pub struct PullInfo {
    /// Samples of this pull that are "not real" (buffering silence).
    pub silence_samples: usize,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    packets: BTreeMap<u64, Vec<u8>>,
    /// u32 seqs are extended to u64 so wraparound is harmless.
    last_ext: Option<u64>,
    /// Extended seq of the next frame to play.
    next_seq: Option<u64>,
    fifo: VecDeque<f32>,
    scratch: Vec<f32>,
    state: State,
    plc_run: u32,
    /// Frames concealed while waiting for a late `next_seq` (elastic stall).
    stall_frames: u32,
    target_ms: u32,
    clean_samples: u64,
    /// Pulled samples left before a late arrival may raise the target again.
    late_cooldown: u64,
    /// Fill smoothed over ~0.5 s of pulls. The instantaneous fill jumps by a
    /// whole frame whenever a packet lands, so the drift servo and the target
    /// decrease look at this instead.
    fill_avg: f64,
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
            stall_frames: 0,
            target_ms,
            clean_samples: 0,
            late_cooldown: 0,
            fill_avg: 0.0,
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

    /// Smoothed fill (see `fill_avg`) — what the drift servo should steer by.
    pub fn avg_fill_ms(&self) -> u32 {
        (self.fill_avg * 1000.0 / self.cfg.sample_rate as f64) as u32
    }

    fn frame_ms(&self) -> u32 {
        (self.cfg.frame_samples as u64 * 1000 / self.cfg.sample_rate as u64) as u32
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
            // Its slot was already concealed (FEC/PLC): the cushion is too thin
            // for this network. Ask for more headroom, at most once a second.
            self.stats.late += 1;
            if self.late_cooldown == 0 {
                self.bump_target(self.cfg.late_bump_ms);
                self.late_cooldown = self.cfg.sample_rate as u64;
            }
            return;
        }
        // We ran dry and went back to buffering: waiting for old frames makes
        // no sense, continue from wherever the stream is now.
        if self.state == State::Buffering && self.packets.is_empty() && self.fifo.is_empty() && ext > ns {
            self.stats.lost_skipped += ext - ns;
            self.next_seq = Some(ext);
            self.packets.insert(ext, payload);
            return;
        }
        // Huge jump: the client restarted or was gone for a long time.
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
        self.stall_frames = 0;
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

    /// A stall just ended with real audio: the buffer grew by the concealed
    /// time. Keep (most of) that headroom so the same spike won't starve us again.
    fn end_stall(&mut self) {
        if self.stall_frames > 0 {
            let grown = self.stall_frames * self.frame_ms();
            self.stats.stalls += 1;
            self.bump_target(grown.min(self.cfg.max_stall_bump_ms));
            self.stall_frames = 0;
        }
    }

    /// Produces one frame into the fifo. Returns false if it can't (dry).
    fn produce_frame(&mut self, dec: &mut dyn FrameDecoder) -> bool {
        let ns = match self.next_seq {
            Some(n) => n,
            None => return false,
        };
        let f = self.cfg.frame_samples;
        self.scratch.resize(f, 0.0);
        if let Some(p) = self.packets.remove(&ns) {
            dec.decode(&p, &mut self.scratch);
            self.stats.decoded += 1;
            self.end_stall();
            self.plc_run = 0;
            self.next_seq = Some(ns + 1);
        } else if let Some(p) = self.packets.get(&(ns + 1)) {
            // Single missing packet: rebuild it from the next packet's FEC data.
            let p = p.clone();
            dec.decode_fec(&p, &mut self.scratch);
            self.stats.fec_recovered += 1;
            self.end_stall();
            self.plc_run = 0;
            self.next_seq = Some(ns + 1);
        } else if self.packets.is_empty() {
            // Dry buffer: the packet is late (jitter) or lost. Conceal, but keep
            // waiting for the same seq — if it shows up it is still played and
            // the buffer simply grew by the concealed time, no real audio lost.
            if self.plc_run < self.cfg.plc_limit_frames {
                dec.conceal(&mut self.scratch);
                self.plc_run += 1;
                self.stall_frames += 1;
                self.stats.plc_frames += 1;
            } else {
                // Still nothing: rebuffer (silence) and ask for more headroom.
                self.state = State::Buffering;
                self.stats.underruns += 1;
                self.plc_run = 0;
                self.stall_frames = 0;
                self.bump_target(20);
                return false;
            }
        } else {
            let low = *self.packets.keys().next().unwrap();
            let gap = low - ns;
            if gap <= self.cfg.small_gap_frames {
                // Small gap: conceal it (what follows is already here).
                dec.conceal(&mut self.scratch);
                self.plc_run += 1;
                self.stats.plc_frames += 1;
                self.next_seq = Some(ns + 1);
                if self.plc_run == 3 {
                    self.bump_target(10);
                }
            } else {
                // Big gap: don't drag on with robot voice, skip ahead.
                self.stats.lost_skipped += gap;
                self.stats.resyncs += 1;
                let p = self.packets.remove(&low).unwrap();
                dec.decode(&p, &mut self.scratch);
                self.stats.decoded += 1;
                self.plc_run = 0;
                self.stall_frames = 0;
                self.next_seq = Some(low + 1);
            }
        }
        self.fifo.extend(self.scratch.iter().copied());
        true
    }

    pub fn pull(&mut self, out: &mut [f32], dec: &mut dyn FrameDecoder) -> PullInfo {
        self.stats.pulled_samples += out.len() as u64;
        self.late_cooldown = self.late_cooldown.saturating_sub(out.len() as u64);
        if self.state == State::Buffering {
            if self.next_seq.is_some() && self.fill_samples() >= self.target_samples() {
                self.state = State::Playing;
                self.fill_avg = self.fill_samples() as f64;
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
            for (o, s) in out[idx..idx + n].iter_mut().zip(self.fifo.drain(..n)) {
                *o = s;
            }
            idx += n;
        }
        let alpha = (out.len() as f64 / (0.5 * self.cfg.sample_rate as f64)).min(1.0);
        self.fill_avg += (self.fill_samples() as f64 - self.fill_avg) * alpha;
        // After a long clean run, slowly lower the target delay (the
        // "decrease" half of AIMD) — but only once the fill has caught up.
        if silence == 0 {
            self.clean_samples += out.len() as u64;
            let need = self.cfg.decrease_after_s as u64 * self.cfg.sample_rate as u64;
            let half_frame = self.cfg.frame_samples as f64 / 2.0;
            if self.clean_samples >= need
                && self.target_ms > self.cfg.min_target_ms
                && self.fill_avg + half_frame >= self.target_samples() as f64
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

    /// Test decoder: decode → payload[0], FEC → next_payload[0] - 1 + 0.25,
    /// PLC → -1.0. The output tells which path produced each frame.
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
        JitterConfig { start_target_ms: 80, ..Default::default() } // 80 ms = 4 frames
    }

    fn pkt(k: u32) -> Vec<u8> {
        vec![k as u8; 4]
    }

    /// Pulls one frame and returns its first sample.
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
            assert_eq!(pull_frame(&mut jb, &mut dec), k as f32, "frame {k}");
        }
        assert_eq!(jb.stats.plc_frames, 0);
        assert_eq!(jb.stats.fec_recovered, 0);
        assert_eq!(jb.stats.underruns, 0);
        assert_eq!(jb.stats.decoded, 8);
    }

    #[test]
    fn buffering_until_target() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        jb.push(0, pkt(10)); // 1 frame < 4-frame target
        assert_eq!(pull_frame(&mut jb, &mut dec), 0.0); // silence
        assert_eq!(jb.state(), State::Buffering);
        for k in 1..4u32 {
            jb.push(k, pkt(k + 10));
        }
        assert_eq!(pull_frame(&mut jb, &mut dec), 10.0); // playing now: frame 0
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
        // Frame 5 must come from FEC: 6 - 1 + 0.25 = 5.25
        assert_eq!(got[5], 5.25);
        assert_eq!(jb.stats.fec_recovered, 1);
        assert_eq!(jb.stats.plc_frames, 0);
    }

    #[test]
    fn late_packet_after_dry_plc_is_still_played() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..5u32 {
            jb.push(k, pkt(k));
        }
        for k in 0..5 {
            assert_eq!(pull_frame(&mut jb, &mut dec), k as f32);
        }
        // Dry: PLC fills in, but frame 5 is still awaited (elastic stall)
        assert_eq!(pull_frame(&mut jb, &mut dec), -1.0);
        assert_eq!(jb.stats.plc_frames, 1);
        let t0 = jb.target_ms();
        // Frame 5 shows up late: it is played, not thrown away
        for k in 5..10u32 {
            jb.push(k, pkt(k));
        }
        assert_eq!(pull_frame(&mut jb, &mut dec), 5.0);
        assert_eq!(pull_frame(&mut jb, &mut dec), 6.0);
        assert_eq!(jb.stats.late, 0);
        assert_eq!(jb.stats.stalls, 1);
        // ...and the 20 ms it grew by is kept as extra headroom
        assert_eq!(jb.target_ms(), t0 + 20);
    }

    #[test]
    fn reordered_packet_raises_target() {
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..10u32 {
            if k != 4 {
                jb.push(k, pkt(k));
            }
        }
        for _ in 0..5 {
            pull_frame(&mut jb, &mut dec); // frame 4 comes from FEC
        }
        assert_eq!(jb.stats.fec_recovered, 1);
        let t0 = jb.target_ms();
        jb.push(4, pkt(4)); // arrives after its slot: too late
        assert_eq!(jb.stats.late, 1);
        assert_eq!(jb.target_ms(), t0 + 10);
        // A second late packet within the cooldown doesn't stack up
        jb.push(3, pkt(3));
        assert_eq!(jb.target_ms(), t0 + 10);
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
        // 4 → PLC (-1.0); 5 → its successor is here, so FEC (5.25); 6 normal
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
        // 4-7 lost: bigger than small_gap_frames (3) → skip ahead instead of PLC
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
        assert_eq!(got[4], 8.0, "should have skipped ahead");
        assert_eq!(jb.stats.resyncs, 1);
        assert_eq!(jb.stats.lost_skipped, 4);
    }

    #[test]
    fn huge_span_jumps_to_live_edge() {
        // Packet burst after a long outage: the span exceeds hard_cap →
        // jump to the live edge (no stale audio, no latency build-up).
        let mut jb = JitterBuffer::new(cfg());
        let mut dec = Mock;
        for k in 0..4u32 {
            jb.push(k, pkt(k));
        }
        for k in 30..40u32 {
            jb.push(k, pkt(k));
        }
        // hard cap in effect: fill must not exceed 400 ms
        assert!(jb.fill_ms() <= 400);
        // The first audio pulled must be near the live edge (0..4 are not played)
        let v = pull_frame(&mut jb, &mut dec);
        assert!(v >= 30.0 || v == -1.0 || v.fract() == 0.25, "should jump to the live edge, got: {v}");
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
        // 4 real frames + plc_limit (6) PLC frames, then underrun
        for _ in 0..12 {
            pull_frame(&mut jb, &mut dec);
        }
        assert_eq!(jb.state(), State::Buffering);
        assert_eq!(jb.stats.underruns, 1);
        assert!(jb.target_ms() > t0, "target should rise: {} → {}", t0, jb.target_ms());
        // The stream continues much further on (the client kept sending)
        for k in 30..40u32 {
            jb.push(k, pkt(k));
        }
        // Enough fill → playing again, continuing from 30
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
        // Play 2+ seconds cleanly while keeping the fill above the target
        for _ in 0..8u32 {
            jb.push(next_push, pkt(next_push));
            next_push += 1;
        }
        for _ in 0..120 {
            jb.push(next_push, pkt(next_push));
            next_push += 1;
            pull_frame(&mut jb, &mut dec);
        }
        assert!(jb.target_ms() < t0, "target should drop: {} → {}", t0, jb.target_ms());
        assert_eq!(jb.stats.underruns, 0);
    }

    #[test]
    fn hard_cap_limits_fill() {
        let mut jb = JitterBuffer::new(cfg());
        for k in 0..2000u32 {
            jb.push(k, pkt(k));
        }
        assert!(jb.fill_ms() <= 400, "fill {}ms > 400ms", jb.fill_ms());
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

    /// Jittery network model: packet k is sent at k*20 ms and arrives after a
    /// random 0..jitter_ms delay (so reordering happens). The consumer pulls
    /// 10 ms per tick and, like the supervisor's drift servo, nudges its speed
    /// once per second to keep the fill near the target.
    fn simulate(jitter_ms: f64, seconds: u32, seed: u64) -> JitterBuffer {
        let mut jb = JitterBuffer::new(JitterConfig::default());
        let mut dec = Mock;
        let mut x = seed.max(1);
        let mut rand = move || {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64
        };
        let frames = seconds * 50;
        let mut arrivals: Vec<(f64, u32)> =
            (0..frames).map(|k| (k as f64 * 20.0 + rand() * jitter_ms, k)).collect();
        arrivals.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        let (mut ai, mut now, mut tick) = (0usize, 0f64, 0u32);
        let (mut adj, mut carry) = (0f64, 0f64);
        let mut buf = Vec::new();
        while now < seconds as f64 * 1000.0 {
            now += 10.0;
            tick += 1;
            while ai < arrivals.len() && arrivals[ai].0 <= now {
                jb.push(arrivals[ai].1, pkt(arrivals[ai].1));
                ai += 1;
            }
            if tick % 100 == 0 {
                let raw = ((jb.avg_fill_ms() as f64 - jb.target_ms() as f64) * 100.0).clamp(-3000.0, 3000.0);
                adj = if jb.state() == State::Playing { 0.7 * adj + 0.3 * raw * 1e-6 } else { 0.0 };
            }
            let want = 480.0 * (1.0 + adj) + carry;
            carry = want - want.floor();
            buf.resize(want.floor() as usize, 0.0);
            jb.pull(&mut buf, &mut dec);
        }
        jb
    }

    #[test]
    fn jittery_network_is_absorbed_not_dropped() {
        // 60 ms of jitter with zero real loss: nothing should be thrown away,
        // and only a sliver of the audio may be synthesized.
        let jb = simulate(60.0, 120, 3);
        let st = jb.stats;
        assert!(
            st.concealed_ratio() < 0.01 && st.late < 20 && st.underruns == 0,
            "concealed {:.2}% (plc {} fec {}), late {}, underruns {}",
            st.concealed_ratio() * 100.0,
            st.plc_frames,
            st.fec_recovered,
            st.late,
            st.underruns
        );
        // The price is latency, and it should stay sane
        assert!(jb.target_ms() <= 160, "target ran away: {} ms", jb.target_ms());
    }

    #[test]
    fn clean_network_keeps_latency_at_the_floor() {
        // USB / a perfect network: the buffer must settle at the minimum.
        let jb = simulate(0.0, 90, 1);
        assert_eq!(jb.target_ms(), JitterConfig::default().min_target_ms);
        assert_eq!(jb.stats.plc_frames, 0);
    }
}
