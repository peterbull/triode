//! Triode — a real-time valve amp in Rust.
//!
//! Native UI plus offline render/trace/selftest, device capture, and a no-audio preview.
//! No subcommand library: the command surface stays small and explicit.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use triode::engine::ReopenReq;
use triode::preset::Preset;
use triode::render::{self, Options};
use triode::selftest;

const HELP: &str = "\
Triode — guitar amp + stompbox rack

  triode                        open the UI (default)
  triode ui [--preset N]        same, explicitly
  triode devices                list input and output devices
  triode render IN OUT.wav      offline render (no audio device needed)
  triode selftest               run the audio sanity checks and exit
  triode preview OUT.ppm [W H]  capture the native UI without opening audio devices
  triode trace [IN.wav]         measure every stage boundary; writes per-stage WAVs
  triode capture OUT.wav        record the input device to a WAV to trace or render later

`trace` with no input file generates a -20 dBFS 110 Hz sine by default; use
`--probe tail` for a deterministic multi-band burst and decay check.

options
  --preset <name|file.json>   built-in/saved preset name, or a JSON file
  --buffer <ms>               requested audio buffer, 1..50 (default 5)
  --input  <name>             capture device (default: system default)
  --output <name>             playback device (default: system default)
  --rate <hz>                 render: force this sample rate
  --chunk <frames>            render/trace: engine chunk size, <=512 (default 256)
  --probe <sine|tail>         trace: built-in input (cannot combine with IN.wav)
  --input-on              arm the input at startup (default: output only, no howl)
  --seconds <n>               capture: how long to record (default 6)

start with --preset blues (or no preset at all) and the test tone in the UI's
wave panel, if you have no guitar to hand.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("triode: {e}");
            ExitCode::from(2)
        }
    }
}

/// Parsed options. Hand-rolled: eight flags do not warrant a dependency.
#[derive(Clone, Debug)]
struct Opts {
    rest: Vec<String>,
    preset: Option<String>,
    input: Option<String>,
    output: Option<String>,
    buffer_ms: u32,
    rate: Option<u32>,
    chunk: usize,
    secs: f32,
    probe: Option<render::TraceProbe>,
    input_on: bool,
    help: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Result<Opts, String> {
        let mut o = Opts {
            rest: Vec::new(),
            preset: None,
            input: None,
            output: None,
            buffer_ms: 5,
            rate: None,
            chunk: 256,
            secs: 6.0,
            probe: None,
            input_on: false,
            help: false,
        };
        let mut it = args.iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--preset" => o.preset = Some(take(&mut it, "--preset")?),
                "--input-on" => o.input_on = true,
                "--input" => {
                    o.input = Some(take(&mut it, "--input")?);
                    // Naming an input device is explicit intent to use it.
                    o.input_on = true;
                }
                "--output" => o.output = Some(take(&mut it, "--output")?),
                "--buffer" => {
                    let v: u32 = take(&mut it, "--buffer")?
                        .parse()
                        .map_err(|_| "--buffer needs a number")?;
                    o.buffer_ms = v.clamp(1, 50);
                }
                "--rate" => {
                    let v: u32 = take(&mut it, "--rate")?
                        .parse()
                        .map_err(|_| "--rate needs a number")?;
                    o.rate = Some(v);
                }
                "--seconds" => {
                    let v: f32 = take(&mut it, "--seconds")?
                        .parse()
                        .map_err(|_| "--seconds needs a number")?;
                    if !v.is_finite() {
                        return Err("--seconds needs a finite duration".into());
                    }
                    o.secs = v.clamp(0.2, 600.0);
                }
                "--chunk" => {
                    let v: usize = take(&mut it, "--chunk")?
                        .parse()
                        .map_err(|_| "--chunk needs a number")?;
                    o.chunk = v;
                }
                "--probe" => {
                    o.probe = Some(match take(&mut it, "--probe")?.as_str() {
                        "sine" => render::TraceProbe::Sine,
                        "tail" => render::TraceProbe::Tail,
                        other => {
                            return Err(format!("unknown trace probe {other:?} (use sine or tail)"))
                        }
                    });
                }
                "-h" | "--help" | "help" => o.help = true,
                s if s.starts_with('-') => return Err(format!("unknown option {s}")),
                s => o.rest.push(s.to_string()),
            }
        }
        Ok(o)
    }

    fn req(&self) -> ReopenReq {
        ReopenReq {
            input: self.input.clone(),
            output: self.output.clone(),
            buffer_ms: self.buffer_ms,
            input_on: self.input_on,
        }
    }
}

fn run(args: &[String]) -> Result<u8, String> {
    let o = Opts::parse(args)?;
    if o.help {
        print!("{HELP}");
        return Ok(0);
    }
    let command = o.rest.first().map(String::as_str).unwrap_or("ui");
    if o.probe.is_some() && command != "trace" {
        return Err("--probe is only valid with trace".into());
    }
    match command {
        "ui" => {
            let preset = preset_for(&o)?;
            triode::ui::App::launch(preset, o.req())?;
            Ok(0)
        }
        "preview" => {
            if o.rest.len() != 2 && o.rest.len() != 4 {
                return Err("preview needs OUT.ppm and optional width height".into());
            }
            let mut size = [1180.0, 780.0];
            if o.rest.len() == 4 {
                for (i, value) in o.rest[2..].iter().enumerate() {
                    let pixels: u16 = value
                        .parse()
                        .map_err(|_| "preview dimensions must be integers")?;
                    let min = [880, 560][i];
                    if !(min..=3840).contains(&pixels) {
                        return Err(format!("preview dimension must be {min}..3840 pixels"));
                    }
                    size[i] = pixels as f32;
                }
            }
            triode::ui::App::preview(preset_for(&o)?, PathBuf::from(&o.rest[1]), size)?;
            Ok(0)
        }
        "devices" => {
            let (i, out) = triode::audio_io::devices();
            let (di, dout) = triode::audio_io::defaults();
            println!("inputs:");
            for d in &i {
                println!(
                    "  {}{}",
                    if di.as_deref() == Some(d) { "* " } else { "  " },
                    d
                );
            }
            println!("outputs:");
            for d in &out {
                println!(
                    "  {}{}",
                    if dout.as_deref() == Some(d) {
                        "* "
                    } else {
                        "  "
                    },
                    d
                );
            }
            if i.is_empty() && out.is_empty() {
                println!("(none — no audio device was found)");
            }
            Ok(0)
        }
        "render" => {
            let rest = &o.rest[1..];
            if rest.len() < 2 {
                return Err("render needs an input WAV and an output WAV".into());
            }
            let opts = Options {
                preset: preset_for(&o)?,
                chunk: o.chunk,
                rate: o.rate,
            };
            let r = render::render(Path::new(&rest[0]), Path::new(&rest[1]), &opts)?;
            println!(
                "{} → {}  ·  {} Hz  ·  {} in / {} out  ·  peak {:.3}  ·  {} limited",
                rest[0], rest[1], r.sr, r.frames_in, r.frames_out, r.peak, r.limited
            );
            // A render that clipped the ceiling is worth a non-zero exit: scripts use it.
            Ok(if r.limited > 0 { 1 } else { 0 })
        }
        "trace" => {
            let input = o.rest.get(1).map(Path::new);
            if input.is_some() && o.probe.is_some() {
                return Err("trace cannot combine IN.wav with --probe".into());
            }
            let dir = PathBuf::from("target/trace");
            let (sr, stages) = render::trace_with_probe(
                input,
                &preset_for(&o)?,
                o.chunk,
                o.rate,
                Some(&dir),
                o.probe.unwrap_or_default(),
            )?;
            println!(
                "{:<12} {:>8} {:>8} {:>8} {:>10} {:>10} {:>9} {:>8}  issue",
                "stage", "peak", "rms", "ΔdB", "dc L", "dc R", "tail dB", "thd"
            );
            let mut previous_rms: Option<f32> = None;
            for st in &stages {
                let delta =
                    previous_rms.map(|rms| 20.0 * (st.rms / rms.max(f32::MIN_POSITIVE)).log10());
                let tail = st
                    .tail_decay_db
                    .map_or_else(|| "-".into(), |db| format!("{db:.1}"));
                let mut issues = Vec::new();
                if st.non_finite {
                    issues.push("NON-FINITE");
                }
                if st.quiet() {
                    issues.push("NO SIGNAL");
                }
                if st.tail_stalled() {
                    issues.push("TAIL NOT DECAYING");
                }
                println!(
                    "{:<12} {:>8.4} {:>8.4} {:>8} {:>+10.2e} {:>+10.2e} {:>9} {:>8.3}  {}",
                    st.name,
                    st.peak,
                    st.rms,
                    delta.map_or_else(|| "-".into(), |db| format!("{db:+.1}")),
                    st.dc,
                    st.dc_right,
                    tail,
                    st.thd,
                    issues.join(", ")
                );
                previous_rms = Some(st.rms);
            }
            println!("\n{} Hz · stage WAVs in {}", sr, dir.display());
            Ok(if stages.iter().any(render::StageReport::failed) {
                1
            } else {
                0
            })
        }
        "capture" => {
            let rest = &o.rest[1..];
            if rest.is_empty() {
                return Err("capture needs an output WAV, e.g. `triode capture guitar.wav`".into());
            }
            let secs = o.secs;
            let frames = triode::audio_io::capture(Path::new(&rest[0]), &o.req(), secs)?;
            println!("recorded {} frames to {}", frames, rest[0]);
            Ok(0)
        }
        "selftest" => {
            let checks = selftest::run();
            let mut bad = 0usize;
            for c in &checks {
                if !c.ok {
                    bad += 1;
                }
                println!(
                    "{} {:<28} {}",
                    if c.ok { "ok  " } else { "FAIL" },
                    c.name,
                    c.detail
                );
            }
            println!("\n{}/{} checks passed", checks.len() - bad, checks.len());
            Ok(if bad == 0 { 0 } else { 1 })
        }
        other => Err(format!(
            "unknown command {other} — try ui, render, selftest or devices"
        )),
    }
}

/// Consume the next argument as a flag's value.
fn take(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value"))
}

/// Where presets live and how `--preset` resolves: a path wins, a name is looked up in the
/// preset directory, and nothing at all falls back to the demo patch.
fn preset_for(o: &Opts) -> Result<Preset, String> {
    let Some(name) = &o.preset else {
        return Ok(Preset::blues());
    };
    let dir = triode::preset::preset_dir();
    let path = PathBuf::from(name);
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("json"))
        && path.exists()
    {
        return Preset::load_from(&path);
    }
    // Accept "foo", "foo.json" and a bare name alike.
    for cand in [name.clone(), format!("{name}.json")] {
        let p = dir.join(&cand);
        if p.exists() {
            return Preset::load_from(&p);
        }
    }
    // Explicit paths and saved patches win; built-ins always work on a fresh checkout.
    match name.as_str() {
        "blues" => return Ok(Preset::blues()),
        "empty" => return Ok(Preset::empty()),
        _ => {}
    }
    let avail = Preset::list(&dir);
    Err(format!(
        "no preset {name:?} in {} (have: {})",
        dir.display(),
        if avail.is_empty() {
            "none yet — save one from the UI".into()
        } else {
            avail.join(", ")
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_presets_are_available_without_saved_files() {
        for name in ["blues", "empty"] {
            let opts = Opts::parse(&["--preset".into(), name.into()]).unwrap();
            assert_eq!(preset_for(&opts).unwrap().name, name);
        }
    }

    #[test]
    fn trace_probe_parses_and_rejects_unknown_names() {
        let tail = Opts::parse(&["trace".into(), "--probe".into(), "tail".into()]).unwrap();
        assert_eq!(tail.probe, Some(render::TraceProbe::Tail));
        assert!(Opts::parse(&["trace".into(), "--probe".into(), "noise".into()]).is_err());
    }

    #[test]
    fn non_trace_commands_reject_probe() {
        let err = run(&[
            "render".into(),
            "in.wav".into(),
            "out.wav".into(),
            "--probe".into(),
            "tail".into(),
        ])
        .unwrap_err();
        assert!(err.contains("only valid with trace"));
    }

    #[test]
    fn trace_rejects_a_wav_with_an_explicit_probe() {
        let err = run(&[
            "trace".into(),
            "input.wav".into(),
            "--probe".into(),
            "tail".into(),
        ])
        .unwrap_err();
        assert!(err.contains("cannot combine"));
    }

    #[test]
    fn capture_duration_must_be_finite() {
        for value in ["NaN", "inf", "-inf"] {
            assert!(Opts::parse(&["--seconds".into(), value.into()]).is_err());
        }
    }
}
