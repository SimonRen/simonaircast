//! Measuring the real PC-to-speaker delay from a microphone recording of
//! test clicks (see `audio_capture::probe`).
//!
//! Each click is heard by the mic once from the PC's own speakers (if they
//! are audible) and once from the AirPlay speaker. With the click times
//! known ([`measure`]) we report both arrivals; for a phone recording, where
//! only the audio is known ([`measure_pairs`]), the gap between the two.

/// Envelope smoothing window.
const ENVELOPE_MS: f64 = 1.0;
/// After an onset, the envelope must stay below the threshold this long
/// before another onset counts (rides out room echo).
const REARM_MS: f64 = 30.0;
/// Onsets this close to a click are the PC's own speakers.
const PC_SPEAKER_MAX: f64 = 0.040;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub median_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub count: usize,
}

impl Summary {
    pub fn of(values_s: &[f64]) -> Option<Self> {
        if values_s.is_empty() {
            return None;
        }
        let mut v: Vec<f64> = values_s.iter().map(|s| s * 1000.0).collect();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let median_ms = if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        };
        Some(Self {
            median_ms,
            min_ms: v[0],
            max_ms: v[n - 1],
            count: n,
        })
    }
}

impl std::fmt::Display for Summary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.0} ms (range {:.0}..{:.0}, {} clicks)",
            self.median_ms, self.min_ms, self.max_ms, self.count
        )
    }
}

/// Times (s) where a click starts: the envelope rises above a threshold set
/// well over the noise floor.
pub fn onsets(samples: &[f32], rate: u32) -> Vec<f64> {
    let rate_f = f64::from(rate);
    let win = ((ENVELOPE_MS / 1000.0 * rate_f) as usize).max(1);
    // Moving average of |x| (cheap, adequate for 1 ms resolution).
    let mut env = Vec::with_capacity(samples.len());
    let mut acc = 0f32;
    for (i, s) in samples.iter().enumerate() {
        acc += s.abs();
        if i >= win {
            acc -= samples[i - win].abs();
        }
        env.push(acc / win as f32);
    }
    if env.is_empty() {
        return Vec::new();
    }
    let mut sorted = env.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let peak = sorted[sorted.len() - 1];
    // Quiet clicks still count as long as they clear the noise by far.
    let threshold = (floor * 8.0).max(peak * 0.08).max(1e-4);
    if peak <= threshold {
        return Vec::new();
    }

    let rearm = (REARM_MS / 1000.0 * rate_f) as usize;
    let mut found = Vec::new();
    let mut armed = true;
    let mut quiet = 0usize;
    for (i, &e) in env.iter().enumerate() {
        if e >= threshold {
            if armed {
                // The envelope lags the true start by about half a window.
                found.push((i.saturating_sub(win / 2)) as f64 / rate_f);
                armed = false;
            }
            quiet = 0;
        } else if !armed {
            quiet += 1;
            if quiet >= rearm {
                armed = true;
            }
        }
    }
    found
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Click to the PC's own speakers (mic and PC output latency).
    pub pc: Option<Summary>,
    /// Click to the AirPlay speaker, as the mic heard it.
    pub airplay: Option<Summary>,
    /// AirPlay speaker behind the PC speakers, from clicks heard on both:
    /// the delay a viewer actually notices against the PC.
    pub gap: Option<Summary>,
    pub clicks: usize,
}

/// Classifies onsets by the click they follow (within one `period`).
pub fn measure(onsets: &[f64], clicks: &[f64], period: f64) -> Measurement {
    let (mut pc, mut airplay, mut gap) = (Vec::new(), Vec::new(), Vec::new());
    for &c in clicks {
        let after: Vec<f64> = onsets
            .iter()
            .map(|&t| t - c)
            .filter(|&d| d > -0.010 && d < period - 0.050)
            .collect();
        let first_pc = after.first().copied().filter(|&d| d < PC_SPEAKER_MAX);
        // The AirPlay arrival is the first onset clearly after the PC one.
        let speaker = after.iter().copied().find(|&d| d >= PC_SPEAKER_MAX);
        if let Some(p) = first_pc {
            pc.push(p);
        }
        if let Some(s) = speaker {
            airplay.push(s);
            if let Some(p) = first_pc {
                gap.push(s - p);
            }
        }
    }
    Measurement {
        pc: Summary::of(&pc),
        airplay: Summary::of(&airplay),
        gap: Summary::of(&gap),
        clicks: clicks.len(),
    }
}

/// For a recording without click times (phone): pairs each onset with the
/// next one when they are closer than half a `period`; the gaps are the
/// AirPlay speaker's delay behind the PC speakers.
pub fn measure_pairs(onsets: &[f64], period: f64) -> Option<Summary> {
    let mut gaps = Vec::new();
    let mut i = 0;
    while i + 1 < onsets.len() {
        let d = onsets[i + 1] - onsets[i];
        if d < period / 2.0 {
            gaps.push(d);
            i += 2;
        } else {
            i += 1;
        }
    }
    Summary::of(&gaps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// A recording with clicks at `at` (s) of the given amplitudes, over
    /// low-level pseudo-random noise.
    fn recording(len_s: f64, at: &[(f64, f32)]) -> Vec<f32> {
        let n = (len_s * f64::from(RATE)) as usize;
        let mut seed = 1u32;
        let mut out: Vec<f32> = (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.004
            })
            .collect();
        for &(t, amp) in at {
            let start = (t * f64::from(RATE)) as usize;
            // 4 ms burst followed by a decaying 60 ms "room" tail.
            for k in 0..(0.064 * f64::from(RATE)) as usize {
                let tt = k as f32 / RATE as f32;
                let decay = if tt < 0.004 {
                    1.0
                } else {
                    (-(tt - 0.004) * 60.0).exp() * 0.5
                };
                out[start + k] += amp * decay * (2.0 * std::f32::consts::PI * 2500.0 * tt).sin();
            }
        }
        out
    }

    #[test]
    fn finds_click_onsets_within_a_millisecond() {
        let rec = recording(3.0, &[(0.5, 0.3), (1.25, 0.05), (2.0, 0.6)]);
        let found = onsets(&rec, RATE);
        assert_eq!(found.len(), 3, "{found:?}");
        for (got, want) in found.iter().zip([0.5, 1.25, 2.0]) {
            assert!((got - want).abs() < 0.001, "{got} vs {want}");
        }
    }

    #[test]
    fn separates_pc_speakers_from_the_airplay_speaker() {
        // Mic hears the PC 12 ms after each click, the HomePod 180..200 ms after.
        let clicks = [1.0, 2.5, 4.0, 5.5];
        let mut at = Vec::new();
        for (i, &c) in clicks.iter().enumerate() {
            at.push((c + 0.012, 0.4));
            at.push((c + 0.180 + 0.005 * i as f64, 0.2));
        }
        let rec = recording(7.0, &at);
        let m = measure(&onsets(&rec, RATE), &clicks, 1.5);
        assert_eq!(m.pc.unwrap().count, 4);
        assert!((m.pc.unwrap().median_ms - 12.0).abs() < 1.5);
        let gap = m.gap.unwrap();
        assert_eq!(gap.count, 4);
        assert!((gap.median_ms - 175.5).abs() < 1.5, "{gap:?}");
        assert!((m.airplay.unwrap().max_ms - 195.0).abs() < 1.5);
    }

    #[test]
    fn airplay_only_when_pc_speakers_are_silent() {
        let clicks = [1.0, 2.5];
        let rec = recording(4.0, &[(1.3, 0.3), (2.8, 0.3)]);
        let m = measure(&onsets(&rec, RATE), &clicks, 1.5);
        assert_eq!(m.pc, None);
        assert_eq!(m.gap, None);
        assert!((m.airplay.unwrap().median_ms - 300.0).abs() < 1.5);
    }

    #[test]
    fn pairs_phone_recording_onsets() {
        let onsets = [0.4, 0.62, 1.9, 2.11, 3.4, 4.9, 5.12];
        let s = measure_pairs(&onsets, 1.5).unwrap();
        assert_eq!(s.count, 3);
        assert!((s.median_ms - 220.0).abs() < 0.5);
    }

    #[test]
    fn silence_has_no_onsets() {
        assert!(onsets(&vec![0.0; 48_000], RATE).is_empty());
        assert!(onsets(&recording(1.0, &[]), RATE).len() <= 1);
    }
}
