//! Signal and callback contracts; no devices or playback.
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use triode::dsp::resampler::SliceSource;
use triode::engine::{Engine, Shared};
use triode::preset::Preset;

struct CountingAllocator;
thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}

// Count only this test thread, so unrelated parallel tests cannot affect the contract.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if WATCH.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if WATCH.try_with(Cell::get).unwrap_or(false) {
            let _ = FREES.try_with(|c| c.set(c.get() + 1));
        }
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn no_heap(f: impl FnOnce()) {
    struct Stop;
    impl Drop for Stop {
        fn drop(&mut self) {
            WATCH.with(|c| c.set(false));
        }
    }
    ALLOCS.with(|c| c.set(0));
    FREES.with(|c| c.set(0));
    WATCH.with(|c| c.set(true));
    let stop = Stop;
    f();
    drop(stop);
    assert_eq!(ALLOCS.with(Cell::get), 0, "allocation in callback");
    assert_eq!(FREES.with(Cell::get), 0, "deallocation in callback");
}

#[test]
fn steady_processing_and_scope_never_touch_the_heap() {
    let shared = Shared::new();
    let mut engine = Engine::new(48000.0, 64);
    engine.load_rack(Preset::blues().to_slots(48000.0));
    let input = [0.1; 8192];
    let mut source = SliceSource::new(&input);
    let mut output = [[0.0; 2]; 256];
    no_heap(|| {
        for _ in 0..16 {
            engine.process(Some(&shared), &mut source, &mut output);
        }
    });
}

#[test]
fn finite_source_has_no_live_clock_correction() {
    let mut engine = Engine::new(48000.0, 64);
    let input: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.04).sin() * 0.1).collect();
    engine.enable_taps(1024);
    engine.process(None, &mut SliceSource::new(&input), &mut [[0.0; 2]; 1024]);
    let actual = &engine.taps().unwrap().stages[0].data;
    for (i, (&got, &want)) in actual.iter().zip(&input).enumerate() {
        assert!(
            (got - want).abs() < 1e-6,
            "input moved at {i}: {got} != {want}"
        );
    }
}

#[test]
fn source_replacement_does_not_replay_the_old_input() {
    let mut engine = Engine::new(48000.0, 64);
    engine.process(
        None,
        &mut SliceSource::new(&[0.5; 4096]),
        &mut [[0.0; 2]; 16],
    );
    engine.set_rates(48000.0, 48000.0);
    engine.enable_taps(64);
    engine.process(
        None,
        &mut SliceSource::new(&[0.0; 4096]),
        &mut [[0.0; 2]; 64],
    );
    assert!(engine.taps().unwrap().stages[0]
        .data
        .iter()
        .all(|x| *x == 0.0));
}

#[test]
fn structural_edits_prepare_and_retire_memory_off_callback() {
    use triode::dsp::cab::Ir;
    use triode::engine::{Cmd, Slot};
    use triode::params::EffectKind;
    let shared = Shared::new();
    let mut engine = Engine::new(48000.0, 64);
    let mut tick = || {
        no_heap(|| {
            engine.process(
                Some(&shared),
                &mut SliceSource::new(&[0.1; 512]),
                &mut [[0.0; 2]; 64],
            )
        })
    };
    for kind in EffectKind::ALL {
        shared.send(Cmd::InsertSlot {
            at: 0,
            slot: Slot::build(kind, true, 48000.0),
        });
        tick();
        shared.send(Cmd::ReplaceSlot {
            slot: 0,
            with: Slot::build(kind, true, 48000.0),
        });
        tick();
        shared.send(Cmd::RemoveSlot { slot: 0 });
        tick();
    }
    for _ in 0..3 {
        shared.send(Cmd::LoadRack(Preset::blues().to_slots(48000.0)));
        tick();
        shared.send(Cmd::LoadRack(Vec::new()));
        tick();
        shared.send(Cmd::InsertSlot {
            at: 0,
            slot: Slot::build(EffectKind::Reverb, true, 48000.0),
        });
        tick();
        shared.load_ir(Ir::new("test".into(), vec![1.0, 0.5, 0.2]).unwrap());
        tick();
        shared.clear_ir();
        tick();
    }
    shared.collect_retired();
}

#[test]
fn full_retirement_queue_defers_without_losing_commands() {
    use triode::engine::{Cmd, Slot};
    use triode::params::EffectKind;
    let shared = Shared::new();
    let mut engine = Engine::new(48000.0, 64);
    // Invalid replacements return their prepared processor instead of dropping it.
    for _ in 0..65 {
        shared.send(Cmd::ReplaceSlot {
            slot: 99,
            with: Slot::build(EffectKind::Reverb, true, 48000.0),
        });
    }
    for _ in 0..2 {
        no_heap(|| {
            engine.process(
                Some(&shared),
                &mut SliceSource::new(&[]),
                &mut [[0.0; 2]; 64],
            )
        });
        assert_eq!(shared.cmds.lock().unwrap().len(), 1);
    }
    shared.collect_retired();
    no_heap(|| {
        engine.process(
            Some(&shared),
            &mut SliceSource::new(&[]),
            &mut [[0.0; 2]; 64],
        )
    });
    assert!(shared.cmds.lock().unwrap().is_empty());
}

#[test]
fn scope_output_matches_every_internal_chunk() {
    let shared = Shared::new();
    let mut engine = Engine::new(48000.0, 64);
    let input: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.04).sin() * 0.1).collect();
    let mut output = [[0.0; 2]; 256];
    engine.process(Some(&shared), &mut SliceSource::new(&input), &mut output);
    let (_, actual, _, _) = shared.scope_snap().unwrap();
    for (i, (got, want)) in actual.iter().zip(output).enumerate() {
        assert_eq!(*got, (want[0] + want[1]) * 0.5, "scope diverged at {i}");
    }
}
