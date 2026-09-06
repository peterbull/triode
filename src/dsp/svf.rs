use super::{flush_denormal, sanitize};

const MAX_STATE: f32 = 64.0;

#[derive(Clone, Copy, Default)]
pub(crate) struct Svf {
    ic1eq: f32,
    ic2eq: f32,
}

impl Svf {
    pub(crate) fn process(&mut self, input: f32, g: f32, k: f32) -> f32 {
        let g = sanitize(g, 0.0, 0.0, 8.0);
        let k = sanitize(k, 1.0 / 1.5, 0.25, 2.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        let v3 = input - self.ic2eq;
        let band = a1 * self.ic1eq + a2 * v3;
        let low = self.ic2eq + a2 * self.ic1eq + a3 * v3;
        self.ic1eq = flush_denormal(sanitize(
            2.0 * band - self.ic1eq,
            0.0,
            -MAX_STATE,
            MAX_STATE,
        ));
        self.ic2eq = flush_denormal(sanitize(2.0 * low - self.ic2eq, 0.0, -MAX_STATE, MAX_STATE));
        sanitize(band * k, 0.0, -MAX_STATE, MAX_STATE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recursive_state_flushes_to_zero_after_silence() {
        let mut filter = Svf::default();
        filter.process(1.0, 0.1, 1.0);
        for _ in 0..200_000 {
            filter.process(0.0, 0.1, 1.0);
        }
        assert_eq!(filter.ic1eq, 0.0);
        assert_eq!(filter.ic2eq, 0.0);
    }
}
