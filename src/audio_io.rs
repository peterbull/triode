//! Live audio I/O: two cpal streams around one [`Engine`].
//!
//! Shape of it: the *input* callback converts whatever the device hands us to mono f32 and
//! pushes it into a drop-if-full ring; the *output* callback runs the whole rack and pulls
//! from that ring. Nothing else crosses the threads except [`Shared`], which is why the UI
//! can rebuild the rack mid-note without the audio thread ever waiting on it.
//!
//! Two clocks, not one: input and output are usually different devices (an interface in,
//! the laptop out) and they drift apart. That is expected and absorbed by the engine's
//! input resampler -- [`crate::engine::StatsSnap::ratio`] is how you watch it.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

use crate::dsp::resampler::Source;
use crate::dsp::ring::{Cons, Prod, Ring};
use crate::dsp::MAX_CHUNK;
use crate::engine::{Engine, ReopenReq, Shared};

/// How long the input ring holds: long enough to ride out a scheduler hiccup, short enough
/// that a dead input reads as silence rather than as a delay that keeps growing.
const RING_MS: f32 = 250.0;

/// Input and output device names, `(inputs, outputs)`.
pub fn devices() -> (Vec<String>, Vec<String>) {
    let host = cpal::default_host();
    (
        host.input_devices().map(names).unwrap_or_default(),
        host.output_devices().map(names).unwrap_or_default(),
    )
}

fn names<I: Iterator<Item = cpal::Device>>(it: I) -> Vec<String> {
    it.filter_map(|d| name_of(&d)).collect()
}

/// cpal 0.18 puts the label behind `description()`; one helper so the fallback is one place.
fn name_of(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|d| d.name().to_string())
}

/// `(default input, default output)`, for pre-selecting the UI's device pickers.
pub fn defaults() -> (Option<String>, Option<String>) {
    let host = cpal::default_host();
    (
        host.default_input_device().and_then(|d| name_of(&d)),
        host.default_output_device().and_then(|d| name_of(&d)),
    )
}

/// The engine's input: the ring, plus the optional built-in test tone.
///
/// The tone exists so the wave view is usable with no instrument attached. Without it the
/// scope and the transfer plot are flat lines until somebody plays, which makes the one
/// view that shows clipping impossible to look at on a quiet desk.
struct Feed {
    cons: Cons,
    shared: Arc<Shared>,
    sr: f32,
    phase: f32,
}

impl Source for Feed {
    fn read(&mut self, dst: &mut [f32]) -> usize {
        let hz = self.shared.tone();
        let n = self.cons.pop(dst);
        if hz <= 0.0 {
            return n;
        }
        // With the tone on, claim the whole block: reporting starvation here would tell the
        // engine we are silent just before we fill the block with a test signal.
        let inc = 2.0 * std::f32::consts::PI * hz / self.sr.max(1.0);
        // Hoisted: `dst.len()` inside the loop would borrow `dst` while it is being written.
        let fill = n < dst.len();
        for s in dst.iter_mut() {
            if fill {
                *s = 0.0;
            }
            self.phase += inc;
            if self.phase > std::f32::consts::PI {
                self.phase -= 2.0 * std::f32::consts::PI;
            }
            *s += 0.3 * self.phase.sin();
        }
        dst.len()
    }

    fn level(&self) -> usize {
        self.cons.level()
    }
}

/// The two live streams. Held only to keep them open -- dropping a cpal `Stream` closes it.
struct Streams {
    _in: Option<Stream>,
    _out: Stream,
}

/// Owns the audio side. The streams live on the thread we spawn, so `Drop` here is what
/// actually stops them: quit flag, join, then the thread's `Streams` go out of scope.
pub struct Audio {
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Audio {
    /// Start the audio side. `engine` is shared with the output callback, and keeping the
    /// `Arc` here is what lets a device change hand the *same* engine -- delay lines,
    /// reverb tail and all -- to the replacement stream instead of starting cold.
    pub fn spawn(
        shared: Arc<Shared>,
        engine: Arc<Mutex<Engine>>,
        req: ReopenReq,
    ) -> Result<Audio, String> {
        let quit = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("triode-audio".into())
            .spawn({
                let quit = quit.clone();
                move || run(shared, engine, req, quit)
            })
            .map_err(|e| format!("could not start the audio thread: {e}"))?;
        Ok(Audio {
            quit,
            thread: Some(thread),
        })
    }
}

impl Drop for Audio {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Build the streams, then watch for reopen requests until quit.
fn run(shared: Arc<Shared>, engine: Arc<Mutex<Engine>>, mut req: ReopenReq, quit: Arc<AtomicBool>) {
    let mut streams = match open(&shared, &engine, &req) {
        Ok(s) => s,
        Err(e) => {
            shared.set_status(format!("audio: {e}"));
            return;
        }
    };
    shared.ready.store(true, Ordering::Relaxed);
    while !quit.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(40));
        let next = shared.reopen.try_lock().ok().and_then(|mut r| r.take());
        let Some(new_req) = next else { continue };
        // Close the old device first, or the same interface shows up twice in the list and
        // the new stream can end up on the handle we just abandoned.
        drop(streams);
        shared.ready.store(false, Ordering::Relaxed);
        req = new_req;
        match open(&shared, &engine, &req) {
            Ok(s) => {
                streams = s;
                shared.ready.store(true, Ordering::Relaxed);
                shared.set_status("audio restarted");
            }
            Err(e) => {
                shared.set_status(format!("could not reopen audio: {e}"));
                return;
            }
        }
    }
    shared.ready.store(false, Ordering::Relaxed);
}

fn device(wanted: &Option<String>, input: bool) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let what = if input { "input" } else { "output" };
    if let Some(name) = wanted {
        let mut list = if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .map_err(|e| format!("could not list {what} devices: {e}"))?;
        return list
            .find(|d| name_of(d).as_deref() == Some(name.as_str()))
            .ok_or_else(|| format!("no such {what} device: {name}"));
    }
    let d = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    d.ok_or_else(|| format!("no default {what} device -- plug something in"))
}

fn open(
    shared: &Arc<Shared>,
    engine: &Arc<Mutex<Engine>>,
    req: &ReopenReq,
) -> Result<Streams, String> {
    let out_dev = device(&req.output, false)?;
    let out_cfg = out_dev
        .default_output_config()
        .map_err(|e| format!("output device has no default config: {e}"))?;
    let rate = out_cfg.sample_rate();
    // The engine always renders stereo; a mono output device just gets the left channel.
    let out_ch = out_cfg.channels().max(1);

    // The input is optional on purpose: an output-only machine should still get an amp,
    // silent, rather than refusing to start.
    // Deliberately not opened unless asked: an armed mic next to these speakers is a
    // feedback loop the user hears before they have touched anything.
    let in_dev = if req.input_on {
        device(&req.input, true).ok()
    } else {
        None
    };
    let in_cfg = in_dev.as_ref().and_then(|d| d.default_input_config().ok());
    let in_rate = in_cfg.as_ref().map(|c| c.sample_rate()).unwrap_or(rate);

    {
        // Sized for these devices before the first callback can fire.
        let mut e = engine.lock().unwrap_or_else(|e| e.into_inner());
        let frames = (req.buffer_ms.max(1) as usize * rate as usize / 1000).clamp(8, MAX_CHUNK);
        e.set_chunk(frames);
        e.set_rates(in_rate as f32, rate as f32);
    }

    let ring = Ring::new(rate as usize * RING_MS as usize / 1000);
    let feed = Feed {
        cons: ring.consumer(),
        shared: shared.clone(),
        sr: rate as f32,
        phase: 0.0,
    };

    let input = match (&in_dev, &in_cfg) {
        (Some(dev), Some(cfg)) => {
            let sc = StreamConfig {
                channels: cfg.channels(),
                sample_rate: cfg.sample_rate(),
                buffer_size: cpal::BufferSize::Default,
            };
            match build_in(
                dev,
                sc,
                ring.producer(),
                shared.clone(),
                cfg.sample_format(),
            ) {
                Ok(s) => {
                    s.play()
                        .map_err(|e| format!("could not start input: {e}"))?;
                    Some(s)
                }
                // A machine with a broken/absent mic is not a reason to have no amp.
                Err(e) => {
                    shared.set_status(format!("input unavailable ({e}), running silent"));
                    None
                }
            }
        }
        _ => {
            shared.set_status(if req.input_on {
                "no input device, running silent"
            } else {
                "input disarmed - output only (arm it in the header to play)"
            });
            None
        }
    };

    let osc = StreamConfig {
        channels: out_ch,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let fmt = out_cfg.sample_format();
    let out = match fmt {
        SampleFormat::F32 => build_out::<f32>(&out_dev, osc, engine.clone(), feed, shared.clone()),
        SampleFormat::F64 => build_out::<f64>(&out_dev, osc, engine.clone(), feed, shared.clone()),
        SampleFormat::I16 => build_out::<i16>(&out_dev, osc, engine.clone(), feed, shared.clone()),
        SampleFormat::I32 => build_out::<i32>(&out_dev, osc, engine.clone(), feed, shared.clone()),
        SampleFormat::U8 => build_out::<u8>(&out_dev, osc, engine.clone(), feed, shared.clone()),
        other => return Err(format!("output device format {other:?} is not supported")),
    }?;
    out.play()
        .map_err(|e| format!("could not start output: {e}"))?;
    Ok(Streams {
        _in: input,
        _out: out,
    })
}

/// Downmix the device's frames to mono f32 and push them into the ring.
///
/// Pushing happens once per 256 frames rather than per sample, because each `push` is an
/// atomic store on the write cursor and one per sample is pure waste.
macro_rules! in_arm {
    ($t:ty, $dev:expr, $cfg:expr, $prod:expr, $shared:expr) => {{
        let mut prod: Prod = $prod;
        let sh = $shared.clone();
        let ch = $cfg.channels.max(1) as usize;
        let mut buf = [0.0f32; 256];
        $dev.build_input_stream(
            $cfg,
            move |data: &[$t], _| {
                let mut src = 0usize;
                while src + ch <= data.len() {
                    let mut k = 0usize;
                    while k < buf.len() && src + ch <= data.len() {
                        let mut acc = 0.0f32;
                        for s in &data[src..src + ch] {
                            acc += f32::from_sample(*s);
                        }
                        buf[k] = acc / ch as f32;
                        k += 1;
                        src += ch;
                    }
                    prod.push(&buf[..k]);
                }
            },
            move |e| sh.set_status(format!("input stream error: {e}")),
            None,
        )
        .map_err(|e| e.to_string())
    }};
}

fn build_in(
    dev: &cpal::Device,
    cfg: StreamConfig,
    prod: Prod,
    shared: Arc<Shared>,
    fmt: SampleFormat,
) -> Result<Stream, String> {
    match fmt {
        SampleFormat::F32 => in_arm!(f32, dev, cfg, prod, shared),
        SampleFormat::F64 => in_arm!(f64, dev, cfg, prod, shared),
        SampleFormat::I16 => in_arm!(i16, dev, cfg, prod, shared),
        SampleFormat::I32 => in_arm!(i32, dev, cfg, prod, shared),
        SampleFormat::U8 => in_arm!(u8, dev, cfg, prod, shared),
        other => Err(format!("input device format {other:?} is not supported")),
    }
}

/// The output callback, generic over the device's sample format.
///
/// The engine renders into a fixed `[Frame; MAX_CHUNK]` and the callback copies it out in
/// device format: the device decides how many frames it wants per call, while the engine's
/// chunk is the unit its parameter smoothers are tuned to, so the two are deliberately not
/// the same number.
fn build_out<T>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    engine: Arc<Mutex<Engine>>,
    mut feed: Feed,
    shared: Arc<Shared>,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut scratch = [[0.0f32; 2]; MAX_CHUNK];
    let sh = shared.clone();
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            // Poison-tolerant: the only way this lock is poisoned is a panic somewhere
            // else, and stopping the audio for that turns one bug into a dead amp.
            let mut e = match engine.lock() {
                Ok(e) => e,
                Err(p) => p.into_inner(),
            };
            let mut done = 0usize;
            while done + ch <= data.len() {
                let frames = ((data.len() - done) / ch).min(MAX_CHUNK);
                e.process(Some(&shared), &mut feed, &mut scratch[..frames]);
                for (i, f) in scratch[..frames].iter().enumerate() {
                    let base = (done / ch + i) * ch;
                    for c in 0..ch {
                        data[base + c] = T::from_sample(f[c.min(1)]);
                    }
                }
                done += frames * ch;
            }
        },
        move |e| sh.set_status(format!("output stream error: {e}")),
        None,
    )
    .map_err(|e| format!("could not build the output stream: {e}"))
}

/// One sample-format instantiation of the capture callback. Mono downmix, same rule as the
/// live input: average the channels, because a guitar is mono.
macro_rules! cap_arm {
    ($t:ty, $dev:expr, $cfg:expr, $sink:expr) => {{
        let sink: Arc<Mutex<Vec<f32>>> = $sink;
        let ch = $cfg.channels.max(1) as usize;
        $dev.build_input_stream(
            $cfg,
            move |data: &[$t], _| {
                if let Ok(mut buf) = sink.lock() {
                    let mut i = 0usize;
                    while i + ch <= data.len() {
                        let mut acc = 0.0f32;
                        for s in &data[i..i + ch] {
                            acc += f32::from_sample(*s);
                        }
                        buf.push(acc / ch as f32);
                        i += ch;
                    }
                }
            },
            move |e| eprintln!("capture stream error: {e}"),
            None,
        )
    }};
}

/// Record the input device to a WAV and stop. No engine, no output device, no UI.
///
/// This exists so a real performance is one command away from being test input:
/// `triode capture take1.wav`, then `triode trace take1.wav` or `--render`. Everything the
/// live path can do to a signal can then be measured against something a human actually
/// played, not only against generated sines.
///
/// A mutex in the *live* callback would be unacceptable; here nothing contends, because the
/// recording thread reads the buffer once the stream is closed. The vector is reserved up
/// front so the callback still never allocates.
pub fn capture(out: &Path, req: &ReopenReq, secs: f32) -> Result<usize, String> {
    let dev = device(&req.input, true)?;
    let cfg = dev
        .default_input_config()
        .map_err(|e| format!("could not read the input config: {e}"))?;
    let sr = cfg.sample_rate();
    let want = (secs.max(0.1) * sr as f32) as usize;
    let sink: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::with_capacity(want + 4096)));

    let sc = StreamConfig {
        channels: cfg.channels(),
        sample_rate: sr,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => cap_arm!(f32, &dev, sc, sink.clone()),
        SampleFormat::F64 => cap_arm!(f64, &dev, sc, sink.clone()),
        SampleFormat::I16 => cap_arm!(i16, &dev, sc, sink.clone()),
        SampleFormat::I32 => cap_arm!(i32, &dev, sc, sink.clone()),
        SampleFormat::U8 => cap_arm!(u8, &dev, sc, sink.clone()),
        other => return Err(format!("input device format {other:?} is not supported")),
    }
    .map_err(|e| format!("could not build the capture stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("could not start recording: {e}"))?;

    // Poll on wall clock. Granularity of 100 ms on a recording that ends by time anyway.
    let start = std::time::Instant::now();
    let budget = Duration::from_secs_f32(secs.max(0.1));
    while start.elapsed() < budget {
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stream);

    let data = std::mem::take(
        &mut *sink
            .lock()
            .map_err(|_| "the capture buffer was poisoned".to_string())?,
    );
    if data.is_empty() {
        return Err(
            "nothing was recorded -- check the input device and microphone permission".into(),
        );
    }
    let frames: Vec<crate::dsp::Frame> = data.iter().map(|v| [*v, *v]).collect();
    let peak = frames.iter().fold(0.0f32, |a, f| a.max(f[0].abs()));
    crate::render::write_wav_stereo(out, &frames, sr)
        .map_err(|e| format!("could not write {}: {e}", out.display()))?;
    eprintln!(
        "recorded {} frames ({:.1} s) at {} Hz, peak {:.3} -> {}",
        frames.len(),
        frames.len() as f32 / sr as f32,
        sr,
        peak,
        out.display()
    );
    if peak < 0.01 {
        eprintln!(
            "note: that is very quiet -- check gain, or that this is the input you think it is"
        );
    }
    Ok(frames.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, ReopenReq, Shared};

    /// End-to-end against real hardware: open this machine's default devices, run the
    /// engine, and confirm the published stats say audio actually moved — that is the part
    /// no offline test can cover (cpal device negotiation, sample-format conversion, the
    /// two-clock resampler, the scope window filling).
    ///
    /// `#[ignore]`d deliberately: it claims the real input and output devices, and on macOS
    /// a CLI-launched binary either inherits the terminal's microphone permission or gets
    /// silently denied (which shows up as `silent_frames` climbing, not as an error). Run it
    /// on purpose: `cargo test -- --ignored`.
    #[test]
    #[ignore = "opens real audio devices; run with `cargo test -- --ignored`"]
    fn live_devices_run_and_fill_the_scope() {
        let shared = Shared::new();
        shared.set_scope(true);
        // The built-in tone means this proves the *output* path even where the mic is
        // denied; the input path is what the printed status tells you about.
        shared.set_tone(220.0);
        let engine = Arc::new(Mutex::new(Engine::new(44_100.0, 64)));
        // Armed on purpose: this test exists to exercise the real input path, which the
        // default output-only startup deliberately does not open.
        let req = ReopenReq {
            input: None,
            output: None,
            buffer_ms: 5,
            input_on: true,
        };
        let _audio = match Audio::spawn(shared.clone(), engine, req) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("no usable audio device on this machine: {e}");
                return;
            }
        };
        let mut running = false;
        for _ in 0..150 {
            std::thread::sleep(Duration::from_millis(20));
            let snap = shared.stats.snapshot();
            if shared.ready.load(Ordering::Relaxed) && snap.rate > 0 && snap.out_peak > 0.0 {
                running = true;
                break;
            }
        }
        assert!(
            running,
            "the audio thread never produced output (status: {:?})",
            shared.status()
        );
        let (_din, dout, _sr, _seq) = shared
            .scope_snap()
            .expect("a scope window once audio is running");
        assert!(
            dout.iter().any(|v| v.abs() > 0.01),
            "scope/transfer window stayed flat even though the test tone was on"
        );
        let snap = shared.stats.snapshot();
        eprintln!(
            "live: {} Hz, chunk {}, cpu {:.0}%, in peak {:.3}, out peak {:.3}",
            snap.rate, snap.buffer_frames, snap.cpu_pct, snap.in_peak, snap.out_peak,
        );
    }
}
