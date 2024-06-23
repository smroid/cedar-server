// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use astro::angle::{anglr_sepr, limit_to_two_PI};
use astro::coords::{alt_frm_eq, az_frm_eq, hr_angl_frm_hz};
use astro::time::{CalType, Date, julian_day, mn_sidr};

use chrono::{Datelike, DateTime, Timelike, Utc};
use std::f64::consts::PI;
use std::time::SystemTime;

/// Returns the separation, in radians, between the given celestial coordinates
/// (in radians).
pub fn angular_separation(p0_ra: f64, p0_dec: f64,
                          p1_ra: f64, p1_dec: f64) -> f64 {
    anglr_sepr(p0_ra, p0_dec, p1_ra, p1_dec)
}

/// Returns the position angle of p1 relative to p0. Range is -pi..pi,
/// increasing counter-clockwise from zero at north.
/// Args and return value in radians.
/// Returns 0 if p0 and p1 are degenerate (same).
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

/// Returns (alt, az, ha) in radians. Returned azimuth is clockwise from north.
/// Returned hour angle is -PI..PI.
/// ra: right ascension in radians.
/// dec: declination in radians.
/// lat: observer latitude in radians.
/// long: observer longitude in radians.
pub fn alt_az_from_equatorial(ra: f64, dec: f64, lat: f64, long: f64,
                              time: SystemTime) -> (/*alt*/f64, /*az*/f64, /*ha*/f64) {
    let gmst = greenwich_mean_sidereal_time_from_system_time(time);

    // Note that astro::coords::hr_angl_frm_observer_long() has a bug. Fortunately
    // the correct relation is trivial.
    let hour_angle = gmst + long - ra;

    let meeus_az = az_frm_eq(hour_angle, dec, lat);
    let az = limit_to_two_PI(meeus_az + PI);
    let mut ha = limit_to_two_PI(hour_angle);
    if ha > PI {
        ha -= 2.0 * PI;
    }

    (alt_frm_eq(hour_angle, dec, lat), az, ha)
}

/// Returns (ra, dec) in radians.
/// alt: elevation in radians
/// az: radians, clockwise from north
/// lat: observer latitude in radians.
/// long: observer longitude in radians.
pub fn equatorial_from_alt_az(alt: f64, az: f64, lat: f64, long: f64,
                              time: SystemTime) -> (f64, f64) {
    let meeus_az = limit_to_two_PI(az - PI);
    let gmst = greenwich_mean_sidereal_time_from_system_time(time);

    // astro::coords::dec_frm_hz() is incorrect.
    let dec = (lat.sin() * alt.sin() - lat.cos() * alt.cos() * meeus_az.cos()).asin();
    let hour_angle = hr_angl_frm_hz(meeus_az, alt, lat);
    let ra = gmst + long - hour_angle;

    (ra, dec)
}

fn greenwich_mean_sidereal_time_from_system_time(time: SystemTime) -> f64 {
    let dt_utc = DateTime::<Utc>::from(time);
    let date = Date{year: dt_utc.date_naive().year() as i16,
                    month: dt_utc.date_naive().month() as u8,
                    decimal_day: dt_utc.date_naive().day() as f64,
                    cal_type: CalType::Gregorian};
    let jd = julian_day(&date);

    let utc_hours = dt_utc.time().num_seconds_from_midnight() as f64 / 3600.0;
    let gmst_hours = mn_sidr(jd).to_degrees() / 15.0 + utc_hours * 1.00273790935;

    limit_to_two_PI((gmst_hours * 15.0).to_radians())
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use astro::angle::{deg_frm_dms, deg_frm_hms};
    use approx::assert_abs_diff_eq;
    use chrono::{FixedOffset, TimeZone};
    use std::time::{Duration};
    use super::*;

    #[test]
    fn test_angular_separation() {
        let p0_ra = PI;
        let p0_dec = 0.0;

        let p1_ra = PI + 1.0;
        let p1_dec = 1.0;

        assert_abs_diff_eq!(angular_separation(p0_ra, p0_dec, p1_ra, p1_dec),
                            1.27,
                            epsilon = 0.01);
    }

    #[test]
    fn test_p1_north_of_p0() {
        // Two points with same RA, differing only in DEC.
        let p0_ra = PI;
        let p0_dec = 0.0;

        let p1_ra = PI;
        let p1_dec = 1.0;

        assert_abs_diff_eq!(position_angle(p0_ra, p0_dec, p1_ra, p1_dec),
                            0.0,
                            epsilon = 0.01);
    }

    #[test]
    fn test_alt_az_equatorial_conversion() {
        let mizar_ra = deg_frm_hms(13, 23, 55.5).to_radians();
        let mizar_dec = deg_frm_dms(54, 55, 31.3).to_radians();

        let dt = FixedOffset::west_opt(8 * 3600).unwrap().with_ymd_and_hms(
            2024, 3, 7, 23, 56, 0).unwrap();
        let time = SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs_f64(
            dt.timestamp_millis() as f64 / 1000.0)).unwrap();

        let lat = 37_f64.to_radians();
        let long = -122_f64.to_radians();

        let (alt, az, ha) =
            alt_az_from_equatorial(mizar_ra, mizar_dec, lat, long, time);

        // Expected values obtained from SkySafari.
        assert_abs_diff_eq!(alt,
                            deg_frm_dms(58, 52, 14.3).to_radians(),
                            epsilon = 0.01);
        assert_abs_diff_eq!(az,
                            deg_frm_dms(42, 59, 36.7).to_radians(),
                            epsilon = 0.01);
        assert_abs_diff_eq!(ha,
                            -deg_frm_hms(2, 29, 50.9).to_radians(),
                            epsilon = 0.01);

        // Now go the other way.
        let (ra, dec) = equatorial_from_alt_az(alt, az, lat, long, time);
        assert_abs_diff_eq!(ra, mizar_ra,
                            epsilon = 0.01);
        assert_abs_diff_eq!(dec, mizar_dec,
                            epsilon = 0.01);
    }

}  // mod tests.
