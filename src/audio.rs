//! Audio buffer: whatever the device gives → 16 kHz mono i16, plus WAV encoding.

use std::io::Cursor;

pub const SAMPLE_RATE: u32 = 16_000;

/// The most a recording loses to WASAPI's opening click: the first samples sit pinned at full
/// scale and decay over ~250 ms. Sent to whisper, the click inflates the first segment's
/// `no_speech_prob` and on short clips yields hallucinated subtitle credits.
const START_CLICK_MAX_SAMPLES: usize = SAMPLE_RATE as usize * 300 / 1000;

const CLICK_WINDOW: usize = SAMPLE_RATE as usize / 100;

/// Room tone: a stretch at or under this RMS level holds no speech. Where the start-click cut
/// stops, and what makes a window tail quiet enough to drop.
const ROOM_TONE_DBFS: f32 = -30.0;

/// Silence: a capture at or under this RMS level in every 10 ms window came from a muted or dead
/// microphone. Quiet dictation and live-microphone room tone both peak above it.
const SILENT_CAPTURE_DBFS: f32 = -40.0;

const WHISPER_WINDOW: usize = SAMPLE_RATE as usize * 30;

const WINDOW_TAIL_MAX: usize = SAMPLE_RATE as usize;

#[derive(Clone, Debug, PartialEq)]
pub struct Audio {
    pub samples: Vec<i16>,
}

impl Audio {
    pub fn from_f32(input: &[f32], channels: u16, rate: u32) -> Audio {
        let mono = downmix(input, channels.max(1) as usize);
        let at_16k = resample(&mono, rate, SAMPLE_RATE);
        let samples = at_16k
            .iter()
            .map(|v| (v.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
            .collect();
        Audio { samples }
    }

    pub fn duration_secs(&self) -> f32 {
        self.samples.len() as f32 / SAMPLE_RATE as f32
    }

    pub fn peak_dbfs(&self) -> f32 {
        let peak = self
            .samples
            .iter()
            .map(|s| (*s as i32).abs())
            .max()
            .unwrap_or(0);
        if peak == 0 {
            return -120.0;
        }
        20.0 * (peak as f32 / i16::MAX as f32).log10()
    }

    /// `self` minus the click a WASAPI capture opens with, when it did. The signature is the first
    /// 10 ms pinned far from zero (160 samples above a quarter of full scale is a rail, never a
    /// voice), so a device that opens cleanly keeps its first 300 ms.
    pub fn without_start_click(mut self) -> Audio {
        let pinned = self.samples.len() >= CLICK_WINDOW
            && self.samples[..CLICK_WINDOW]
                .iter()
                .all(|s| s.unsigned_abs() > i16::MAX as u16 / 4);
        if !pinned {
            return self;
        }
        let limit = self.samples.len().min(START_CLICK_MAX_SAMPLES);
        let mut end = 0;
        let mut prev = f32::INFINITY;
        while end + CLICK_WINDOW <= limit {
            let db = rms_dbfs(&self.samples[end..end + CLICK_WINDOW]);
            // 1 dB of tolerance: the decay is not monotonic at the 10 ms scale.
            if db <= ROOM_TONE_DBFS || db > prev + 1.0 {
                break;
            }
            prev = db;
            end += CLICK_WINDOW;
        }
        self.samples.drain(..end);
        self
    }

    /// `self` minus a quiet sub-second remainder past its last 30 s multiple. Whisper decodes in 30 s
    /// windows and invents subtitle credits for a short last one, scoring `no_speech_prob` 0.000, so
    /// the no-speech gate cannot catch it. Only a quiet remainder under 1 s goes.
    pub fn without_window_tail(mut self) -> Audio {
        let len = self.samples.len();
        let rem = len % WHISPER_WINDOW;
        if len > WHISPER_WINDOW
            && rem < WINDOW_TAIL_MAX
            && self.samples[len - rem..]
                .chunks(CLICK_WINDOW)
                .all(|w| rms_dbfs(w) <= ROOM_TONE_DBFS)
        {
            self.samples.truncate(len - rem);
        }
        self
    }

    pub fn is_silent(&self) -> bool {
        self.samples
            .chunks(CLICK_WINDOW)
            .all(|w| rms_dbfs(w) <= SILENT_CAPTURE_DBFS)
    }

    pub fn loudest_window_dbfs(&self) -> f32 {
        self.samples
            .chunks(CLICK_WINDOW)
            .map(rms_dbfs)
            .fold(f32::NEG_INFINITY, f32::max)
            .max(-120.0)
    }

    pub fn window_percentile_dbfs(&self, pct: f64) -> f32 {
        if self.samples.is_empty() {
            return -120.0;
        }
        let mut windows: Vec<f32> = self.samples.chunks(CLICK_WINDOW).map(rms_dbfs).collect();
        windows.sort_by(f32::total_cmp);
        let idx = ((pct / 100.0) * (windows.len() - 1) as f64).round() as usize;
        windows[idx].max(-120.0)
    }

    pub fn to_wav(&self) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cursor, spec).expect("in-memory writer");
            for s in &self.samples {
                w.write_sample(*s).expect("in-memory write");
            }
            w.finalize().expect("in-memory finalize");
        }
        cursor.into_inner()
    }
}

fn downmix(input: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return input.to_vec();
    }
    input
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

fn rms_dbfs(w: &[i16]) -> f32 {
    let mean_sq = w.iter().map(|s| (*s as f32).powi(2)).sum::<f32>() / w.len().max(1) as f32;
    20.0 * (mean_sq.sqrt() / i16::MAX as f32).log10()
}

fn resample(mono: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || from == 0 || mono.is_empty() {
        return mono.to_vec();
    }
    let filtered: Vec<f32> = if from > to {
        // Average over the decimation window to damp aliasing before interpolating. Ratios below
        // 1.5 round `win` to 1 and are not prefiltered.
        let win = (from as f32 / to as f32).round().max(1.0) as usize;
        (0..mono.len())
            .map(|i| {
                let lo = i.saturating_sub(win / 2);
                let hi = (i + win / 2 + 1).min(mono.len());
                mono[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
            })
            .collect()
    } else {
        mono.to_vec()
    };
    let out_len = ((mono.len() as u64 * to as u64) / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = filtered[idx.min(filtered.len() - 1)];
            let b = filtered[(idx + 1).min(filtered.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn click(ms: usize) -> Vec<i16> {
        (0..SAMPLE_RATE as usize * ms / 1000)
            .map(|i| (i16::MAX as f32 * 0.5f32.powf(i as f32 / 800.0)) as i16)
            .collect()
    }

    #[test]
    fn only_a_capture_silent_in_every_window_is_silent() {
        let zero = Audio {
            samples: vec![0; 16_000 + 80],
        };
        assert!(zero.is_silent(), "digital zero");
        let near_zero = Audio {
            samples: vec![300; 16_000],
        };
        assert!(near_zero.is_silent(), "near-zero input at -40.8 dBFS");
        let mut loud_tail = zero.samples.clone();
        for s in &mut loud_tail[16_000..] {
            *s = 8000;
        }
        let loud_tail = Audio { samples: loud_tail };
        assert!(!loud_tail.is_silent(), "a loud final partial window");
    }

    #[test]
    fn the_loudest_window_can_sit_at_the_gate_while_the_peak_reads_fine() {
        let empty = Audio { samples: vec![] };
        assert_eq!(empty.loudest_window_dbfs(), -120.0);

        let probe = Audio::from_f32(
            &sine(SAMPLE_RATE, 1, 1.0, 200.0, 460.0 / i16::MAX as f32),
            1,
            SAMPLE_RATE,
        );
        assert!(probe.peak_dbfs() > -40.0, "{}", probe.peak_dbfs());
        assert!(
            (probe.loudest_window_dbfs() - (-40.07)).abs() < 0.2,
            "{}",
            probe.loudest_window_dbfs()
        );
        assert!(probe.is_silent(), "{}", probe.loudest_window_dbfs());
    }

    #[test]
    fn the_loudest_window_counts_a_final_partial_window() {
        let mut samples = vec![0i16; 16_000];
        samples.extend(std::iter::repeat_n(8000i16, 80));
        let a = Audio { samples };
        assert!(
            a.loudest_window_dbfs() > -40.0,
            "{}",
            a.loudest_window_dbfs()
        );
    }

    #[test]
    fn window_percentile_is_nearest_rank_and_empty_reads_as_the_floor() {
        let empty = Audio { samples: vec![] };
        assert_eq!(empty.window_percentile_dbfs(50.0), -120.0);
        assert_eq!(empty.window_percentile_dbfs(95.0), -120.0);

        let mut samples = Vec::new();
        for lvl in [-50.0, -40.0, -30.0, -20.0, -10.0] {
            let amp = (i16::MAX as f32 * 10f32.powf(lvl / 20.0)).round() as i16;
            samples.extend(std::iter::repeat_n(amp, CLICK_WINDOW));
        }
        let a = Audio { samples };
        assert!((a.window_percentile_dbfs(0.0) - (-50.0)).abs() < 0.1);
        assert!((a.window_percentile_dbfs(50.0) - (-30.0)).abs() < 0.1);
        assert!((a.window_percentile_dbfs(95.0) - (-10.0)).abs() < 0.1);
    }

    #[test]
    fn window_percentile_of_digital_silence_is_the_floor() {
        let zero = Audio {
            samples: vec![0; SAMPLE_RATE as usize],
        };
        assert_eq!(zero.window_percentile_dbfs(50.0), -120.0);
        assert_eq!(zero.window_percentile_dbfs(95.0), -120.0);

        let mut samples = vec![0i16; SAMPLE_RATE as usize];
        samples[..CLICK_WINDOW].fill(8000);
        let mostly_zero = Audio { samples };
        assert_eq!(mostly_zero.window_percentile_dbfs(50.0), -120.0);
    }

    #[test]
    fn window_percentile_counts_a_final_partial_window() {
        let mut samples = vec![0i16; 16_000];
        samples.extend(std::iter::repeat_n(8000i16, 80));
        let a = Audio { samples };
        assert!(
            a.window_percentile_dbfs(100.0) > -40.0,
            "{}",
            a.window_percentile_dbfs(100.0)
        );
    }

    #[test]
    fn a_wasapi_start_click_is_cut_and_a_clean_start_is_kept() {
        let mut samples = vec![i16::MAX; 2_000];
        samples.resize(SAMPLE_RATE as usize, 100);
        let cut = Audio { samples }.without_start_click();
        assert_eq!(cut.samples.len(), 16_000 - 2_080);
        assert!(cut.samples.iter().all(|s| *s == 100));
        let voice = Audio::from_f32(&sine(SAMPLE_RATE, 1, 0.5, 200.0, 0.5), 1, SAMPLE_RATE).samples;
        let mut samples = click(200);
        samples.extend_from_slice(&voice);
        assert_eq!(Audio { samples }.without_start_click().samples, voice);
        let mut samples = vec![i16::MAX; 400];
        samples.resize(16_000, i16::MAX / 2);
        let capped = Audio { samples }.without_start_click();
        assert_eq!(capped.samples.len(), 16_000 - 4_800);
        let neg = Audio {
            samples: vec![i16::MIN; 4_000],
        }
        .without_start_click();
        assert!(neg.samples.is_empty());
        let voice =
            Audio::from_f32(&sine(SAMPLE_RATE, 1, 1.0, 200.0, 0.95), 1, SAMPLE_RATE).samples;
        for start in [vec![100i16; 16_000], voice] {
            let kept = Audio {
                samples: start.clone(),
            }
            .without_start_click();
            assert_eq!(kept.samples, start);
        }
        let short = Audio {
            samples: vec![i16::MAX; 100],
        }
        .without_start_click();
        assert_eq!(short.samples.len(), 100);
    }

    fn tone_then(secs: usize, tail_ms: usize, tail: i16) -> Audio {
        let mut samples = vec![200i16; SAMPLE_RATE as usize * secs];
        samples.extend(std::iter::repeat_n(
            tail,
            SAMPLE_RATE as usize * tail_ms / 1000,
        ));
        Audio { samples }
    }

    #[test]
    fn a_quiet_sub_second_tail_past_a_whisper_window_is_cut() {
        let w = WHISPER_WINDOW;
        assert_eq!(
            tone_then(30, 160, 200).without_window_tail().samples.len(),
            w
        );
        assert_eq!(
            tone_then(60, 500, 200).without_window_tail().samples.len(),
            2 * w
        );
        let loud = tone_then(30, 500, i16::MAX / 4);
        assert_eq!(loud.clone().without_window_tail(), loud);
        // A quiet word in the remainder: 300 ms at about -27 dBFS inside 900 ms of room tone.
        // Averaged over the remainder it is under -30 dBFS, yet it is speech. Kept.
        let mut samples = vec![200i16; WHISPER_WINDOW];
        samples.extend(
            Audio::from_f32(&sine(SAMPLE_RATE, 1, 0.3, 200.0, 0.063), 1, SAMPLE_RATE).samples,
        );
        samples.resize(WHISPER_WINDOW + SAMPLE_RATE as usize * 900 / 1000, 200);
        let word = Audio { samples };
        assert!(rms_dbfs(&word.samples[WHISPER_WINDOW..]) <= ROOM_TONE_DBFS);
        assert_eq!(word.clone().without_window_tail(), word);
        let long = tone_then(30, 1000, 200);
        assert_eq!(long.clone().without_window_tail(), long);
        for clip in [
            tone_then(30, 0, 200),
            tone_then(29, 900, 200),
            tone_then(0, 160, 200),
        ] {
            assert_eq!(clip.clone().without_window_tail(), clip);
        }
    }

    fn sine(rate: u32, channels: u16, secs: f32, hz: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin();
                std::iter::repeat_n(v, channels as usize)
            })
            .collect()
    }

    #[test]
    fn stereo_48k_becomes_mono_16k_of_the_same_duration() {
        let a = Audio::from_f32(&sine(48_000, 2, 1.0, 440.0, 0.5), 2, 48_000);
        assert_eq!(a.samples.len(), 16_000);
        assert!((a.duration_secs() - 1.0).abs() < 0.001);
        assert!((a.peak_dbfs() - (-6.0)).abs() < 1.0, "{}", a.peak_dbfs());
    }

    #[test]
    fn a_zero_rate_does_not_panic() {
        let a = Audio::from_f32(&[0.1, 0.2, 0.3], 1, 0);
        assert_eq!(a.samples.len(), 3);
    }

    #[test]
    fn already_16k_mono_is_untouched_in_length() {
        let a = Audio::from_f32(&sine(16_000, 1, 0.5, 440.0, 0.5), 1, 16_000);
        assert_eq!(a.samples.len(), 8_000);
    }

    #[test]
    fn peak_is_reported_in_dbfs() {
        let half = Audio::from_f32(&sine(16_000, 1, 0.1, 440.0, 0.5), 1, 16_000);
        assert!(
            (half.peak_dbfs() - (-6.0)).abs() < 0.3,
            "{}",
            half.peak_dbfs()
        );
        let silence = Audio {
            samples: vec![0; 1600],
        };
        assert!(silence.peak_dbfs() <= -120.0);
    }

    #[test]
    fn wav_is_16k_mono_16bit_pcm() {
        let a = Audio::from_f32(&sine(16_000, 1, 0.1, 440.0, 0.5), 1, 16_000);
        let bytes = a.to_wav();
        let reader = hound::WavReader::new(std::io::Cursor::new(&bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (16_000, 1, 16)
        );
        assert_eq!(reader.len() as usize, a.samples.len());
        assert_eq!(&bytes[..4], b"RIFF");
    }
}
