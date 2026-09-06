//! Offline rendering: run a WAV file through the exact same engine the live path uses.
//!
//! This exists so DSP changes can be heard and measured without a cable, an interface, or
//! a room. It is deliberately the same `Engine`, the same chunk loop and the same command
//! mailbox as the audio thread — a render that passed a bypassed or simplified path would
//! verify nothing.

use std::path::Path;

use hound::{SampleFormat, WavReader, WavWriter};

use crate::dsp::resampler::SliceSource;
use crate::dsp::{analysis, Frame, MAX_CHUNK};
use crate::engine::{Cmd, Engine, Flag, Shared};
use crate::preset::Preset;

/// What a render produced, for the CLI to print and for tests to assert on.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    pub sr: u32,
    pub frames_in: usize,
    pub frames_out: usize,
    pub peak: f32,
    /// Output samples that hit the limiter ceiling.
    pub limited: usize,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub preset: Preset,
    pub chunk: usize,
    /// Force an engine rate instead of using the file's own.
    pub rate: Option<u32>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            preset: Preset::blues(),
            chunk: 256,
            rate: None,
        }
    }
}

/// Build an engine plus its mailbox from a preset. Shared with the live audio path so a
/// preset loads identically in both.
pub fn engine_from_preset(
    preset: &Preset,
    sr: f32,
    chunk: usize,
) -> (Engine, std::sync::Arc<Shared>) {
    let shared = Shared::new();
    let mut engine = Engine::new(sr, chunk);
    engine.load_rack(preset.to_slots(sr));
    for (i, n) in preset.amp_norms().iter().enumerate() {
        engine.set_amp_param(i, *n);
    }
    // Routed through the mailbox rather than a direct call, so the render exercises the
    // same delivery path the UI uses.
    shared.send(Cmd::Flag(Flag::Cab, preset.cab));
    shared.send(Cmd::Flag(Flag::InputHpf, preset.hpf));
    // Drain now so the first rendered chunk already has the switches set.
    engine.process(
        Some(&shared),
        &mut SliceSource::new(&[]),
        &mut [[0.0; 2]; 1],
    );
    (engine, shared)
}

/// Decode a WAV to mono f32 in roughly `-1..=1`.
pub fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let mut reader =
        WavReader::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let spec = reader.spec();
    let sr = spec.sample_rate;
    if spec.channels == 0 || !(8000..=192000).contains(&sr) {
        return Err(format!(
            "unsupported WAV format: {} channels at {sr} Hz",
            spec.channels
        ));
    }
    let ch = spec.channels as usize;
    let bits = spec.bits_per_sample;

    let mut out = Vec::new();
    match spec.sample_format {
        SampleFormat::Float => match bits {
            32 => mix_in(&mut out, reader.samples::<f32>(), ch)?,
            other => {
                return Err(format!(
                    "unsupported float depth {other}-bit in {}",
                    path.display()
                ))
            }
        },
        SampleFormat::Int => match bits {
            8 => {
                let scale = 1.0 / 128.0;
                mix_in(
                    &mut out,
                    reader.samples::<i8>().map(|s| s.map(|v| v as f32 * scale)),
                    ch,
                )?;
            }
            16 => {
                let scale = 1.0 / 32768.0;
                mix_in(
                    &mut out,
                    reader.samples::<i16>().map(|s| s.map(|v| v as f32 * scale)),
                    ch,
                )?;
            }
            24 | 32 => {
                // Hound decodes integers at their native bit depth, not left-aligned.
                // https://docs.rs/hound/3.5.1/hound/trait.Sample.html
                let scale = 1.0 / (1_u64 << (bits - 1)) as f32;
                mix_in(
                    &mut out,
                    reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)),
                    ch,
                )?;
            }
            other => {
                return Err(format!(
                    "unsupported bit depth {other} in {}",
                    path.display()
                ))
            }
        },
    }
    Ok((out, sr))
}

/// Decode failures must not silently shift interleaved channel alignment.
fn mix_in<E: std::fmt::Display>(
    out: &mut Vec<f32>,
    samples: impl Iterator<Item = Result<f32, E>>,
    channels: usize,
) -> Result<(), String> {
    let mut acc = 0.0f64;
    let mut n = 0usize;
    for s in samples {
        let s = s.map_err(|e| format!("WAV decode failed: {e}"))?;
        if !s.is_finite() {
            return Err("WAV contains a non-finite sample".into());
        }
        acc += s as f64;
        n += 1;
        if n == channels {
            out.push((acc / channels as f64) as f32);
            acc = 0.0;
            n = 0;
        }
    }
    if n != 0 {
        return Err("WAV ends with an incomplete channel frame".into());
    }
    Ok(())
}

/// Write stereo f32 frames as an interleaved WAV.
pub fn write_wav_stereo(path: &Path, frames: &[Frame], sr: u32) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: sr,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(path, spec)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    for f in frames {
        writer
            .write_sample(f[0])
            .map_err(|e| format!("write failed: {e}"))?;
        writer
            .write_sample(f[1])
            .map_err(|e| format!("write failed: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("finalize failed: {e}"))?;
    Ok(())
}

/// Render `input` through `opts` and write the result. Returns the report either way.
/// Seconds of deliberate input-less tail so delay and reverb decay land in the file.
const TAIL_SECONDS: usize = 2;

/// Output-clock duration plus tail, rounded once to the processing block boundary.
fn total_chunks(in_frames: usize, input_sr: u32, chunk: usize, sr: u32) -> usize {
    let frames = (in_frames as u64 * sr as u64).div_ceil(input_sr as u64) as usize;
    (frames + TAIL_SECONDS * sr as usize).div_ceil(chunk)
}

pub fn render(input: &Path, output: &Path, opts: &Options) -> Result<Report, String> {
    let (mono, file_sr) = read_wav_mono(input)?;
    if mono.is_empty() {
        return Err(format!("{} contains no samples", input.display()));
    }
    let sr = opts.rate.unwrap_or(file_sr).clamp(8000, 192000);
    let chunk = opts.chunk.clamp(1, MAX_CHUNK);

    // The mailbox is drained inside `engine_from_preset`; held alive here so the
    // engine's switch state stays exactly as the preset set it.
    let (mut engine, _shared) = engine_from_preset(&opts.preset, sr as f32, chunk);
    engine.set_rates(file_sr as f32, sr as f32);
    let mut src = SliceSource::new(&mono);
    let mut out: Vec<Frame> = Vec::with_capacity(mono.len() + chunk);
    let mut buf = [[0.0f32; 2]; MAX_CHUNK];

    let mut report = Report {
        sr,
        frames_in: mono.len(),
        ..Default::default()
    };
    // Include every resampled input frame and the delay/reverb tail.
    let total_chunks = total_chunks(mono.len(), file_sr, chunk, sr);
    for _ in 0..total_chunks {
        engine.process(None, &mut src, &mut buf[..chunk]);
        for f in buf[..chunk].iter() {
            report.peak = report.peak.max(f[0].abs().max(f[1].abs()));
            if f[0].abs() > 0.949 || f[1].abs() > 0.949 {
                report.limited += 1;
            }
        }
        out.extend_from_slice(&buf[..chunk]);
    }
    report.frames_out = out.len();

    write_wav_stereo(output, &out, sr)?;
    Ok(report)
}

/// Built-in trace input. [`TraceProbe::Sine`] preserves the original trace signal.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TraceProbe {
    #[default]
    Sine,
    Tail,
}

/// One measured stage boundary.
#[derive(Clone, Debug)]
pub struct StageReport {
    pub name: String,
    pub peak: f32,
    pub rms: f32,
    /// Left DC, retained for compatibility.
    pub dc: f32,
    pub dc_right: f32,
    /// Only meaningful for the built-in sine input; 0.0 for Tail and WAV input.
    pub thd: f32,
    pub non_finite: bool,
    /// RMS after the excitation; present only for [`TraceProbe::Tail`].
    pub tail_early_rms: Option<f32>,
    pub tail_final_rms: Option<f32>,
    /// Final-window energy relative to early-tail energy, in dB.
    pub tail_decay_db: Option<f32>,
}

impl StageReport {
    /// A stage this quiet is not passing signal — it is muted, gated shut, or broken.
    pub fn quiet(&self) -> bool {
        self.peak < 1e-5
    }

    /// An energetic tail that is flat or growing is a feedback/decay fault.
    pub fn tail_stalled(&self) -> bool {
        self.tail_final_rms.is_some_and(|rms| rms >= 1e-4)
            && self
                .tail_decay_db
                .is_some_and(|db| db.is_finite() && db >= 0.0)
    }

    pub fn failed(&self) -> bool {
        self.non_finite || self.quiet() || self.tail_stalled()
    }
}

/// The frequency the built-in trace signal is generated at.
pub const TEST_HZ: f32 = 110.0;

/// Two seconds with three short bursts at the front; the existing render tail follows it.
fn tail_input() -> Vec<f32> {
    let sr = 48_000;
    let mut input = vec![0.0; sr * 2];
    for (start, hz) in [(0, 110.0), (sr * 2 / 25, 440.0), (sr * 4 / 25, 1760.0)] {
        let burst = analysis::sine(sr * 3 / 50, hz, sr as f32, 0.1);
        input[start..start + burst.len()].copy_from_slice(&burst);
    }
    input
}

fn stereo_peak(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .chain(right)
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

fn stereo_rms(left: &[f32], right: &[f32]) -> f32 {
    let n = left.len() + right.len();
    if n == 0 {
        return 0.0;
    }
    let energy: f64 = left
        .iter()
        .chain(right)
        .map(|sample| (*sample as f64).powi(2))
        .sum();
    (energy / n as f64).sqrt() as f32
}

fn tail_metric(left: &[f32], right: &[f32], sr: u32) -> (Option<f32>, Option<f32>, Option<f32>) {
    let window = sr as usize * 2;
    let early = window;
    let end = left.len().min(right.len());
    if window == 0 || end < window * 2 || early + window > end {
        return (None, None, None);
    }
    // A maximum reverse-delay window unfolds from 2–4 s, so compare that whole first
    // return with the final 4–6 s. Broad windows cannot land between sparse delay repeats.
    let early_rms = stereo_rms(&left[early..early + window], &right[early..early + window]);
    let final_rms = stereo_rms(&left[end - window..end], &right[end - window..end]);
    (
        Some(early_rms),
        Some(final_rms),
        Some(20.0 * (final_rms / early_rms.max(f32::MIN_POSITIVE)).log10()),
    )
}

/// Run the engine with every stage boundary recorded, and measure each one.
///
/// This preserves the original sine trace API. Use [`trace_with_probe`] to choose a different
/// deterministic built-in input.
pub fn trace(
    input: Option<&Path>,
    preset: &Preset,
    chunk: usize,
    rate: Option<u32>,
    dir: Option<&Path>,
) -> Result<(u32, Vec<StageReport>), String> {
    trace_with_probe(input, preset, chunk, rate, dir, TraceProbe::Sine)
}

/// Like [`trace`], with an explicit built-in probe when `input` is absent.
pub fn trace_with_probe(
    input: Option<&Path>,
    preset: &Preset,
    chunk: usize,
    rate: Option<u32>,
    dir: Option<&Path>,
    probe: TraceProbe,
) -> Result<(u32, Vec<StageReport>), String> {
    let chunk = chunk.clamp(1, MAX_CHUNK);
    let (mono, file_sr, sine, tail) = match input {
        Some(p) => {
            let (m, sr) = read_wav_mono(p)?;
            (m, sr, false, false)
        }
        None => match probe {
            TraceProbe::Sine => (
                analysis::sine(48_000 * 2, TEST_HZ, 48_000.0, 0.1),
                48_000,
                true,
                false,
            ),
            TraceProbe::Tail => (tail_input(), 48_000, false, true),
        },
    };
    if mono.is_empty() {
        return Err("nothing to trace: the input has no samples".into());
    }
    let sr = rate.unwrap_or(file_sr).clamp(8000, 192_000);
    let (mut engine, _shared) = engine_from_preset(preset, sr as f32, chunk);
    engine.set_rates(file_sr as f32, sr as f32);
    let chunks = total_chunks(mono.len(), file_sr, chunk, sr)
        + if tail {
            (2 * sr as usize).div_ceil(chunk)
        } else {
            0
        };
    engine.enable_taps(chunks * chunk);

    let mut src = SliceSource::new(&mono);
    let mut buf = [[0.0f32; 2]; MAX_CHUNK];
    for _ in 0..chunks {
        engine.process(None, &mut src, &mut buf[..chunk]);
    }

    let taps = engine.taps().expect("taps were just enabled");
    let reports = taps
        .stages
        .iter()
        .map(|st| {
            let (tail_early_rms, tail_final_rms, tail_decay_db) = if tail {
                tail_metric(&st.data, &st.right, sr)
            } else {
                (None, None, None)
            };
            StageReport {
                name: st.name.clone(),
                peak: stereo_peak(&st.data, &st.right),
                rms: stereo_rms(&st.data, &st.right),
                dc: analysis::mean(&st.data),
                dc_right: analysis::mean(&st.right),
                thd: if sine {
                    analysis::thd(&st.data, TEST_HZ, sr as f32)
                } else {
                    0.0
                },
                non_finite: st
                    .data
                    .iter()
                    .chain(&st.right)
                    .any(|sample| !sample.is_finite()),
                tail_early_rms,
                tail_final_rms,
                tail_decay_db,
            }
        })
        .collect();
    if let Some(d) = dir {
        taps.write_wavs(d, sr)
            .map_err(|e| format!("could not write stage files: {e}"))?;
    }
    Ok((sr, reports))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::{any_non_finite, mean, peak, pluck, rms};
    use crate::params::amp_ix;

    #[test]
    fn pcm_bit_depths_preserve_quarter_scale() {
        let dir = std::env::temp_dir().join(format!("triode-pcm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for bits in [16, 24, 32] {
            let path = dir.join(format!("{bits}.wav"));
            let spec = hound::WavSpec {
                channels: 1,
                sample_rate: 48000,
                bits_per_sample: bits,
                sample_format: SampleFormat::Int,
            };
            let mut writer = WavWriter::create(&path, spec).unwrap();
            writer.write_sample(1_i32 << (bits - 3)).unwrap();
            writer.finalize().unwrap();
            let (samples, _) = read_wav_mono(&path).unwrap();
            assert_eq!(samples, vec![0.25], "{bits}-bit scaling");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tail_probe_is_deterministic_finite_and_two_seconds_long() {
        let first = tail_input();
        assert_eq!(first, tail_input());
        assert_eq!(first.len(), 48_000 * 2);
        assert!(first.iter().all(|sample| sample.is_finite()));
        assert!(first.iter().any(|sample| *sample != 0.0));
        assert!(first[48_000 / 4..].iter().all(|sample| *sample == 0.0));

        let (_, stages) = trace_with_probe(
            None,
            &Preset::empty(),
            128,
            Some(48_000),
            None,
            TraceProbe::Tail,
        )
        .unwrap();
        assert!(stages
            .iter()
            .all(|stage| !stage.non_finite && stage.thd == 0.0));
        assert!(stages.iter().all(|stage| stage.tail_decay_db.is_some()));
    }

    #[test]
    fn anti_phase_stereo_has_energy() {
        assert_eq!(stereo_peak(&[0.5], &[-0.5]), 0.5);
        assert!((stereo_rms(&[0.5], &[-0.5]) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn an_energetic_non_decaying_tail_is_a_failure() {
        let stage = StageReport {
            name: "test".into(),
            peak: 0.1,
            rms: 0.1,
            dc: 0.0,
            dc_right: 0.0,
            thd: 0.0,
            non_finite: false,
            tail_early_rms: Some(0.1),
            tail_final_rms: Some(0.1),
            tail_decay_db: Some(0.0),
        };
        assert!(stage.tail_stalled());
        assert!(stage.failed());
    }

    #[test]
    fn a_late_tail_is_a_failure_even_if_the_early_window_was_silent() {
        let stage = StageReport {
            name: "late echo".into(),
            peak: 0.1,
            rms: 0.01,
            dc: 0.0,
            dc_right: 0.0,
            thd: 0.0,
            non_finite: false,
            tail_early_rms: Some(0.0),
            tail_final_rms: Some(0.01),
            tail_decay_db: Some(120.0),
        };
        assert!(stage.tail_stalled());
    }

    #[test]
    fn increased_delay_feedback_leaves_more_tail_energy_without_instability() {
        let trace_delay = |feedback| {
            let mut preset = Preset::empty();
            preset.slots.push(crate::preset::SlotPreset::with_values(
                crate::params::EffectKind::Delay,
                &[200.0, feedback, 0.8, 3400.0],
            ));
            trace_with_probe(None, &preset, 128, Some(48_000), None, TraceProbe::Tail)
                .unwrap()
                .1
                .pop()
                .unwrap()
        };
        let low = trace_delay(0.1);
        let high = trace_delay(0.7);
        assert!(
            high.tail_early_rms.unwrap() > low.tail_early_rms.unwrap() * 2.0,
            "feedback did not increase tail energy: {:?} -> {:?}",
            low.tail_early_rms,
            high.tail_early_rms
        );
        assert!(!high.non_finite);
    }

    #[test]
    fn converted_trace_keeps_the_generated_fundamental() {
        for sr in [44100, 48000, 96000] {
            let (_, stages) = trace(None, &Preset::empty(), 128, Some(sr), None).unwrap();
            assert!(
                stages[0].thd < 0.02,
                "input fundamental moved at {sr}: {}",
                stages[0].thd
            );
        }
    }

    /// The whole point of tap points: a quiet output is not evidence of where it went quiet,
    /// so assert on every boundary of the demo patch instead of only the last one. A stage
    /// that stops carrying signal (a filter designed at the wrong rate, a shaper that
    /// saturates on its own bias, a bypass that swallows the buffer) fails here rather than
    /// in someone's ears.
    #[test]
    fn every_stage_of_the_demo_patch_carries_signal() {
        let (sr, stages) = trace(None, &Preset::blues(), 256, Some(48_000), None).expect("trace");
        // input + four demo slots + amp + cab + out
        assert_eq!(
            stages.len(),
            8,
            "stage list: {:?}",
            stages.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        for st in &stages {
            assert!(
                !st.quiet(),
                "{} carries no signal (peak {:.2e}, rms {:.2e})",
                st.name,
                st.peak,
                st.rms
            );
        }
        let out = stages.last().unwrap();
        assert!(out.dc.abs() < 1e-3, "dc reached the output: {:.2e}", out.dc);
        // `CEILING` (0.95) is what the limiter aims at, and its finite attack overshoots it;
        // `FULL_SCALE` is what the engine guarantees. Assert the guarantee, not the aim.
        assert!(
            out.peak <= crate::engine::FULL_SCALE,
            "output passed full scale: {}",
            out.peak
        );
        let _ = sr;
    }

    /// The `input` stage has to be the device's level, not ours. It is the only reading that
    /// answers "is a guitar open and plugged in", and it was wrong twice in one sitting: once
    /// recorded after the input trim (so a +36 dB trim made a quiet input look hot), and once
    /// a whole block late. `Preset::blues()` runs the trim at +36 dB, so pre/post is a 63x
    /// difference and easy to pin.
    #[test]
    fn the_input_stage_reads_the_device_level_not_ours() {
        let (_, stages) = trace(None, &Preset::blues(), 256, Some(48_000), None).expect("trace");
        let (input, first_slot) = (&stages[0], &stages[1]);
        // The generated signal is 0.1 amplitude; anything else means the tap is downstream of
        // something we control.
        assert!(
            (input.peak - 0.1).abs() < 1e-3,
            "input stage should read the raw 0.1 amplitude, got {}",
            input.peak
        );
        assert!(
            first_slot.peak > input.peak * 2.0,
            "input trim did not register between the input and the first slot ({} -> {})",
            input.peak,
            first_slot.peak
        );
    }

    /// A trace is also the tool that answers "did the rack change anything at all", so the
    /// stages must not all be identical copies of the input.
    #[test]
    fn the_rack_actually_changes_the_signal() {
        let (_, stages) = trace(None, &Preset::blues(), 256, Some(48_000), None).expect("trace");
        let input = &stages[0];
        let overdrive = stages
            .iter()
            .find(|s| s.name.contains("OD"))
            .expect("an OD stage");
        assert!(
            overdrive.thd > input.thd + 0.01,
            "overdrive did not add distortion: in {:.3}, od {:.3}",
            input.thd,
            overdrive.thd
        );
    }
    use std::fs;

    fn write_test_wav(path: &Path, mono: &[f32], sr: u32) {
        // Reuse the stereo writer with both channels equal; enough for a render input.
        let frames: Vec<Frame> = mono.iter().map(|s| [*s, *s]).collect();
        write_wav_stereo(path, &frames, sr).unwrap();
    }

    #[test]
    fn a_plucked_string_renders_silence_to_a_starting_level_then_decays() {
        let dir = std::env::temp_dir().join(format!("triode-render-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        let output = dir.join("out.wav");
        let note = pluck(48000, 110.0, 48000.0, 600.0);
        write_test_wav(&input, &note, 48000);

        let opts = Options {
            preset: Preset::empty(),
            chunk: 128,
            rate: None,
        };
        let report = render(&input, &output, &opts).expect("render");

        assert_eq!(report.sr, 48000);
        assert!(
            report.frames_out > note.len(),
            "output should include engine latency padding"
        );
        let (rendered, _) = read_wav_mono(&output).unwrap();
        assert!(!any_non_finite(&rendered));
        // The empty preset is unity through the amp, so a 0.5-peak input must still be
        // audible; a render of silence is the classic silent-regression bug.
        assert!(
            peak(&rendered[2000..]) > 0.05,
            "render produced near-silence: {}",
            peak(&rendered)
        );
        assert!(
            report.peak <= 1.0 + 1e-6,
            "limiter ceiling breached: {}",
            report.peak
        );
        // Decays: the end is quieter than the start.
        assert!(rms(&rendered[rendered.len() - 5000..]) < rms(&rendered[5000..10000]));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hot_input_cannot_exceed_the_ceiling() {
        let dir = std::env::temp_dir().join(format!("triode-hot-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        let output = dir.join("out.wav");
        // Deliberately clipping-level input plus a preset that adds gain.
        let hot: Vec<f32> = (0..36000).map(|i| (i as f32 * 0.05).sin() * 3.0).collect();
        write_test_wav(&input, &hot, 48000);

        let mut preset = Preset::blues();
        // Max the knobs that actually create level. This used to lift `master` only, which
        // "passed" while `set_amp_param` was double-converting the curve and inflating every
        // stage ~50x, so the limiter was engaged by the bug rather than by the input. With
        // honest gain staging, a ceiling test has to genuinely ask for gain.
        for i in [amp_ix::GAIN, amp_ix::MASTER, amp_ix::TRIM] {
            preset.amp[i] = 1.0;
        }
        let opts = Options {
            preset,
            chunk: 64,
            rate: None,
        };
        let report = render(&input, &output, &opts).expect("render");
        let (rendered, _) = read_wav_mono(&output).unwrap();
        assert!(!any_non_finite(&rendered));
        // What this test owns is the end-to-end guarantee: a clipping input never leaves
        // full scale. It used to also require `limited > 0`, i.e. that the limiter engage.
        // That only ever passed while `set_amp_param` was double-converting the curve and
        // inflating every stage; with honest gain staging the rack path tops out near 0.1
        // even with gain, trim and master maxed (the power-amp shaper is bounded, `makeup`
        // compensates away 70% of the gain, and the cab model takes ~18 dB more), so the
        // limiter is a backstop this path cannot reach. Its ceiling is verified directly in
        // `dsp::limiter` and the selftest, which is where a ceiling belongs.
        assert!(
            report.peak <= crate::engine::FULL_SCALE,
            "full scale breached: {}",
            report.peak
        );
        // Canary for the opposite failure, which I hit twice while fixing the above: an
        // engine that quietly stops making sound. Loud enough to be a fault, too quiet to
        // be the old inflated levels.
        assert!(
            report.peak > 0.02,
            "engine output collapsed to {:.4}",
            report.peak
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_dc_reaches_the_output() {
        let dir = std::env::temp_dir().join(format!("triode-dc-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        let output = dir.join("out.wav");
        // A square wave has real DC content once it is distorted.
        let sq: Vec<f32> = (0..48000)
            .map(|i| if (i % 200) < 100 { 0.6 } else { -0.6 })
            .collect();
        write_test_wav(&input, &sq, 48000);
        let opts = Options {
            preset: Preset::blues(),
            chunk: 256,
            rate: None,
        };
        render(&input, &output, &opts).unwrap();
        let (rendered, _) = read_wav_mono(&output).unwrap();
        let tail = &rendered[rendered.len() / 2..];
        assert!(mean(tail).abs() < 0.02, "dc at output: {}", mean(tail));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn forcing_a_different_rate_still_renders() {
        let dir = std::env::temp_dir().join(format!("triode-rate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        let output = dir.join("out.wav");
        for input_rate in [44100, 96000] {
            let note = pluck(20000, 220.0, input_rate as f32, 300.0);
            write_test_wav(&input, &note, input_rate);
            for rate in [44100u32, 48000, 96000] {
                let opts = Options {
                    preset: Preset::blues(),
                    chunk: 128,
                    rate: Some(rate),
                };
                let report = render(&input, &output, &opts).unwrap();
                assert_eq!(report.sr, rate);
                let expected_frames = (((20000.0 / input_rate as f64 + 2.0) * rate as f64 / 128.0)
                    .ceil() as usize)
                    * 128;
                assert_eq!(
                    report.frames_out, expected_frames,
                    "duration at {input_rate} -> {rate}"
                );
                let (rendered, sr) = read_wav_mono(&output).unwrap();
                assert_eq!(sr, rate);
                assert_eq!(rendered.len(), expected_frames);
                assert!(!any_non_finite(&rendered));
            }
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_and_broken_files_report_rather_than_panic() {
        let dir = std::env::temp_dir();
        let missing = dir.join("triode-definitely-not-here.wav");
        let err = render(&missing, &missing, &Options::default()).unwrap_err();
        assert!(err.contains("cannot read"), "unhelpful: {err}");

        let junk = dir.join(format!("triode-junk-{}.wav", std::process::id()));
        fs::write(&junk, b"not a wav at all").unwrap();
        let err = render(&junk, &junk, &Options::default()).unwrap_err();
        assert!(!err.is_empty());
        let _ = fs::remove_file(&junk);
    }

    #[test]
    fn a_preset_changes_the_render() {
        // Sanity check that presets actually reach the render path: a max-decay reverb
        // tail must differ from the empty rack.
        let dir = std::env::temp_dir().join(format!("triode-cmp-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.wav");
        let note = pluck(24000, 330.0, 48000.0, 60.0);
        write_test_wav(&input, &note, 48000);

        let mut wet = Preset::blues();
        wet.slots.clear();
        wet.slots.push(crate::preset::SlotPreset::with_values(
            crate::params::EffectKind::Reverb,
            &[0.9, 1.0, 1.0, 0.5],
        ));

        let render_with = |p: Preset| -> Vec<f32> {
            let out = dir.join("o.wav");
            render(
                &input,
                &out,
                &Options {
                    preset: p,
                    chunk: 128,
                    rate: None,
                },
            )
            .unwrap();
            read_wav_mono(&out).unwrap().0
        };
        let dry = render_with(Preset::empty());
        let soaked = render_with(wet);
        let late = dry.len() - 6000;
        assert!(
            rms(&soaked[late..]) > rms(&dry[late..]) * 2.0,
            "preset had no effect on render"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
