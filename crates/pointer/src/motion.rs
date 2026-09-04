//! How the pointer gets from A to B.
//!
//! Shapes borrowed from OxyMouse (github.com/oxylabs/OxyMouse, Python), whose
//! three algorithms are bezier, gaussian and perlin. Re-implemented rather
//! than bound: OxyMouse returns a point list from A to B, which is exactly
//! what `wire::Op::Perform` already carries, so the whole of this module
//! lives on this side of the connection. **The agent is unaffected by
//! anything in this file** — it replays points either way.
//!
//! Straight-line teleports are a poor way to drive a real desktop: UI that
//! responds to hover, drag thresholds, and anything with enter/leave
//! animations all behave differently when the pointer arrives instantly at a
//! coordinate rather than travelling to it.
//!
//! Randomness is seeded and explicit, never drawn from the OS. A path that
//! cannot be reproduced cannot be tested, and in a codebase built on a
//! replayable event log, a movement that differs on replay is a movement that
//! cannot be audited.

/// xorshift64*. Small, fast, seeded — sufficient for path jitter and
/// deliberately not a CSPRNG. Nothing here is a secret.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn seed(s: u64) -> Self {
        // 0 is a fixed point of xorshift; anything else is fine.
        Rng(if s == 0 { 0x9E3779B97F4A7C15 } else { s })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[-1, 1)`.
    pub fn signed(&mut self) -> f64 {
        self.unit() * 2.0 - 1.0
    }

    /// Standard normal, Box–Muller.
    pub fn gauss(&mut self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

/// Cubic ease-in-out, the pacing curve every algorithm here shares. Humans
/// accelerate away and decelerate in; only the *shape* of the detour differs
/// between algorithms, not the speed profile along it.
fn ease(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Zero at both ends, one in the middle. Every deviation is multiplied by
/// this, which is what guarantees a path starts where the pointer is and
/// lands exactly on the target however violent the noise in between.
fn envelope(t: f64) -> f64 {
    (std::f64::consts::PI * t).sin()
}

fn smoothstep(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

/// 1-D gradient (Perlin) noise over a unit lattice, hashed from the seed so
/// it is reproducible without storing the lattice.
fn perlin1(x: f64, seed: u64) -> f64 {
    let grad = |i: i64| -> f64 {
        let mut h = (i as u64)
            .wrapping_mul(0x9E3779B97F4A7C15)
            .wrapping_add(seed);
        h ^= h >> 29;
        h = h.wrapping_mul(0xBF58476D1CE4E5B9);
        h ^= h >> 32;
        ((h >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    let i = x.floor();
    let f = x - i;
    let (g0, g1) = (grad(i as i64), grad(i as i64 + 1));
    let (a, b) = (g0 * f, g1 * (f - 1.0));
    a + smoothstep(f) * (b - a)
}

/// The path shape. Every variant produces the same *count* of points and the
/// same speed profile; they differ in how far, and how, the route bows away
/// from the straight line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Motion {
    /// Straight, eased. Deterministic and shortest. The right choice when a
    /// test is asserting on coordinates.
    Ease,
    /// Cubic Bézier through two randomized control points. OxyMouse's
    /// "perfect for moving to click a button" — one clean arc, no wobble.
    /// `bow` is the control-point offset as a fraction of the distance.
    Bezier { bow: f64 },
    /// A Bézier spine with a Gaussian random walk laid over it. Wobblier and
    /// less repeatable in shape; the closest of the three to an unhurried
    /// hand. `sigma` is in pixels at the midpoint.
    Gaussian { sigma: f64 },
    /// A straight line displaced by summed octaves of gradient noise.
    /// Smoother and more correlated than Gaussian — drift rather than
    /// tremor.
    ///
    /// `amplitude` is a scale knob, **not** a promise of pixels: noise can
    /// legitimately be near-flat over a short traverse, so the peak deviation
    /// varies by seed. Measured over 12 seeds on a 1837px diagonal with 3
    /// octaves, `amplitude: 40.0` gave a mean peak of 16px and a minimum of
    /// 6px. Multiply by roughly 0.4 to predict a typical peak, and do not
    /// rely on a floor.
    Perlin { amplitude: f64, octaves: u32 },
}

impl Default for Motion {
    fn default() -> Self {
        // Bezier: one arc, no tremor, and cheap. Ease is for tests.
        Motion::Bezier { bow: 0.12 }
    }
}

impl Motion {
    /// `n` points from (but excluding) `from`, ending **exactly** on `to`.
    /// Rounding and noise must never leave the pointer a pixel short of
    /// where the caller aimed.
    pub fn points(
        &self,
        from: (f64, f64),
        to: (f64, f64),
        n: u32,
        rng: &mut Rng,
    ) -> Vec<(f64, f64)> {
        let n = n.max(1);
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let dist = (dx * dx + dy * dy).sqrt();
        // Unit normal to the straight line, the axis every detour uses.
        let (nx, ny) = if dist > f64::EPSILON {
            (-dy / dist, dx / dist)
        } else {
            (0.0, 0.0)
        };

        // Two control offsets for the Bézier variants, drawn once per path.
        let (c1, c2) = match self {
            Motion::Bezier { bow } => (rng.signed() * bow * dist, rng.signed() * bow * dist),
            Motion::Gaussian { .. } => (rng.signed() * 0.06 * dist, rng.signed() * 0.06 * dist),
            _ => (0.0, 0.0),
        };
        let noise_seed = rng.next_u64();
        // Where in the noise field this path samples from. Without it every
        // path starts at lattice origin 0, where 1-D gradient noise is pinned
        // to exactly zero, so a short traverse could come out nearly straight
        // however large the amplitude — measured 5.9px against a requested 40
        // on one seed and 28.6px on the next.
        let phase = rng.unit() * 512.0;

        let mut out = Vec::with_capacity(n as usize);
        for i in 1..=n {
            let t = i as f64 / n as f64;
            let e = ease(t);
            // Position along the straight line, at the eased pace.
            let mut x = from.0 + dx * e;
            let mut y = from.1 + dy * e;

            let off = match self {
                Motion::Ease => 0.0,
                // Cubic Bézier's normal displacement with endpoints on the
                // line: 3(1-t)²t·c1 + 3(1-t)t²·c2, in `e` so the bow tracks
                // the pacing rather than the raw parameter.
                Motion::Bezier { .. } => {
                    let u = 1.0 - e;
                    3.0 * u * u * e * c1 + 3.0 * u * e * e * c2
                }
                Motion::Gaussian { sigma } => {
                    let u = 1.0 - e;
                    let spine = 3.0 * u * u * e * c1 + 3.0 * u * e * e * c2;
                    spine + rng.gauss() * sigma * envelope(t)
                }
                Motion::Perlin { amplitude, octaves } => {
                    // 1-D gradient noise peaks well below ±1, and lower the
                    // more octaves are summed, because the higher ones are
                    // uncorrelated and cancel: measured peak per unit
                    // amplitude over 400 seeds was 0.372 at 1 octave, 0.280
                    // at 3, 0.254 at 4. The gain lifts that to a useful
                    // range without pretending to be exact — see the note on
                    // the variant for what `amplitude` actually buys.
                    const GAIN: f64 = 3.0;
                    let mut sum = 0.0;
                    let mut amp = 1.0;
                    let mut freq = 3.0;
                    let mut norm = 0.0;
                    for o in 0..(*octaves).max(1) {
                        sum += perlin1(phase + t * freq, noise_seed.wrapping_add(o as u64)) * amp;
                        norm += amp;
                        amp *= 0.5;
                        freq *= 2.0;
                    }
                    (sum / norm.max(f64::EPSILON)) * amplitude * GAIN * envelope(t)
                }
            };
            x += nx * off;
            y += ny * off;
            out.push((x, y));
        }
        // The envelope is zero at t=1 for the noise terms and the Bézier
        // offset vanishes at e=1, so this is belt-and-braces against float
        // drift rather than a correction.
        *out.last_mut().unwrap() = to;
        out
    }
}
