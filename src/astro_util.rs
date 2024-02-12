/// Returns the separation, in radians, between the given celestial coordinates
/// (in radians).
pub fn angular_separation(p0_ra: f64, p0_dec: f64,
                          p1_ra: f64, p1_dec: f64) -> f64 {
    (p0_dec.sin() * p1_dec.sin() +
     p0_dec.cos() * p1_dec.cos() * (p0_ra - p1_ra).cos()).acos()
}

/// Returns the position angle of p1 relative to p0. Range is -pi..pi,
/// increasing counter-clockwise from zero at north.
/// Args and return value in radians.
pub fn position_angle(p0_ra: f64, p0_dec: f64,
                      p1_ra: f64, p1_dec: f64) -> f64 {
    // Adapted from
    // https://astronomy.stackexchange.com/questions/25306
    // (measuring-misalignment-between-two-positions-on-sky)

    let sin_term = (0.5 * (p1_ra - p0_ra)).sin();
    let y = (p1_dec - p0_dec).sin() +
        2.0 * p0_dec.sin() * p1_dec.cos() * sin_term * sin_term;
    let x = p0_dec.cos() * (p1_ra - p0_ra).sin();

    x.atan2(y)
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use approx::assert_abs_diff_eq;
    use std::f64::consts::{FRAC_PI_2, PI};
    use super::*;

    #[test]
    fn p1_north_of_p0() {
        // Two points with same RA, differing only in DEC.
        let p0_ra = PI;
        let p0_dec = 0.0;

        let p1_ra = PI;
        let p1_dec = 1.0;

        assert_abs_diff_eq!(position_angle(p0_ra, p0_dec, p1_ra, p1_dec),
                            0.0,
                            epsilon = 0.01);
    }

}  // mod tests.
