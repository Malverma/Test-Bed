//! Linear resampling of interleaved stereo, for Windows audio: WASAPI runs
//! at the device's rate, Opus at 48 kHz.

pub struct Resampler {
    step: f64,
    pos: f64,
    last: [f32; 2],
}

impl Resampler {
    pub fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, pos: 0.0, last: [0.0; 2] }
    }

    pub fn push(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if (self.step - 1.0).abs() < f64::EPSILON {
            out.extend_from_slice(input);
            return;
        }
        let frames = input.len() / 2;
        // Position is relative to `last` (index -1) .. input frames.
        while self.pos < frames as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            let at = |k: isize, c: usize| if k < 0 { self.last[c] } else { input[k as usize * 2 + c] };
            for c in 0..2 {
                let (a, b) = (at(i - 1, c), at(i, c));
                out.push(a + (b - a) * t);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        if frames > 0 {
            self.last = [input[(frames - 1) * 2], input[(frames - 1) * 2 + 1]];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_44k1_to_48k() {
        let mut rs = Resampler::new(44_100, 48_000);
        let input: Vec<f32> = (0..4410).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut out = Vec::new();
        rs.push(&input[..4410], &mut out);
        rs.push(&input[4410..], &mut out);
        // 100 ms in, about 100 ms out.
        assert!((out.len() as i32 / 2 - 4800).abs() <= 2, "{}", out.len() / 2);
    }

    #[test]
    fn same_rate_passes_through() {
        let mut out = Vec::new();
        Resampler::new(48_000, 48_000).push(&[0.5, -0.5], &mut out);
        assert_eq!(out, [0.5, -0.5]);
    }
}
