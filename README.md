# Triode

A real-time valve guitar amp in Rust. Audio interface in → up to 12 stompbox effects →
preamp + passive tone stack → phase inverter → long-tail-pair power amp → cabinet
simulation → brickwall limiter → out. Desktop UI with knobs, bypass switches and a live
rack you can edit while playing.

`docs/SPEC.md` holds the DSP maths, the real-time rules, and the decisions worth arguing
about — including two deviations flagged for ratification.

## Run it

```sh
cargo run                    # the UI: microphone/interface → speakers
cargo run -- devices         # what this machine has, marking the defaults
cargo run -- selftest        # 10 audio sanity checks, no hardware needed
cargo run -- render in.wav out.wav --preset blues
cargo run -- trace           # per-stage level / DC / THD through the whole chain
cargo run -- capture take1.wav --seconds 6   # record the input to a WAV
```

`trace` with no file generates a −20 dBFS 110 Hz sine, so it always reports something, and
because the signal is known it prints **THD per stage** — which stage adds the distortion,
not just the fact that something did. Every stage is also written as its own WAV, so the
fuzz stage can be soloed instead of inferred. `capture` makes a real performance test input.

Needs a Rust toolchain (<https://rustup.rs>). No build step, no assets, no server.

**It starts with the input disarmed**, on purpose: laptop speakers a few centimetres from the
laptop's own microphone plus an amp is a howl before you have touched anything. Open the window
and you get a live amp with nothing captured; tick **`input on`** in the header to hear the guitar
(or start with `--input-on`, or `--input "Device Name"`, which arms it too). If you are on an
interface and headphones, there is nothing to howl — arm it and leave it armed.

**Turn your monitor down first.** A live mic through speakers will howl; master starts at
0.3 and there is a `mute` box in the amp card. On macOS, a binary launched from a terminal
inherits that terminal's Microphone permission — if it was never granted you get silence,
not an error (the header bar shows a climbing input-gap figure rather than a crash).

## The UI

* **Amp card** — gain, bass, mid, treble, presence, master, input trim; `cab`, `hp filter`
  and `mute` switches; load a real impulse response by path or fall back to the built-in
  speaker model.
* **Rack** — one card per pedal with its own knobs and a bypass LED. Add from any of nine
  kinds, swap the kind in place, remove, and reorder with ◀ ▶ **or by dragging a card onto
  another**. Reordering moves the running effect rather than rebuilding it, so a delay tail
  keeps decaying through the move.
* **Knobs** — drag vertically (or horizontally), hold `shift` for fine, scroll to nudge,
  double-click for the preset default, or `tab` to focus and use the arrow keys. Mapped
  through each parameter's curve, so a log pot feels like one.
* **Wave view** (bottom) — a scope of the last N ms, and an **input → output transfer
  plot** of the same samples against a 1:1 line. The transfer plot is the honest one: a
  clipped sine still looks like a sine on a scope, but here any stage that limits or squares
  off peels off the diagonal and goes flat. `freeze` holds the window; the built-in test
  tone drives both views with no guitar attached.
* **Header** — live CPU, sample rate, clock drift in ppm, input/output clip counts
  (clickable to reset) and xruns. Device pickers with an explicit `apply`, because changing
  an audio device is not a silent action.

Presets are JSON in `./presets` (override with `$TRIODE_PRESETS`); save and load from the
bottom-right of the window. `blues` ships as the default patch.

## Testing

```sh
cargo test                     # 136 tests, no hardware, no network
cargo test -- --ignored       # the one test that opens real audio devices
cargo run -- selftest         # the same checks as a pass/fail table
```

Offline rendering runs the *identical* engine with no `cpal` in sight, so tone can be
verified against generated signals: DC really disappears, the limiter really holds its
ceiling under abuse, a delay echo really lands at the millisecond on the knob, each rate
from 44.1 to 96 kHz really renders, and no effect can produce a NaN at either knob rail.

The C prototype (`src/*.c`, `vendor/`, `Makefile`) is left in place and still builds. It was
read for device and buffer behaviour clues; nothing in the Rust build depends on it.
