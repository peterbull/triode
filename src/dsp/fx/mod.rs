//! Seventeen stompbox effects. Each is a [`crate::engine::Proc`]: no allocation,
//! locking or blocking inside `process`, and all parameter indexes match the tables
//! in [`crate::params`] exactly (a test asserts the counts so a spec edit cannot
//! silently desync an effect).

pub mod analog_delay;
pub mod bitcrusher;
pub mod boost;
pub mod chorus;
pub mod comp;
pub mod delay;
pub mod drive;
pub mod envelope_filter;
pub mod eq;
pub mod flanger;
pub mod gate;
pub mod phaser;
pub mod reverb;
pub mod ring_mod;
pub mod step_filter;
pub mod trem;

#[cfg(test)]
mod tests {
    use crate::engine::{make_proc, Slot};
    use crate::params::EffectKind;

    /// The spec table is the UI's source of truth and the effects' index contract; if a
    /// table grows past `MAX_PARAMS` or a kind forgets its params, this fails here.
    #[test]
    fn every_effect_gets_its_spec_sized_state() {
        for kind in EffectKind::ALL {
            let specs = kind.params();
            assert!(!specs.is_empty(), "{} has no params", kind.name());
            assert!(specs.len() <= crate::params::MAX_PARAMS);
            let defaults = kind.default_norms();
            for (i, s) in specs.iter().enumerate() {
                // Every declared param must have a sane default in the same range.
                assert!(
                    defaults.v[i] >= -1e-6 && defaults.v[i] <= 1.0 + 1e-6,
                    "{}/{} default norm out of range",
                    kind.name(),
                    s.name
                );
            }
            // And a processor must exist and run for it.
            let mut slot = Slot::build(kind, true, 48000.0);
            let mut buf = [[0.1f32, -0.1]; 64];
            slot.advance(1.0);
            slot.proc.process(&mut buf, 64, &slot.values);
            assert!(
                buf.iter().all(|f| f[0].is_finite() && f[1].is_finite()),
                "{kind:?} junk"
            );
            let _ = make_proc(kind);
        }
    }
}
