//! Triode — a real-time guitar amplifier.
//!
//! Signal path: `input → effects rack → preamp/tone stack → power amp → cab sim → limiter → output`.
//!
//! Everything audio-thread-facing obeys three rules, and they are the reason most
//! of the types here look more constrained than they strictly need to be:
//!
//! 1. no allocation on the audio thread (fixed scratch buffers, processors are
//!    built on the UI thread and moved in),
//! 2. no blocking on a lock the UI thread holds (the command mailbox is drained
//!    with `try_lock`; a busy mailbox means "process the previous block again"),
//! 3. no unbounded parameter jump (params are smoothed once per chunk, and every
//!    inbound value is clamped and required to be finite — a NaN in a biquad pole
//!    is a permanent output blowout, so that check is load-bearing).

pub mod audio_io;
pub mod dsp;
pub mod engine;
pub mod params;
pub mod preset;
pub mod render;
pub mod selftest;
pub mod taps;
pub mod ui;
