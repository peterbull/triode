//! Single-producer / single-consumer f32 ring buffer.
//!
//! Two independent CoreAudio devices (a microphone and a pair of headphones) run on
//! their own clocks, so the input callback and the output callback have to hand audio
//! to each other without a lock. A mutex here would mean the audio thread can be
//! descheduled while waiting on a UI-side lock, and `try_lock` would mean dropping a
//! block (an audible click) whenever that happens, so this is a proper SPSC ring.
//!
//! Only one thread writes `head`, only one writes `tail`; the atomic pair provides the
//! happens-before edge that makes the payload bytes visible. Indices grow monotonically
//! and are masked on access, so "full" and "empty" are distinguishable without a flag.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::dsp::next_pow2;

const PRODUCER: u8 = 1;
const CONSUMER: u8 = 2;
const RESETTING: u8 = 4;

struct Inner {
    /// Each slot has its own interior-mutability boundary. The producer only writes slots
    /// outside `[tail, head)`, while the consumer only reads slots inside that range.
    buf: Box<[UnsafeCell<f32>]>,
    mask: usize,
    /// Written by the producer only; read by the consumer.
    head: AtomicUsize,
    /// Written by the consumer only; read by the producer.
    tail: AtomicUsize,
    /// Runtime enforcement of the SPSC contract, plus exclusive reset access.
    endpoints: AtomicU8,
}

unsafe impl Sync for Inner {}
unsafe impl Send for Inner {}

/// Shared ring. Claim one [`Ring::producer`] and one [`Ring::consumer`].
#[derive(Clone)]
pub struct Ring {
    inner: Arc<Inner>,
}

impl Ring {
    pub fn new(capacity: usize) -> Ring {
        let cap = next_pow2(capacity);
        Ring {
            inner: Arc::new(Inner {
                buf: (0..cap)
                    .map(|_| UnsafeCell::new(0.0))
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
                mask: cap - 1,
                head: AtomicUsize::new(0),
                tail: AtomicUsize::new(0),
                endpoints: AtomicU8::new(0),
            }),
        }
    }

    /// Producer half (move this to the input callback).
    pub fn producer(&self) -> Prod {
        self.claim(PRODUCER, "producer");
        Prod {
            inner: self.inner.clone(),
            scratch: 0,
        }
    }

    /// Consumer half (move this to the output callback).
    pub fn consumer(&self) -> Cons {
        self.claim(CONSUMER, "consumer");
        Cons {
            inner: self.inner.clone(),
        }
    }

    fn claim(&self, endpoint: u8, name: &str) {
        let claimed =
            self.inner
                .endpoints
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    (state & (endpoint | RESETTING) == 0).then_some(state | endpoint)
                });
        assert!(claimed.is_ok(), "ring {name} already active");
    }

    pub fn capacity(&self) -> usize {
        self.inner.mask + 1
    }

    /// Reset both indices. Only valid while no callback is running — used when the
    /// streams are torn down for a device change, when cpal has already guaranteed
    /// both callbacks have returned.
    pub fn reset(&self) {
        assert!(
            self.inner
                .endpoints
                .compare_exchange(0, RESETTING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "cannot reset a ring with active endpoints"
        );
        self.inner.head.store(0, Ordering::Relaxed);
        self.inner.tail.store(0, Ordering::Relaxed);
        for slot in self.inner.buf.iter() {
            // SAFETY: RESETTING excludes both endpoint issuance and active callbacks.
            unsafe { *slot.get() = 0.0 };
        }
        self.inner.endpoints.store(0, Ordering::Release);
    }
}

/// Producer half. `Send` because exactly one thread owns it at a time.
pub struct Prod {
    inner: Arc<Inner>,
    scratch: usize,
}

unsafe impl Send for Prod {}

impl Drop for Prod {
    fn drop(&mut self) {
        self.inner.endpoints.fetch_and(!PRODUCER, Ordering::Release);
    }
}

impl Prod {
    /// Append as much of `data` as fits; returns how many frames were written.
    /// Dropping the remainder is the intended overflow behaviour (never block).
    pub fn push(&mut self, data: &[f32]) -> usize {
        let inner = &*self.inner;
        let head = inner.head.load(Ordering::Relaxed);
        let tail = inner.tail.load(Ordering::Acquire);
        let cap = inner.mask + 1;
        let free = cap - head.wrapping_sub(tail);
        let n = free.min(data.len());
        self.scratch = self.scratch.saturating_add(data.len() - n);
        if n == 0 {
            return 0;
        }
        let first = head & inner.mask;
        for (offset, sample) in data[..n].iter().enumerate() {
            // SAFETY: the single producer owns every free slot until the Release-store of
            // head publishes it; the consumer cannot read this slot before that store.
            unsafe { *inner.buf[(first + offset) & inner.mask].get() = *sample };
        }
        inner.head.store(head.wrapping_add(n), Ordering::Release);
        n
    }

    /// Frames dropped because the ring was full (a metric worth surfacing).
    pub fn dropped(&self) -> usize {
        self.scratch
    }
}

/// Consumer half.
pub struct Cons {
    inner: Arc<Inner>,
}

unsafe impl Send for Cons {}

impl Drop for Cons {
    fn drop(&mut self) {
        self.inner.endpoints.fetch_and(!CONSUMER, Ordering::Release);
    }
}

impl Cons {
    /// Read up to `dst.len()` frames; returns how many were read (0 = starved).
    pub fn pop(&mut self, dst: &mut [f32]) -> usize {
        let inner = &*self.inner;
        let tail = inner.tail.load(Ordering::Relaxed);
        let head = inner.head.load(Ordering::Acquire);
        let avail = head.wrapping_sub(tail);
        let n = avail.min(dst.len());
        if n == 0 {
            return 0;
        }
        let first = tail & inner.mask;
        for (offset, sample) in dst[..n].iter_mut().enumerate() {
            // SAFETY: the Acquire-load of head only exposes fully written slots, and the
            // single producer cannot reuse this slot until tail is Release-stored below.
            *sample = unsafe { *inner.buf[(first + offset) & inner.mask].get() };
        }
        inner.tail.store(tail.wrapping_add(n), Ordering::Release);
        n
    }

    /// Frames waiting to be read. Used by the drift corrector, so a stale read is fine.
    pub fn level(&self) -> usize {
        let head = self.inner.head.load(Ordering::Acquire);
        let tail = self.inner.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::analysis::mean;

    #[test]
    fn cursor_overflow_preserves_capacity_and_fifo() {
        let ring = Ring::new(8);
        ring.inner.head.store(usize::MAX - 3, Ordering::Relaxed);
        ring.inner.tail.store(usize::MAX - 3, Ordering::Relaxed);
        let mut producer = ring.producer();
        let mut consumer = ring.consumer();
        assert_eq!(producer.push(&[1.0; 6]), 6);
        assert_eq!(consumer.level(), 6);
        assert_eq!(producer.push(&[2.0; 3]), 2);
        assert_eq!(producer.dropped(), 1);
        let mut out = [0.0; 8];
        assert_eq!(consumer.pop(&mut out), 8);
        assert_eq!(out, [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 2.0, 2.0]);
        assert_eq!(consumer.level(), 0);
        assert_eq!(producer.push(&[3.0; 8]), 8);
    }

    #[test]
    fn fifo_order_and_content_survive_wrapping() {
        let r = Ring::new(8); // 8 frames
        let mut p = r.producer();
        let mut c = r.consumer();
        // Push/pop in small pieces so the indices wrap several times.
        let mut expect: Vec<f32> = Vec::new();
        let mut got: Vec<f32> = Vec::new();
        let mut next = 0.0f32;
        for round in 0..200 {
            let k = (round % 5) + 1;
            let chunk: Vec<f32> = (0..k).map(|i| next + i as f32).collect();
            next += k as f32;
            let w = p.push(&chunk);
            expect.extend_from_slice(&chunk[..w]);
            let mut dst = vec![0.0f32; (round % 3) + 1];
            let n = c.pop(&mut dst);
            got.extend_from_slice(&dst[..n]);
        }
        let mut tail = Vec::new();
        let mut dst = vec![0.0f32; 8];
        loop {
            let n = c.pop(&mut dst);
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&dst[..n]);
        }
        got.extend(tail);
        assert!(!got.is_empty());
        assert_eq!(&got[..], &expect[..got.len()], "FIFO order broken");
    }

    #[test]
    fn full_ring_drops_instead_of_blocking() {
        let r = Ring::new(4);
        let mut p = r.producer();
        assert_eq!(p.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 4);
        assert_eq!(p.push(&[7.0]), 0, "ring is full");
        // Six offered to a 4-slot ring drops two, then the fully rejected push drops one.
        assert_eq!(p.dropped(), 3);
        let mut c = r.consumer();
        assert_eq!(c.level(), 4);
        let mut dst = [0.0f32; 4];
        assert_eq!(c.pop(&mut dst), 4);
        assert_eq!(dst, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!(c.level(), 0);
        assert_eq!(c.pop(&mut dst), 0);
    }

    #[test]
    fn empty_push_and_pop_are_harmless() {
        let r = Ring::new(16);
        let mut p = r.producer();
        let mut c = r.consumer();
        assert_eq!(p.push(&[]), 0);
        assert_eq!(c.pop(&mut []), 0);
        assert_eq!(c.level(), 0);
        assert_eq!(mean(&[]), 0.0);
    }

    #[test]
    fn duplicate_endpoints_are_rejected_and_reissued_after_drop() {
        let r = Ring::new(4);
        let p = r.producer();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.producer())).is_err());
        drop(p);
        let _replacement = r.producer();

        let c = r.consumer();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.consumer())).is_err());
        drop(c);
        let _replacement = r.consumer();
    }

    #[test]
    fn reset_requires_idle_endpoints_and_clears_buffer() {
        let r = Ring::new(4);
        let mut p = r.producer();
        assert_eq!(p.push(&[1.0, 2.0]), 2);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| r.reset())).is_err());
        drop(p);
        r.reset();
        let c = r.consumer();
        assert_eq!(c.level(), 0);
    }

    #[test]
    fn two_threads_move_every_sample_in_order() {
        const N: usize = 200_000;
        let r = Ring::new(1024);
        let mut p = r.producer();
        let mut c = r.consumer();
        // Arc, not &total: the consumer thread outlives this scope's borrow checker view,
        // so a plain reference cannot be moved into `spawn`.
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let producer = std::thread::spawn(move || {
            let mut i = 0.0f32;
            let mut dropped = 0usize;
            while (i as usize) < N {
                let chunk: Vec<f32> = (0..64).map(|k| i + k as f32).collect();
                let w = p.push(&chunk);
                dropped += chunk.len() - w;
                i += w as f32;
                if w == 0 {
                    std::thread::yield_now();
                }
            }
            dropped
        });

        let consumer = std::thread::spawn(move || {
            let mut dst = vec![0.0f32; 137];
            let mut seen = 0usize;
            let mut last = -1.0f32;
            loop {
                let n = c.pop(&mut dst);
                if n == 0 {
                    if seen >= N {
                        break;
                    }
                    std::thread::yield_now();
                    continue;
                }
                for &s in &dst[..n] {
                    assert_eq!(s, last + 1.0, "out-of-order sample at {seen}");
                    last = s;
                    seen += 1;
                    if seen >= N {
                        break;
                    }
                }
                counter.store(seen, Ordering::Relaxed);
            }
            seen
        });

        let _dropped = producer.join().unwrap();
        let seen = consumer.join().unwrap();
        assert_eq!(seen, N, "consumer saw {} of {N}", seen);
    }
}
