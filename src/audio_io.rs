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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, ErrorKind, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig,
    SupportedBufferSize, SupportedStreamConfig,
};

use crate::dsp::resampler::Source;
use crate::dsp::ring::{Cons, Prod, Ring};
use crate::dsp::MAX_CHUNK;
use crate::engine::{Engine, ReopenReq, Shared};

/// How long the input ring holds: long enough to ride out a scheduler hiccup, short enough
/// that a dead input reads as silence rather than as a delay that keeps growing.
const RING_MS: f32 = 250.0;
const MAX_OPEN_ATTEMPTS: u8 = 4;
const INPUT_ERROR: u32 = 1;
const OUTPUT_ERROR: u32 = 2;

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
        for (i, s) in dst.iter_mut().enumerate() {
            if i >= n {
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

    fn clock_level(&self) -> Option<usize> {
        (self.shared.tone() <= 0.0).then(|| self.cons.level())
    }
}

/// The two live streams. Held only to keep them open -- dropping a cpal `Stream` closes it.
struct Streams {
    _in: Option<Stream>,
    _out: Stream,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceIdentity {
    name: String,
    address: Option<String>,
}

impl DeviceIdentity {
    fn matches(&self, device: &cpal::Device) -> bool {
        let Ok(description) = device.description() else {
            return false;
        };
        match self.address.as_deref() {
            Some(address) => description.address() == Some(address),
            None => description.name() == self.name,
        }
    }
}

#[derive(Clone, Debug)]
struct OpenReq {
    settings: ReopenReq,
    input: Option<DeviceIdentity>,
    output: Option<DeviceIdentity>,
}

impl OpenReq {
    fn manual(settings: ReopenReq) -> Self {
        Self {
            settings,
            input: None,
            output: None,
        }
    }
}

struct Opened<S> {
    streams: S,
    recovery: OpenReq,
    note: String,
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

/// Build the streams, then own recovery and manual reopen requests until quit.
fn run(shared: Arc<Shared>, engine: Arc<Mutex<Engine>>, req: ReopenReq, quit: Arc<AtomicBool>) {
    let callback_errors = Arc::new(AtomicU32::new(0));
    let open_shared = shared.clone();
    run_owner(
        shared,
        OpenReq::manual(req),
        quit,
        callback_errors,
        move |req, errors| open(&open_shared, &engine, req, errors),
        || std::thread::sleep(Duration::from_millis(40)),
    );
}

fn run_owner<S>(
    shared: Arc<Shared>,
    mut req: OpenReq,
    quit: Arc<AtomicBool>,
    callback_errors: Arc<AtomicU32>,
    mut opener: impl FnMut(&OpenReq, Arc<AtomicU32>) -> Result<Opened<S>, String>,
    mut wait: impl FnMut(),
) {
    let mut streams = None;
    let mut attempts = 0u8;
    let mut opening = true;
    let mut retry_ticks = 0usize;

    while !quit.load(Ordering::Relaxed) {
        shared.collect_retired();

        let manual = match shared.reopen.lock() {
            Ok(mut request) => request.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(manual) = manual {
            streams.take();
            shared.ready.store(false, Ordering::Relaxed);
            callback_errors.store(0, Ordering::Relaxed);
            req = OpenReq::manual(manual);
            retry_ticks = 0;
            attempts = 0;
            opening = true;
        } else {
            let errors = callback_errors.swap(0, Ordering::Relaxed);
            if errors != 0 && streams.is_some() {
                streams.take();
                shared.ready.store(false, Ordering::Relaxed);
                attempts = 0;
                retry_ticks = 0;
                opening = true;
                let side = match errors {
                    INPUT_ERROR => "input",
                    OUTPUT_ERROR => "output",
                    _ => "input/output",
                };
                shared.set_status(format!("{side} stream failed; recovering"));
            }
        }

        if retry_ticks > 0 {
            retry_ticks -= 1;
        } else if opening {
            match opener(&req, callback_errors.clone()) {
                Ok(opened) => {
                    req = opened.recovery;
                    streams = Some(opened.streams);
                    attempts = 0;
                    opening = false;
                    let mut applied = req.settings.clone();
                    if let Some(input) = &req.input {
                        applied.input = Some(input.name.clone());
                    }
                    if let Some(output) = &req.output {
                        applied.output = Some(output.name.clone());
                    }
                    *shared.applied.lock().unwrap_or_else(|e| e.into_inner()) = Some(applied);
                    shared.ready.store(true, Ordering::Relaxed);
                    shared.set_status(if opened.note.is_empty() {
                        "audio running"
                    } else {
                        &opened.note
                    });
                }
                Err(error) => {
                    attempts += 1;
                    opening = attempts < MAX_OPEN_ATTEMPTS;
                    // 40 ms owner ticks: 400/800/1600 ms waits, interrupted by manual retry or quit.
                    retry_ticks = if opening { (1usize << attempts) * 5 } else { 0 };
                    if opening {
                        shared.set_status(format!(
                            "audio unavailable (attempt {attempts}/{MAX_OPEN_ATTEMPTS}): {error}"
                        ));
                    } else {
                        shared.set_status(format!(
                            "audio unavailable after {MAX_OPEN_ATTEMPTS} attempts: {error}; choose devices to retry"
                        ));
                    }
                }
            }
        }

        wait();
    }
    drop(streams);
    shared.ready.store(false, Ordering::Relaxed);
}

fn identity_of(device: &cpal::Device) -> Result<DeviceIdentity, String> {
    let description = device
        .description()
        .map_err(|e| format!("could not identify device: {e}"))?;
    Ok(DeviceIdentity {
        name: description.name().to_string(),
        address: description.address().map(str::to_string),
    })
}

fn device(
    wanted: &Option<String>,
    pinned: Option<&DeviceIdentity>,
    input: bool,
) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let what = if input { "input" } else { "output" };
    if let Some(identity) = pinned {
        let mut list = if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .map_err(|e| format!("could not list {what} devices: {e}"))?;
        return list
            .find(|candidate| identity.matches(candidate))
            .ok_or_else(|| format!("pinned {what} device is unavailable: {}", identity.name));
    }
    if let Some(name) = wanted {
        let mut list = if input {
            host.input_devices()
        } else {
            host.output_devices()
        }
        .map_err(|e| format!("could not list {what} devices: {e}"))?;
        return list
            .find(|candidate| name_of(candidate).as_deref() == Some(name.as_str()))
            .ok_or_else(|| format!("no such {what} device: {name}"));
    }
    let device = if input {
        host.default_input_device()
    } else {
        host.default_output_device()
    };
    device.ok_or_else(|| format!("no default {what} device -- plug something in"))
}

fn requested_config(config: &SupportedStreamConfig, buffer_ms: u32) -> (StreamConfig, u32) {
    let requested = ((buffer_ms.max(1) as u64 * config.sample_rate() as u64) / 1000)
        .clamp(1, u32::MAX as u64) as u32;
    let frames = match *config.buffer_size() {
        SupportedBufferSize::Range { min, max } => requested.clamp(min, max),
        SupportedBufferSize::Unknown => requested,
    };
    (
        StreamConfig {
            channels: config.channels(),
            sample_rate: config.sample_rate(),
            buffer_size: BufferSize::Fixed(frames),
        },
        frames,
    )
}

fn default_config(config: &SupportedStreamConfig) -> StreamConfig {
    StreamConfig {
        buffer_size: BufferSize::Default,
        ..config.config()
    }
}

fn buffer_rejected(error: &cpal::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::UnsupportedConfig | ErrorKind::InvalidInput
    )
}

fn open(
    shared: &Arc<Shared>,
    engine: &Arc<Mutex<Engine>>,
    req: &OpenReq,
    callback_errors: Arc<AtomicU32>,
) -> Result<Opened<Streams>, String> {
    let settings = &req.settings;
    let out_dev = device(&settings.output, req.output.as_ref(), false)?;
    let out_identity = identity_of(&out_dev)?;
    let out_cfg = out_dev
        .default_output_config()
        .map_err(|e| format!("output device has no default config: {e}"))?;
    let out_rate = out_cfg.sample_rate();
    let mut recovery = req.clone();
    recovery.output = Some(out_identity);
    let mut notes = Vec::new();

    // Disarmed input is optional. Explicitly armed input must actually open; otherwise
    // report/retry the failure rather than publishing a misleading successful setup.
    let mut input = None;
    let mut in_rate = out_rate;
    let mut input_device = None;
    let mut input_config = None;
    if settings.input_on {
        let dev = device(&settings.input, req.input.as_ref(), true)?;
        let config = dev
            .default_input_config()
            .map_err(|e| format!("input config: {e}"))?;
        in_rate = config.sample_rate();
        recovery.input = Some(identity_of(&dev)?);
        input_device = Some(dev);
        input_config = Some(config);
    } else {
        recovery.input = None;
        notes.push("input disarmed - output only (arm it in the header to play)".into());
    }

    {
        // No callback can be live yet. Rate changes reset only the source resampler; effect
        // state belongs to this engine and survives stream recovery.
        let mut engine = engine.lock().unwrap_or_else(|error| error.into_inner());
        let frames =
            ((settings.buffer_ms.max(1) as usize * out_rate as usize) / 1000).clamp(8, MAX_CHUNK);
        engine.set_chunk(frames);
        engine.set_rates(in_rate as f32, out_rate as f32);
    }

    let ring = Ring::new(in_rate as usize * RING_MS as usize / 1000);
    if let (Some(dev), Some(config)) = (&input_device, &input_config) {
        let (stream, note) = build_input_with_fallback(
            dev,
            config,
            settings.buffer_ms,
            &ring,
            shared.clone(),
            callback_errors.clone(),
        )
        .map_err(|e| format!("input unavailable: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("could not start input: {e}"))?;
        input = Some(stream);
        if let Some(note) = note {
            notes.push(note);
        }
    }

    let (out, output_note) = build_output_with_fallback(
        &out_dev,
        &out_cfg,
        settings.buffer_ms,
        engine.clone(),
        &ring,
        shared.clone(),
        callback_errors,
    )?;
    out.play()
        .map_err(|e| format!("could not start output: {e}"))?;
    if let Some(note) = output_note {
        notes.push(note);
    }

    Ok(Opened {
        streams: Streams {
            _in: input,
            _out: out,
        },
        recovery,
        note: notes.join("; "),
    })
}

/// Use the first two physical inputs and ignore extra interface loopback channels.
fn physical_input_mono<T>(frame: &[T]) -> f32
where
    T: Sample + Copy,
    f32: FromSample<T>,
{
    let channels = frame.len().min(2);
    if channels == 0 {
        return 0.0;
    }
    frame[..channels]
        .iter()
        .map(|sample| f32::from_sample(*sample))
        .sum::<f32>()
        / channels as f32
}

/// Downmix the device's first two physical inputs to mono and push them into the ring.
///
/// Pushing happens once per 256 frames rather than per sample, because each `push` is an
/// atomic store on the write cursor and one per sample is pure waste.
macro_rules! in_arm {
    ($t:ty, $dev:expr, $cfg:expr, $ring:expr, $shared:expr, $errors:expr) => {{
        let mut prod: Prod = $ring.producer();
        let stats = $shared.clone();
        let errors = $errors.clone();
        let ch = $cfg.channels.max(1) as usize;
        let mut buf = [0.0f32; 256];
        $dev.build_input_stream(
            $cfg,
            move |data: &[$t], _| {
                let mut src = 0usize;
                while src + ch <= data.len() {
                    let mut k = 0usize;
                    while k < buf.len() && src + ch <= data.len() {
                        buf[k] = physical_input_mono(&data[src..src + ch]);
                        k += 1;
                        src += ch;
                    }
                    let written = prod.push(&buf[..k]);
                    stats
                        .stats
                        .dropped_in
                        .fetch_add((k - written) as u64, Ordering::Relaxed);
                }
            },
            move |_error| {
                errors.fetch_or(INPUT_ERROR, Ordering::Relaxed);
            },
            None,
        )
    }};
}

fn build_in(
    dev: &cpal::Device,
    cfg: StreamConfig,
    ring: &Ring,
    shared: Arc<Shared>,
    errors: Arc<AtomicU32>,
    fmt: SampleFormat,
) -> Result<Stream, cpal::Error> {
    match fmt {
        SampleFormat::F32 => in_arm!(f32, dev, cfg, ring, shared, errors),
        SampleFormat::F64 => in_arm!(f64, dev, cfg, ring, shared, errors),
        SampleFormat::I16 => in_arm!(i16, dev, cfg, ring, shared, errors),
        SampleFormat::I32 => in_arm!(i32, dev, cfg, ring, shared, errors),
        SampleFormat::U8 => in_arm!(u8, dev, cfg, ring, shared, errors),
        other => Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("input device format {other:?} is not supported"),
        )),
    }
}

fn build_input_with_fallback(
    dev: &cpal::Device,
    supported: &SupportedStreamConfig,
    buffer_ms: u32,
    ring: &Ring,
    shared: Arc<Shared>,
    errors: Arc<AtomicU32>,
) -> Result<(Stream, Option<String>), String> {
    let (fixed, frames) = requested_config(supported, buffer_ms);
    match build_in(
        dev,
        fixed,
        ring,
        shared.clone(),
        errors.clone(),
        supported.sample_format(),
    ) {
        Ok(stream) => Ok((stream, None)),
        Err(fixed_error) if buffer_rejected(&fixed_error) => build_in(
            dev,
            default_config(supported),
            ring,
            shared,
            errors,
            supported.sample_format(),
        )
        .map(|stream| {
            (
                stream,
                Some(format!(
                    "input rejected fixed {frames}-frame buffer ({fixed_error}); using device default"
                )),
            )
        })
        .map_err(|error| {
            format!(
                "fixed {frames}-frame buffer was rejected ({fixed_error}); default also failed ({error})"
            )
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// The output callback, generic over the device's sample format.
fn build_out<T>(
    dev: &cpal::Device,
    cfg: StreamConfig,
    engine: Arc<Mutex<Engine>>,
    ring: &Ring,
    input_rate: f32,
    shared: Arc<Shared>,
    errors: Arc<AtomicU32>,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = cfg.channels.max(1) as usize;
    let mut scratch = [[0.0f32; 2]; MAX_CHUNK];
    let mut feed = Feed {
        cons: ring.consumer(),
        shared: shared.clone(),
        sr: input_rate,
        phase: 0.0,
    };
    dev.build_output_stream(
        cfg,
        move |data: &mut [T], _| {
            let callback_frames = data.len() / ch;
            let mut engine = match engine.try_lock() {
                Ok(engine) => engine,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    for sample in data.iter_mut() {
                        *sample = T::from_sample(0.0);
                    }
                    shared.stats.overruns.fetch_add(1, Ordering::Relaxed);
                    shared
                        .stats
                        .buffer_frames
                        .store(callback_frames as u32, Ordering::Relaxed);
                    return;
                }
            };
            let mut done = 0usize;
            while done + ch <= data.len() {
                let frames = ((data.len() - done) / ch).min(MAX_CHUNK);
                engine.process(Some(&shared), &mut feed, &mut scratch[..frames]);
                for (i, frame) in scratch[..frames].iter().enumerate() {
                    let base = (done / ch + i) * ch;
                    for channel in 0..ch {
                        data[base + channel] = T::from_sample(frame[channel.min(1)]);
                    }
                }
                done += frames * ch;
            }
            shared
                .stats
                .buffer_frames
                .store(callback_frames as u32, Ordering::Relaxed);
        },
        move |_error| {
            errors.fetch_or(OUTPUT_ERROR, Ordering::Relaxed);
        },
        None,
    )
}

fn build_output_once(
    dev: &cpal::Device,
    cfg: StreamConfig,
    engine: Arc<Mutex<Engine>>,
    ring: &Ring,
    shared: Arc<Shared>,
    errors: Arc<AtomicU32>,
    format: SampleFormat,
) -> Result<Stream, cpal::Error> {
    // Stream construction is off-callback; rates were configured by open().
    let input_rate = engine.lock().unwrap_or_else(|e| e.into_inner()).in_sr();
    match format {
        SampleFormat::F32 => build_out::<f32>(dev, cfg, engine, ring, input_rate, shared, errors),
        SampleFormat::F64 => build_out::<f64>(dev, cfg, engine, ring, input_rate, shared, errors),
        SampleFormat::I16 => build_out::<i16>(dev, cfg, engine, ring, input_rate, shared, errors),
        SampleFormat::I32 => build_out::<i32>(dev, cfg, engine, ring, input_rate, shared, errors),
        SampleFormat::U8 => build_out::<u8>(dev, cfg, engine, ring, input_rate, shared, errors),
        other => Err(cpal::Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("output device format {other:?} is not supported"),
        )),
    }
}

fn build_output_with_fallback(
    dev: &cpal::Device,
    supported: &SupportedStreamConfig,
    buffer_ms: u32,
    engine: Arc<Mutex<Engine>>,
    ring: &Ring,
    shared: Arc<Shared>,
    errors: Arc<AtomicU32>,
) -> Result<(Stream, Option<String>), String> {
    let (fixed, frames) = requested_config(supported, buffer_ms);
    match build_output_once(
        dev,
        fixed,
        engine.clone(),
        ring,
        shared.clone(),
        errors.clone(),
        supported.sample_format(),
    ) {
        Ok(stream) => Ok((stream, None)),
        Err(fixed_error) if buffer_rejected(&fixed_error) => build_output_once(
            dev,
            default_config(supported),
            engine,
            ring,
            shared,
            errors,
            supported.sample_format(),
        )
        .map(|stream| {
            (
                stream,
                Some(format!(
                    "output rejected fixed {frames}-frame buffer ({fixed_error}); using device default"
                )),
            )
        })
        .map_err(|error| {
            format!(
                "fixed {frames}-frame buffer was rejected ({fixed_error}); default also failed ({error})"
            )
        }),
        Err(error) => Err(error.to_string()),
    }
}

/// One sample-format instantiation of the capture callback. Mono downmix, same rule as the
/// live input: use the first two physical channels and ignore interface loopback channels.
macro_rules! cap_arm {
    ($t:ty, $dev:expr, $cfg:expr, $sink:expr, $failed:expr) => {{
        let sink: Arc<Mutex<Vec<f32>>> = $sink;
        let failed: Arc<AtomicBool> = $failed;
        let ch = $cfg.channels.max(1) as usize;
        $dev.build_input_stream(
            $cfg,
            move |data: &[$t], _| {
                if let Ok(mut buf) = sink.lock() {
                    let mut i = 0usize;
                    while i + ch <= data.len() && buf.len() < buf.capacity() {
                        buf.push(physical_input_mono(&data[i..i + ch]));
                        i += ch;
                    }
                }
            },
            move |_error| failed.store(true, Ordering::Relaxed),
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
    if !secs.is_finite() || !(0.1..=600.0).contains(&secs) {
        return Err("capture duration must be finite and between 0.1 and 600 seconds".into());
    }
    let dev = device(&req.input, None, true)?;
    let cfg = dev
        .default_input_config()
        .map_err(|e| format!("could not read the input config: {e}"))?;
    let sr = cfg.sample_rate();
    let want = (secs.max(0.1) * sr as f32) as usize;
    let sink: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::with_capacity(want)));
    drop(sink.lock().expect("new capture mutex"));
    let failed = Arc::new(AtomicBool::new(false));

    let sc = StreamConfig {
        channels: cfg.channels(),
        sample_rate: sr,
        buffer_size: cpal::BufferSize::Default,
    };
    let stream = match cfg.sample_format() {
        SampleFormat::F32 => cap_arm!(f32, &dev, sc, sink.clone(), failed.clone()),
        SampleFormat::F64 => cap_arm!(f64, &dev, sc, sink.clone(), failed.clone()),
        SampleFormat::I16 => cap_arm!(i16, &dev, sc, sink.clone(), failed.clone()),
        SampleFormat::I32 => cap_arm!(i32, &dev, sc, sink.clone(), failed.clone()),
        SampleFormat::U8 => cap_arm!(u8, &dev, sc, sink.clone(), failed.clone()),
        other => return Err(format!("input device format {other:?} is not supported")),
    }
    .map_err(|e| format!("could not build the capture stream: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("could not start recording: {e}"))?;

    // Poll on wall clock. Granularity of 100 ms on a recording that ends by time anyway.
    let start = std::time::Instant::now();
    let budget = Duration::from_secs_f32(secs.max(0.1));
    while start.elapsed() < budget && !failed.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stream);
    finish_capture(out, &sink, sr, &failed)
}

fn finish_capture(
    out: &Path,
    sink: &Mutex<Vec<f32>>,
    sr: u32,
    failed: &AtomicBool,
) -> Result<usize, String> {
    if failed.load(Ordering::Relaxed) {
        return Err("capture stream failed; incomplete recording was not saved".into());
    }
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

    #[test]
    fn multichannel_interfaces_do_not_mix_loopback_channels_into_input() {
        let scarlett = [0.2f32, 0.4, 1.0, -1.0];
        assert!((physical_input_mono(&scarlett) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn requested_buffers_are_rate_scaled_clamped_and_have_a_default_fallback() {
        let supported = SupportedStreamConfig::new(
            2,
            48000,
            SupportedBufferSize::Range { min: 128, max: 512 },
            SampleFormat::F32,
        );
        for (ms, expected) in [(1, 128), (5, 240), (50, 512)] {
            let (config, frames) = requested_config(&supported, ms);
            assert_eq!(frames, expected);
            assert_eq!(config.buffer_size, BufferSize::Fixed(expected));
        }
        assert_eq!(default_config(&supported).buffer_size, BufferSize::Default);
        let unknown =
            SupportedStreamConfig::new(1, 96000, SupportedBufferSize::Unknown, SampleFormat::F32);
        assert_eq!(requested_config(&unknown, 5).1, 480);
    }

    #[test]
    fn capture_callback_failure_rejects_partial_recording() {
        struct FailingDevice;
        impl FailingDevice {
            fn build_input_stream(
                &self,
                _: StreamConfig,
                mut data: impl FnMut(&[f32], ()),
                mut error: impl FnMut(cpal::Error),
                _: Option<Duration>,
            ) {
                data(&[0.25; 8], ());
                error(cpal::Error::with_message(
                    ErrorKind::UnsupportedConfig,
                    "injected disconnect",
                ));
            }
        }
        let sink = Arc::new(Mutex::new(Vec::with_capacity(16)));
        let failed = Arc::new(AtomicBool::new(false));
        let config = StreamConfig {
            channels: 1,
            sample_rate: 48000,
            buffer_size: BufferSize::Default,
        };
        cap_arm!(f32, FailingDevice, config, sink.clone(), failed.clone());
        assert_eq!(sink.lock().unwrap().len(), 8);
        assert_eq!(
            finish_capture(Path::new("unused.wav"), &sink, 48000, &failed).unwrap_err(),
            "capture stream failed; incomplete recording was not saved"
        );
        assert_eq!(
            sink.lock().unwrap().len(),
            8,
            "failed capture must not consume or write the partial buffer"
        );
    }

    #[test]
    fn invalid_capture_duration_is_rejected_before_opening_devices() {
        for seconds in [f32::NAN, f32::INFINITY, -1.0, 601.0] {
            assert!(capture(Path::new("unused.wav"), &request(), seconds).is_err());
        }
    }

    fn request() -> ReopenReq {
        ReopenReq {
            input: None,
            output: Some("interface".into()),
            buffer_ms: 5,
            input_on: false,
        }
    }

    #[test]
    fn failed_opens_retry_then_publish_the_successful_request() {
        use std::cell::Cell;
        let shared = Shared::new();
        let quit = Arc::new(AtomicBool::new(false));
        let attempts = Cell::new(0);
        let ticks = Cell::new(0);
        let saw_ready = Cell::new(false);
        run_owner(
            shared.clone(),
            OpenReq::manual(request()),
            quit.clone(),
            Arc::new(AtomicU32::new(0)),
            |req, _| {
                attempts.set(attempts.get() + 1);
                if attempts.get() < 3 {
                    return Err("not yet".into());
                }
                Ok(Opened {
                    streams: (),
                    recovery: req.clone(),
                    note: String::new(),
                })
            },
            || {
                ticks.set(ticks.get() + 1);
                if shared.ready.load(Ordering::Relaxed) {
                    saw_ready.set(true);
                    quit.store(true, Ordering::Relaxed);
                }
                if ticks.get() > 100 {
                    quit.store(true, Ordering::Relaxed);
                }
            },
        );
        assert_eq!(attempts.get(), 3);
        assert!(saw_ready.get());
        assert_eq!(*shared.applied.lock().unwrap(), Some(request()));
        assert!(
            !shared.ready.load(Ordering::Relaxed),
            "shutdown clears readiness"
        );
    }

    #[test]
    fn stream_errors_use_pinned_devices_but_manual_requests_can_change_them() {
        use std::cell::{Cell, RefCell};
        let shared = Shared::new();
        let quit = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(AtomicU32::new(0));
        let seen = RefCell::new(Vec::new());
        let ticks = Cell::new(0);
        run_owner(
            shared.clone(),
            OpenReq::manual(request()),
            quit.clone(),
            errors.clone(),
            |req, _| {
                seen.borrow_mut().push(req.clone());
                let mut recovery = req.clone();
                recovery.output = Some(DeviceIdentity {
                    name: "interface".into(),
                    address: Some("stable-id".into()),
                });
                Ok(Opened {
                    streams: (),
                    recovery,
                    note: String::new(),
                })
            },
            || {
                ticks.set(ticks.get() + 1);
                match ticks.get() {
                    1 => errors.store(OUTPUT_ERROR, Ordering::Relaxed),
                    2 => {
                        let mut req = request();
                        req.output = Some("new interface".into());
                        *shared.reopen.lock().unwrap() = Some(req);
                    }
                    _ => quit.store(true, Ordering::Relaxed),
                }
            },
        );
        let seen = seen.borrow();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[1].output.as_ref().unwrap().address.as_deref(),
            Some("stable-id")
        );
        assert!(seen[2].output.is_none());
        assert_eq!(seen[2].settings.output.as_deref(), Some("new interface"));
        assert!(!seen.iter().any(|r| r.settings.input_on));
    }

    #[test]
    fn exhausted_retry_budget_still_accepts_manual_retry_and_quit() {
        use std::cell::Cell;
        let shared = Shared::new();
        let quit = Arc::new(AtomicBool::new(false));
        let attempts = Cell::new(0);
        let ticks = Cell::new(0);
        run_owner::<()>(
            shared.clone(),
            OpenReq::manual(request()),
            quit.clone(),
            Arc::new(AtomicU32::new(0)),
            |_, _| {
                attempts.set(attempts.get() + 1);
                Err("unplugged".into())
            },
            || {
                ticks.set(ticks.get() + 1);
                if ticks.get() == 250 {
                    *shared.reopen.lock().unwrap() = Some(request());
                }
                if ticks.get() == 500 {
                    quit.store(true, Ordering::Relaxed);
                }
            },
        );
        assert_eq!(attempts.get(), 2 * MAX_OPEN_ATTEMPTS);
        assert!(shared.applied.lock().unwrap().is_none());
        assert!(!shared.ready.load(Ordering::Relaxed));
    }

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
