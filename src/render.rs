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
    let ch = spec.channels.max(1) as usize;
    let bits = spec.bits_per_sample;

    let mut out = Vec::new();
    match spec.sample_format {
        SampleFormat::Float => match bits {
            32 => mix_in(&mut out, reader.samples::<f32>(), ch),
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
                );
            }
            16 => {
                let scale = 1.0 / 32768.0;
                mix_in(
                    &mut out,
                    reader.samples::<i16>().map(|s| s.map(|v| v as f32 * scale)),
                    ch,
                );
            }
            24 | 32 => {
                let scale = 1.0 / 2_147_483_648.0;
                mix_in(
                    &mut out,
                    reader.samples::<i32>().map(|s| s.map(|v| v as f32 * scale)),
                    ch,
                );
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

/// Interleave-aware mixdown to mono. Decode errors skip the sample rather than failing the
/// whole file — a truncated tail should still render.
fn mix_in<E>(out: &mut Vec<f32>, samples: impl Iterator<Item = Result<f32, E>>, channels: usize) {
    let mut acc = 0.0f32;
    let mut n = 0usize;
    for s in samples.flatten() {
        acc += s;
        n += 1;
        if n == channels {
            out.push(acc / channels as f32);
            acc = 0.0;
            n = 0;
        }
    }
    if n > 0 {
        out.push(acc / channels as f32);
    }
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

/// Chunks of pure tail, at the engine's chunk size.
fn tail_chunks(chunk: usize, sr: u32) -> usize {
    (TAIL_SECONDS * sr as usize / chunk).max(1)
}

/// Chunks to run so every input sample is pulled through, plus the tail.
///
/// Deliberately a fixed count rather than "stop when the source runs dry": the engine pulls
/// no input at all on its first calls while the resampler primes its window, so starvation is
/// normal start-up behaviour and using it as a stop condition ended a render after one chunk.
fn total_chunks(in_frames: usize, chunk: usize, sr: u32) -> usize {
    in_frames.div_ceil(chunk) + tail_chunks(chunk, sr) + 4
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
    let mut src = SliceSource::new(&mono);
    let mut out: Vec<Frame> = Vec::with_capacity(mono.len() + chunk);
    let mut buf = [[0.0f32; 2]; MAX_CHUNK];

    let mut report = Report {
        sr,
        frames_in: mono.len(),
        ..Default::default()
    };
    // A fixed chunk count, not a starvation check. The engine pulls no input at all on
    // its first calls while the resampler primes its window, so "consumed nothing this
    // block" is normal start-up rather than the source running dry -- breaking on it used
    // to end the render after a single chunk. Instead: enough chunks to pull every input
    // sample through, plus a two-second tail so delay/reverb decay is in the file.
    let _tail_chunks = tail_chunks(chunk, sr);
    let total_chunks = total_chunks(mono.len(), chunk, sr);
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

/// One measured stage boundary.
#[derive(Clone, Debug)]
pub struct StageReport {
    pub name: String,
    pub peak: f32,
    pub rms: f32,
    pub dc: f32,
    /// Only meaningful for the built-in sine input; 0.0 when a file was used.
    pub thd: f32,
}

impl StageReport {
    /// A stage this quiet is not passing signal — it is muted, gated shut, or broken.
    pub fn quiet(&self) -> bool {
        self.peak < 1e-5
    }
}

/// The frequency the built-in trace signal is generated at.
pub const TEST_HZ: f32 = 110.0;

/// Run the engine with every stage boundary recorded, and measure each one.
///
/// This exists because a silent output cannot tell you *where* the signal died, and the
/// obvious answer ("the output is quiet") is indistinguishable from a legitimately quiet
/// patch. With no input file it generates a -20 dBFS [`TEST_HZ`] sine, so a trace always has
/// something to say and per-stage THD is meaningful: it shows the exact stage where
/// distortion appears instead of implicating the whole chain.
///
/// Pass `dir` to also write one WAV per stage, which is the difference between reading a
/// number and hearing the fuzz stage.
pub fn trace(
    input: Option<&Path>,
    preset: &Preset,
    chunk: usize,
    rate: Option<u32>,
    dir: Option<&Path>,
) -> Result<(u32, Vec<StageReport>), String> {
    let chunk = chunk.clamp(1, MAX_CHUNK);
    // A generated signal means `thd` is a real measurement; a borrowed file's fundamental is
    // unknown, so THD is left at 0 rather than invented.
    let (mono, file_sr, generated) = match input {
        Some(p) => {
            let (m, sr) = read_wav_mono(p)?;
            (m, sr, false)
        }
        None => (
            analysis::sine(48000 * 2, TEST_HZ, 48000.0, 0.1),
            48_000,
            true,
        ),
    };
    if mono.is_empty() {
        return Err("nothing to trace: the input has no samples".into());
    }
    let sr = rate.unwrap_or(file_sr).clamp(8000, 192_000);
    let (mut engine, _shared) = engine_from_preset(preset, sr as f32, chunk);
    let chunks = total_chunks(mono.len(), chunk, sr);
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
        .map(|st| StageReport {
            name: st.name.clone(),
            peak: analysis::peak(&st.data),
            rms: analysis::rms(&st.data),
            dc: analysis::mean(&st.data),
            thd: if generated {
                analysis::thd(&st.data, TEST_HZ, sr as f32)
            } else {
                0.0
            },
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
        let note = pluck(20000, 220.0, 44100.0, 300.0);
        write_test_wav(&input, &note, 44100);
        for rate in [44100u32, 48000, 96000] {
            let opts = Options {
                preset: Preset::blues(),
                chunk: 128,
                rate: Some(rate),
            };
            let report = render(&input, &output, &opts).unwrap();
            assert_eq!(report.sr, rate);
            let (rendered, sr) = read_wav_mono(&output).unwrap();
            assert_eq!(sr, rate);
            assert!(!any_non_finite(&rendered));
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
