//! The engine: rack of slots, the command mailbox, and the shared state between the
//! UI thread and the audio thread.
//!
//! ## Why the mailbox looks over-built
//!
//! The UI thread only ever *pushes* [`Cmd`]s; the audio callback drains them with
//! `try_lock`. If the lock is momentarily held the audio thread processes the previous
//! block again rather than waiting — a busy mailbox costs one block of latency, never a
//! dropout and never a priority inversion.
//!
//! ## Why parameters and structure are separate
//!
//! A knob move sends [`Cmd::SetParam`], which touches three floats. It never rebuilds a
//! slot, so delay lines, reverb tails and envelope state survive every edit. Only
//! [`Cmd::InsertSlot`]/[`Cmd::RemoveSlot`]/[`Cmd::MoveSlot`] change the `Vec<Slot>`, and
//! they *move* the existing `Box<dyn Proc>` instead of recreating it, so reordering the
//! rack keeps each pedal's internal state.
//!
//! ## Why insertion carries a built slot
//!
//! `Box<dyn Proc>` (a reverb's comb buffers are a few hundred KB) is allocated on the UI
//! thread and moved into the queue. The audio thread never allocates.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Instant;

use crate::dsp::amp::Amp;
use crate::dsp::biquad::{Biquad, Coef, DcBlocker};
use crate::dsp::cab::{Cab, Ir};
use crate::dsp::limiter::Limiter;
use crate::dsp::meter::Meter;
use crate::dsp::resampler::{Resampler, Source};
use crate::dsp::{db2lin, sanitize, Frame, MAX_CHUNK};
use crate::params::{amp_ix, ParamVals, AMP_SPECS, MAX_SLOTS};

mod slot;
pub use slot::{make_proc, Proc, Slot};

#[cfg(test)]
mod tests;

/// Which non-rack switch a [`Cmd::Flag`] refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flag {
    Cab,
    InputHpf,
    Muted,
}

/// A request to rebuild the audio streams (device or buffer-size change).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReopenReq {
    pub input: Option<String>,
    pub output: Option<String>,
    pub buffer_ms: u32,
    /// Open the input stream at all. `false` means output-only: the amp is live and the
    /// test tone still drives the wave view, but nothing is captured -- which is the only
    /// reliable way to avoid a howl on the very common laptop-speakers-plus-laptop-mic
    /// desk. The UI arms it with one click.
    pub input_on: bool,
}

/// One UI-initiated change. Non-allocating except for a pre-built slot or an IR.
pub enum Cmd {
    SetParam {
        slot: usize,
        idx: usize,
        norm: f32,
    },
    SetEnabled {
        slot: usize,
        on: bool,
    },
    /// Insert a slot built on the UI thread at index `at` (clamped).
    InsertSlot {
        at: usize,
        slot: Slot,
    },
    RemoveSlot {
        slot: usize,
    },
    MoveSlot {
        from: usize,
        to: usize,
    },
    /// Replace the kind at `slot`, keeping its position. State is intentionally new.
    ReplaceSlot {
        slot: usize,
        with: Slot,
    },
    AmpParam {
        idx: usize,
        norm: f32,
    },
    Flag(Flag, bool),
    /// Complete cabinet prepared off-thread, including convolution buffers.
    LoadCab(Box<Cab>),
    /// Replace the whole rack at once (preset load). Built on the UI thread for the same
    /// reason as [`Cmd::InsertSlot`], and truncated to [`MAX_SLOTS`] on the way in.
    LoadRack(Vec<Slot>),
}

fn put(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}
fn get(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// Lock-free read-only statistics, published by the audio thread for the UI.
#[derive(Debug, Default)]
pub struct Stats {
    pub in_peak: AtomicU32,
    pub in_rms: AtomicU32,
    pub out_peak: AtomicU32,
    pub out_rms: AtomicU32,
    pub gr_db: AtomicU32,
    pub cpu_pct: AtomicU32,
    /// Engine sample rate as an integer Hz.
    pub rate: AtomicU32,
    /// Measured input/output step ratio (1.0 when the clocks agree).
    pub ratio: AtomicU32,
    pub buffer_frames: AtomicU32,
    pub ring_frames: AtomicU32,
    pub clip_in: AtomicU64,
    pub clip_out: AtomicU64,
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub dropped_in: AtomicU64,
}

/// Plain-data view of [`Stats`] for the UI.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatsSnap {
    pub in_peak: f32,
    pub in_rms: f32,
    pub out_peak: f32,
    pub out_rms: f32,
    pub gr_db: f32,
    pub cpu_pct: f32,
    pub rate: u32,
    pub ratio: f32,
    pub buffer_frames: u32,
    pub ring_frames: u32,
    pub clip_in: u64,
    pub clip_out: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub dropped_in: u64,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnap {
        StatsSnap {
            in_peak: get(&self.in_peak),
            in_rms: get(&self.in_rms),
            out_peak: get(&self.out_peak),
            out_rms: get(&self.out_rms),
            gr_db: get(&self.gr_db),
            cpu_pct: get(&self.cpu_pct),
            rate: self.rate.load(Ordering::Relaxed),
            ratio: get(&self.ratio),
            buffer_frames: self.buffer_frames.load(Ordering::Relaxed),
            ring_frames: self.ring_frames.load(Ordering::Relaxed),
            clip_in: self.clip_in.load(Ordering::Relaxed),
            clip_out: self.clip_out.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            overruns: self.overruns.load(Ordering::Relaxed),
            dropped_in: self.dropped_in.load(Ordering::Relaxed),
        }
    }

    pub fn reset_clips(&self) {
        self.clip_in.store(0, Ordering::Relaxed);
        self.clip_out.store(0, Ordering::Relaxed);
    }
}

/// How many frames the scope keeps, per channel. About 45 ms at 48 kHz, which is long
/// enough to see a few cycles of a low E and short enough to draw every frame.
pub const SCOPE_FRAMES: usize = 2048;

/// Rolling window of the signal, for the UI's scope and transfer plot.
///
/// Deliberately plain `VecDeque`s behind one mutex rather than a lock-free ring: the
/// audio thread fills it with `try_lock` and simply skips a block when the UI is reading,
/// so a stalled UI costs a dropped waveform frame, never a glitched audio callback.
#[derive(Debug)]
pub struct Scope {
    pub din: VecDeque<f32>,
    pub dout: VecDeque<f32>,
    /// Sample rate the window was captured at, so the UI can label the time axis.
    pub sr: f32,
    /// Bumped on every block published, so the UI can tell a live trace from a frozen one.
    pub seq: u64,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            din: VecDeque::with_capacity(SCOPE_FRAMES),
            dout: VecDeque::with_capacity(SCOPE_FRAMES),
            sr: 0.0,
            seq: 0,
        }
    }
}

/// State shared between the UI, the audio-thread owner, and both cpal callbacks.
pub struct Shared {
    pub cmds: Mutex<VecDeque<Cmd>>,
    // Fixed-capacity return path: callback hands ownership back, never frees it.
    retired: Mutex<Vec<Cmd>>,
    pub applied: Mutex<Option<ReopenReq>>,
    pub stats: Stats,
    /// Set by the audio-thread owner once streams are live.
    pub ready: AtomicBool,
    pub quit: AtomicBool,
    /// Pending reopen request, consumed by the thread that owns the `Stream`s.
    pub reopen: Mutex<Option<ReopenReq>>,
    /// Last error or note worth showing in the UI (written rarely).
    pub status: Mutex<String>,
    /// Waveform window, written by the audio thread, read by the UI.
    pub scope: Mutex<Scope>,
    /// Cleared when the UI is hidden or the user hits freeze; the engine then skips the
    /// capture entirely rather than copying samples nobody looks at.
    pub scope_on: AtomicBool,
    /// Built-in test tone in Hz, `0` = off. Lives here so the UI can set it without
    /// reaching into the audio thread; `audio_io` mixes it into the engine's input.
    pub tone_hz: AtomicU32,
}

const MAX_COMMANDS: usize = 64;

impl Default for Shared {
    fn default() -> Self {
        let shared = Self {
            cmds: Mutex::new(VecDeque::new()),
            retired: Mutex::new(Vec::with_capacity(MAX_COMMANDS)),
            applied: Mutex::new(None),
            stats: Stats::default(),
            ready: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            reopen: Mutex::new(None),
            status: Mutex::new(String::new()),
            scope: Mutex::new(Scope::default()),
            scope_on: AtomicBool::new(true),
            tone_hz: AtomicU32::new(0),
        };
        // macOS std mutexes allocate their native lock on first use, even try_lock.
        // Warm every callback-facing lock here, before either audio stream can run.
        drop(shared.cmds.lock().expect("new command mutex"));
        drop(shared.retired.lock().expect("new retirement mutex"));
        drop(shared.scope.lock().expect("new scope mutex"));
        shared
    }
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared::default())
    }

    /// UI/audio-owner thread only. Drop retired processors outside the callback.
    pub fn collect_retired(&self) {
        self.retired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn load_ir(&self, ir: Ir) {
        let mut cab = Cab::new(self.preparation_rate());
        cab.load_ir(ir);
        self.send(Cmd::LoadCab(Box::new(cab)));
    }

    pub fn clear_ir(&self) {
        self.send(Cmd::LoadCab(Box::new(Cab::new(self.preparation_rate()))));
    }

    fn preparation_rate(&self) -> f32 {
        match self.stats.rate.load(Ordering::Relaxed) {
            0 => 44100.0,
            sr => sr as f32,
        }
    }

    pub fn set_scope(&self, on: bool) {
        // Not a Cmd: this only turns observation on or off, and the audio thread reads it
        // atomically, so routing it through the mailbox would add a block of latency to a
        // control that has no effect on the sound.
        self.scope_on.store(on, Ordering::Relaxed);
    }

    /// Test-tone frequency in Hz; `0` turns it off.
    pub fn set_tone(&self, hz: f32) {
        self.tone_hz.store(hz.max(0.0).to_bits(), Ordering::Relaxed);
    }

    pub fn tone(&self) -> f32 {
        f32::from_bits(self.tone_hz.load(Ordering::Relaxed))
    }

    /// A copy of the waveform window, or `None` when the audio thread is mid-write.
    /// Never blocks, which is the whole point of the `try_lock`.
    pub fn scope_snap(&self) -> Option<(Vec<f32>, Vec<f32>, f32, u64)> {
        let sc = self.scope.try_lock().ok()?;
        Some((
            sc.din.iter().copied().collect(),
            sc.dout.iter().copied().collect(),
            sc.sr,
            sc.seq,
        ))
    }

    pub fn send(&self, mut cmd: Cmd) {
        self.collect_retired();
        if let Cmd::LoadRack(slots) = &mut cmd {
            // Preparation belongs to the caller, not the callback. Preserve room for edits.
            slots.truncate(MAX_SLOTS);
            slots.reserve(MAX_SLOTS - slots.len());
        }
        // Lock only long enough to push a small enum; never while building anything.
        // A poisoned `Mutex<VecDeque>` is still sound to use: the guard's panic left the
        // queue intact, not structurally broken. Clearing it here would silently discard
        // every queued knob move, so take the inner queue and keep pushing.
        match self.cmds.lock() {
            Ok(mut q) => q.push_back(cmd),
            Err(e) => e.into_inner().push_back(cmd),
        }
    }

    pub fn set_status(&self, msg: impl AsRef<str>) {
        // One lock, poison-tolerant. The previous shape took the lock in the `if let` and
        // then locked the *same* mutex again in its `else` arm (the guard lives for the
        // whole if/else), so the poisoned-lock recovery path deadlocked instead of
        // recovering -- i.e. a panicking UI task would hang the app on the next status
        // write, which is exactly the moment the UI wants to report the failure.
        let mut s = self.status.lock().unwrap_or_else(|e| e.into_inner());
        *s = msg.as_ref().to_string();
    }

    pub fn status(&self) -> String {
        match self.status.lock() {
            Ok(s) => s.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }
}

/// Ceiling the safety limiter works against, leaving room for a few samples of attack
/// overshoot below full scale.
pub const CEILING: f32 = 0.95;

/// Hard bound on what ever leaves the engine. `CEILING` is a target the limiter's finite
/// attack overshoots; this is the number that is actually true of every output sample.
pub const FULL_SCALE: f32 = 1.0;

/// The live signal chain. Owned by the output callback; `process` is the only entry.
pub struct Engine {
    pub slots: Vec<Slot>,
    pub amp: Amp,
    pub cab: Box<Cab>,
    limiter: Limiter,
    out_dc: [DcBlocker; 2],
    in_dc: DcBlocker,
    in_hpf: Biquad,
    amp_target: ParamVals,
    amp_smooth: ParamVals,
    amp_values: ParamVals,
    cab_on: bool,
    hpf_on: bool,
    muted: bool,
    scratch: [[f32; 2]; MAX_CHUNK],
    /// Mono summaries of the block currently in flight, captured either side of the
    /// rack so the UI can plot input against output (which is what shows clipping).
    cap_in: [f32; MAX_CHUNK],
    cap_out: [f32; MAX_CHUNK],
    cap_n: usize,
    resampler: Resampler,
    step_base: f32,
    step: f32,
    meter_in: Meter,
    meter_out: Meter,
    sr: f32,
    in_sr: f32,
    chunk: usize,
    overruns: u64,
    /// Input frames seen *consecutively* with nothing in the ring (dead mic / no
    /// permission). Resets the moment input returns, so the UI's warning describes now
    /// rather than the whole session.
    silent_run: u64,
    /// Longest such gap since start-up; what offline reports measure.
    silent_max: u64,
    /// Per-stage recording, off unless a trace asked for it. See [`crate::taps`].
    taps: Option<crate::taps::TapLog>,
}

impl Engine {
    pub fn new(sr: f32, chunk: usize) -> Engine {
        let defaults: [f32; 7] = std::array::from_fn(|i| AMP_SPECS[i].default_norm());
        let norms = ParamVals::from_norms(&defaults);
        let mut e = Engine {
            slots: Vec::new(),
            amp: Amp::new(sr),
            cab: Box::new(Cab::new(sr)),
            limiter: Limiter::new(),
            out_dc: [DcBlocker::new(), DcBlocker::new()],
            in_dc: DcBlocker::new(),
            in_hpf: Biquad::new(Coef::highpass(75.0, 0.707, sr)),
            amp_target: norms,
            amp_smooth: norms,
            amp_values: ParamVals::ZEROED,
            cab_on: true,
            hpf_on: true,
            muted: false,
            scratch: [[0.0; 2]; MAX_CHUNK],
            cap_in: [0.0; MAX_CHUNK],
            cap_out: [0.0; MAX_CHUNK],
            cap_n: 0,
            resampler: Resampler::new(),
            step_base: 1.0,
            step: 1.0,
            meter_in: Meter::new(),
            meter_out: Meter::new(),
            sr,
            in_sr: sr,
            chunk: chunk.clamp(16, MAX_CHUNK),
            overruns: 0,
            silent_run: 0,
            silent_max: 0,
            taps: None,
        };
        // Reserved up front so Vec<Slot> never reallocates on the audio thread.
        e.slots.reserve(MAX_SLOTS);
        e.set_rates(sr, sr);
        for (i, spec) in AMP_SPECS.iter().enumerate() {
            e.amp_values.v[i] = spec.denorm(norms.v[i]);
        }
        e
    }

    /// The engine runs at the *output* device's rate; input is resampled to it.
    pub fn set_rates(&mut self, in_rate: f32, out_rate: f32) {
        self.sr = sanitize(out_rate, 48000.0, 8000.0, 192000.0);
        self.in_sr = sanitize(in_rate, self.sr, 8000.0, 192000.0);
        self.step_base = self.in_sr / self.sr;
        self.step = self.step_base;
        self.resampler.reset();
        self.resampler.set_step(self.step);
        self.in_dc.reset();
        self.in_hpf.reset();
        self.amp.set_rates(self.sr);
        self.cab.set_rates(self.sr);
        self.limiter.set_rates(self.sr);
        for dc in &mut self.out_dc {
            dc.set_hz(20.0, self.sr);
        }
        self.in_dc.set_hz(15.0, self.sr);
        self.in_hpf.set(Coef::highpass(75.0, 0.707, self.sr));
        for s in self.slots.iter_mut() {
            s.proc.set_rates(self.sr);
        }
    }

    pub fn sr(&self) -> f32 {
        self.sr
    }

    pub fn in_sr(&self) -> f32 {
        self.in_sr
    }

    pub fn set_chunk(&mut self, chunk: usize) {
        self.chunk = chunk.clamp(16, MAX_CHUNK);
    }

    pub fn chunk(&self) -> usize {
        self.chunk
    }

    pub fn slots(&self) -> &[Slot] {
        &self.slots
    }

    /// How many slots are currently audible.
    pub fn active_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.enabled).count()
    }

    pub fn amp_values(&self) -> ParamVals {
        self.amp_values
    }

    /// Drain and apply pending UI commands. Bounded so a pathological queue cannot stall
    /// the callback; leftovers are picked up next block.
    fn drain(&mut self, shared: &Shared) {
        let mut queue = match shared.cmds.try_lock() {
            Ok(q) => q,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
        };
        let mut retired = match shared.retired.try_lock() {
            Ok(r) => r,
            Err(TryLockError::WouldBlock) => return,
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
        };
        for _ in 0..MAX_COMMANDS {
            // Keep the command queued if its return value might not fit. No lost edits.
            if retired.len() == retired.capacity() {
                break;
            }
            let Some(cmd) = queue.pop_front() else { break };
            if let Some(old) = self.apply(cmd) {
                retired.push(old);
            }
        }
    }

    fn apply(&mut self, cmd: Cmd) -> Option<Cmd> {
        match cmd {
            Cmd::SetParam { slot, idx, norm } => {
                if let Some(s) = self.slots.get_mut(slot) {
                    if idx < s.kind.params().len() {
                        // `norm` is already a knob position; clamp it, do NOT run it
                        // through spec.norm() — that would normalise a second time.
                        s.target.v[idx] = sanitize(norm, s.target.v[idx], 0.0, 1.0);
                    }
                }
            }
            Cmd::SetEnabled { slot, on } => {
                if let Some(s) = self.slots.get_mut(slot) {
                    s.enabled = on;
                }
            }
            Cmd::InsertSlot { at, slot } => {
                if self.slots.len() < MAX_SLOTS {
                    let at = at.min(self.slots.len());
                    let mut slot = slot;
                    slot.proc.set_rates(self.sr);
                    self.slots.insert(at, slot);
                } else {
                    return Some(Cmd::InsertSlot { at, slot });
                }
            }
            Cmd::RemoveSlot { slot } => {
                if slot < self.slots.len() {
                    return Some(Cmd::InsertSlot {
                        at: slot,
                        slot: self.slots.remove(slot),
                    });
                }
            }
            Cmd::MoveSlot { from, to } => {
                if from < self.slots.len() {
                    let to = to.min(self.slots.len() - 1);
                    let s = self.slots.remove(from);
                    self.slots.insert(to, s);
                }
            }
            Cmd::LoadRack(slots) => {
                let mut slots = slots;
                for s in slots.iter_mut() {
                    s.proc.set_rates(self.sr);
                }
                return Some(Cmd::LoadRack(std::mem::replace(&mut self.slots, slots)));
            }
            Cmd::ReplaceSlot { slot, with } => {
                if slot < self.slots.len() {
                    let mut with = with;
                    with.proc.set_rates(self.sr);
                    let old = std::mem::replace(&mut self.slots[slot], with);
                    return Some(Cmd::ReplaceSlot { slot, with: old });
                }
                return Some(Cmd::ReplaceSlot { slot, with });
            }
            Cmd::AmpParam { idx, norm } => self.set_amp_param(idx, norm),
            Cmd::Flag(flag, on) => match flag {
                Flag::Cab => self.cab_on = on,
                Flag::InputHpf => self.hpf_on = on,
                Flag::Muted => self.muted = on,
            },
            Cmd::LoadCab(mut cab) => {
                cab.set_rates(self.sr);
                return Some(Cmd::LoadCab(std::mem::replace(&mut self.cab, cab)));
            }
        }
        None
    }

    /// Process `out.len()` output frames: pull input, run the chain, write out.
    ///
    /// Called from the output callback. `src` is the input side — a ring in the live app,
    /// a slice in `--render` — which is why the engine does not care where audio comes
    /// from. `shared` is `None` for the offline renderer.
    pub fn process(&mut self, shared: Option<&Shared>, src: &mut dyn Source, out: &mut [Frame]) {
        let t0 = shared.map(|_| Instant::now());
        if let Some(shared) = shared {
            self.drain(shared);
        }
        let need = out.len();
        let mut done = 0;
        self.cap_n = 0;
        while done < need {
            let n = (need - done).min(self.chunk);
            self.advance_amp(n);
            self.fill_scratch(src, n);
            // Captured before the rack, because `run_chain` overwrites the scratch in
            // place and the input side is gone by the time it returns.
            let c = self.cap_n.min(MAX_CHUNK);
            let take = n.min(MAX_CHUNK - c);
            if take > 0 {
                for i in 0..take {
                    let f = self.scratch[i];
                    self.cap_in[c + i] = 0.5 * (f[0] + f[1]);
                }
            }
            self.run_chain(n);
            let c2 = self.cap_n.min(MAX_CHUNK);
            let take2 = n.min(MAX_CHUNK - c2);
            for i in 0..take2 {
                let f = self.scratch[i];
                self.cap_out[c2 + i] = 0.5 * (f[0] + f[1]);
            }
            self.cap_n = (self.cap_n + n).min(MAX_CHUNK);
            out[done..done + n].copy_from_slice(&self.scratch[..n]);
            done += n;
        }
        if let (Some(t0), Some(shared)) = (t0, shared) {
            // Overrun = the callback outlasted its own block, i.e. the next one is late.
            let budget = need as f64 / self.sr.max(1.0) as f64;
            let spent = t0.elapsed().as_secs_f64();
            put(
                &shared.stats.cpu_pct,
                (spent / budget.max(1e-9) * 100.0) as f32,
            );
            if spent > budget {
                self.overruns += 1;
                shared
                    .stats
                    .overruns
                    .store(self.overruns, Ordering::Relaxed);
            }
        }
        self.publish(shared, src);
    }

    /// Smooth + denormalise the amp/global params for this chunk. Done before
    /// `fill_scratch` so the input trim applies to *this* chunk, not last's.
    fn advance_amp(&mut self, n: usize) {
        let k = 1.0 - (-((n as f32) / (0.015 * self.sr.max(1.0))).exp());
        for (i, spec) in AMP_SPECS.iter().enumerate() {
            let t = self.amp_target.get(spec, i);
            let cur = self.amp_smooth.v[i].clamp(0.0, 1.0);
            self.amp_smooth.v[i] = cur + (t - cur) * k;
            self.amp_values.v[i] = spec.denorm(self.amp_smooth.v[i]);
        }
    }

    /// Resample `n` input frames into the stereo scratch buffer and gain-stage them.
    fn fill_scratch(&mut self, src: &mut dyn Source, n: usize) {
        // Two devices on independent clocks drift apart, so the input resampler's ratio
        // is nudged to keep the ring centred on about half a block. Bounded to ±0.1 %,
        // which is far below audible pitch change for a guitar.
        let target = (self.sr * 0.005).max(32.0);
        let level = src.level();
        if let Some(clock_level) = src.clock_level() {
            let err = (clock_level as f32 - target) / (target * 8.0);
            let corr = 1.0 + err.clamp(-0.001, 0.001);
            self.step =
                (self.step_base * corr).clamp(self.step_base * 0.999, self.step_base * 1.001);
        } else {
            self.step = self.step_base;
        }
        self.resampler.set_step(self.step);
        if level == 0 {
            self.silent_run = self.silent_run.saturating_add(n as u64);
            self.silent_max = self.silent_max.max(self.silent_run);
        } else {
            self.silent_run = 0;
        }

        let trim = db2lin(self.amp_values.v[amp_ix::TRIM]);
        for f in self.scratch[..n].iter_mut() {
            let raw = self.resampler.next(src);
            // Stage 0, recorded at the device's level. "Is a guitar plugged in and open"
            // cannot be answered after our own trim, which turns silence loud and a hot
            // pick-up silent; and it must be this sample, not the previous block's.
            if let Some(t) = self.taps.as_mut() {
                t.push_mono(0, raw);
            }
            let mut x = raw * trim;
            x = self.in_dc.process(x);
            if self.hpf_on {
                x = self.in_hpf.process(x);
            }
            // Clamp ahead of the rack: a clipped-at-source interface or a 40 dB trim
            // should saturate the amp, not send +1e12 into a reverb comb.
            let x = if x.is_finite() {
                x.clamp(-8.0, 8.0)
            } else {
                0.0
            };
            self.meter_in.push(x);
            *f = [x, x]; // a guitar is mono; time-based effects widen it later
        }
    }

    fn run_chain(&mut self, n: usize) {
        let buf = &mut self.scratch[..n];
        let k = 1.0 - (-((n as f32) / (0.015 * self.sr.max(1.0))).exp());

        // A disabled slot still gets its stage recorded (unchanged from the previous one):
        // every stage must hold the same number of frames or the per-stage WAVs slide apart.
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.enabled {
                slot.advance(k);
                slot.proc.process(buf, n, &slot.values);
            }
            if let Some(t) = self.taps.as_mut() {
                for f in buf.iter() {
                    t.push(1 + i, f);
                }
            }
        }

        self.amp.process(buf, n, &self.amp_values);
        let after_slots = 1 + self.slots.len();
        if let Some(t) = self.taps.as_mut() {
            for f in buf.iter() {
                t.push(after_slots, f);
            }
        }
        if self.cab_on {
            self.cab.process(buf, n);
        }
        if let Some(t) = self.taps.as_mut() {
            for f in buf.iter() {
                t.push(after_slots + 1, f);
            }
        }

        // Master taper: quadratic in linear gain, silence at 0, ~+1.6 dB wide open.
        let master = if self.muted {
            0.0
        } else {
            let m = self.amp_values.v[amp_ix::MASTER].clamp(0.0, 1.0);
            m * m * 1.2
        };
        for f in buf.iter_mut() {
            f[0] *= master;
            f[1] *= master;
            self.limiter.process(f, CEILING);
            f[0] = self.out_dc[0].process(f[0]);
            f[1] = self.out_dc[1].process(f[1]);
            // Brickwall, deliberately last. The limiter above clamps to `CEILING`, but the
            // DC blocker runs after it and is a highpass: on a clamped transient it can ring
            // and put a sample back over the ceiling. This is what makes "no output sample
            // exceeds full scale" true by construction rather than by trusting the stage
            // order. (Its original comment cited a 0.976 overshoot that was measured while
            // `set_amp_param` was mis-setting the amp -- a broken engine, so that figure is
            // withdrawn; the ordering reason above stands on its own.)
            f[0] = f[0].clamp(-FULL_SCALE, FULL_SCALE);
            f[1] = f[1].clamp(-FULL_SCALE, FULL_SCALE);
            // Mute is the final safety switch, including residual DC-filter history.
            if self.muted {
                *f = [0.0; 2];
            }
            self.meter_out.push_frame(f[0], f[1]);
            if let Some(t) = self.taps.as_mut() {
                t.push(after_slots + 2, f);
            }
        }
    }

    fn publish(&mut self, shared: Option<&Shared>, src: &dyn Source) {
        let Some(shared) = shared else { return };
        let s = &shared.stats;
        let (ip, ir, ic) = self.meter_in.take();
        let (op, orm, oc) = self.meter_out.take();
        put(&s.in_peak, ip);
        put(&s.in_rms, ir);
        put(&s.out_peak, op);
        put(&s.out_rms, orm);
        put(&s.gr_db, self.limiter.reduction_db());
        s.rate.store(self.sr as u32, Ordering::Relaxed);
        put(&s.ratio, self.step / self.step_base.max(f32::MIN_POSITIVE));
        s.ring_frames.store(src.level() as u32, Ordering::Relaxed);
        if ic {
            s.clip_in.fetch_add(1, Ordering::Relaxed);
        }
        if oc {
            s.clip_out.fetch_add(1, Ordering::Relaxed);
        }
        s.underruns
            .store(self.resampler.underruns(), Ordering::Relaxed);
        self.publish_scope(shared);
    }

    /// Hand this block's captured window to the UI.
    ///
    /// `try_lock`, not `lock`: if the UI happens to be copying the window, this block is
    /// simply not plotted. The alternative -- waiting -- puts an unbounded block on the
    /// audio callback for the sake of a picture, which is exactly the trade the rest of
    /// this file refuses to make.
    fn publish_scope(&mut self, shared: &Shared) {
        if !shared.scope_on.load(Ordering::Relaxed) || self.cap_n == 0 {
            return;
        }
        let Ok(mut sc) = shared.scope.try_lock() else {
            return;
        };
        let n = self.cap_n;
        for i in 0..n {
            if sc.din.len() >= SCOPE_FRAMES {
                sc.din.pop_front();
                sc.dout.pop_front();
            }
            sc.din.push_back(self.cap_in[i]);
            sc.dout.push_back(self.cap_out[i]);
        }
        sc.sr = self.sr;
        sc.seq = sc.seq.wrapping_add(1);
    }

    /// Consecutive blocks that arrived with an empty input ring. The UI turns this into
    /// "check microphone permission" — on macOS a CLI-launched binary inherits its
    /// terminal's permission, and a denial is silence rather than an error.
    pub fn silent_frames(&self) -> u64 {
        self.silent_run
    }

    /// The longest input gap seen since start-up, in frames.
    pub fn silent_max(&self) -> u64 {
        self.silent_max
    }

    /// Start recording every stage boundary, each up to `limit` frames.
    ///
    /// The stage list is fixed from the rack as it stands now: `input`, one per slot, `amp`,
    /// `cab`, `out`. Adding or removing a slot afterwards shifts the indices, so this is a
    /// trace/bring-up affordance, not something to leave on while editing; `TapLog` bounds
    /// every push, so a rack change loses at worst a stage rather than sound.
    pub fn enable_taps(&mut self, limit: usize) {
        let mut names: Vec<String> = Vec::with_capacity(self.slots.len() + 3);
        names.push("input".into());
        for (i, s) in self.slots.iter().enumerate() {
            names.push(format!("{}.{}", i, s.kind.short()));
        }
        names.push("amp".into());
        names.push("cab".into());
        names.push("out".into());
        // Leaked on purpose: a handful of stage names, once per trace. Interning them in
        // TapStage as `String` would mean threading lifetimes through a debug affordance.
        let names: Vec<&'static str> = names
            .into_iter()
            .map(|n| Box::leak(n.into_boxed_str()) as &'static str)
            .collect();
        self.taps = Some(crate::taps::TapLog::new(&names, limit));
    }

    /// Frames currently recorded per stage (0 when taps are off).
    pub fn tap_len(&self) -> usize {
        self.taps.as_ref().map_or(0, |t| t.len())
    }

    /// The recorder itself, for measuring or writing out after a run.
    pub fn taps(&self) -> Option<&crate::taps::TapLog> {
        self.taps.as_ref()
    }

    pub fn overruns(&self) -> u64 {
        self.overruns
    }

    pub fn limiter(&self) -> &Limiter {
        &self.limiter
    }

    /// Initial rack preparation, outside the callback. Live edits use `Cmd::LoadRack`.
    pub fn load_rack(&mut self, slots: Vec<Slot>) {
        self.slots.clear();
        for mut s in slots.into_iter().take(MAX_SLOTS) {
            s.proc.set_rates(self.sr);
            self.slots.push(s);
        }
    }

    /// Move an amp knob to a **normalised** position (`0.0..=1.0`) -- the same units the UI
    /// sends and the same units `amp_target` stores.
    ///
    /// This used to be `spec.norm(norm)`, which normalised a value that was already
    /// normalised. On the linear knobs that merely read wrong; on `master`, whose curve is
    /// exponential over `0.0..=1.0`, 0.3 came back as ~0.8 -- about +8 dB straight into the
    /// power-amp shaper, which then squared off on any real input. The UI calls this setter
    /// directly when it pushes its knob state, so it was audible in the live app, not just
    /// in offline renders. Keep this and the mailbox path sharing one definition.
    pub fn set_amp_param(&mut self, idx: usize, norm: f32) {
        if let Some(spec) = AMP_SPECS.get(idx) {
            self.amp_target.v[idx] = sanitize(norm, spec.default_norm(), 0.0, 1.0);
        }
    }

    pub fn flags(&self) -> (bool, bool, bool) {
        (self.cab_on, self.hpf_on, self.muted)
    }
}
