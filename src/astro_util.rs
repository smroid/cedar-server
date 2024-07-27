// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use astro::angle::{anglr_sepr, limit_to_two_PI};
use astro::coords::{alt_frm_eq, az_frm_eq, hr_angl_frm_hz};
use astro::time::{CalType, Date, julian_day, mn_sidr};

use chrono::{Datelike, DateTime, Timelike, Utc};
use std::f64::consts::PI;
use std::time::SystemTime;

extern crate nalgebra as na;

/// Convert ra/dec (radians) to x/y/z on unit sphere.
pub fn to_unit_vector(ra: f64, dec: f64) -> [f64; 3] {
    [(ra.cos() * dec.cos()),  // x
     (ra.sin() * dec.cos()),  // y
     dec.sin()]               // z
}

/// Convert x/y/z on unitsphere to ra/dec (radians).
pub fn from_unit_vector(v: &[f64; 3]) -> (f64, f64) {
    let x = v[0];
    let y = v[1];
    let z = v[2];
    let dec = z.asin();
    let mut ra = y.atan2(x);
    if ra < 0.0 {
        ra += 2.0 * PI;
    }
    (ra, dec)
}

/// Return the Euclidean distance between the given vectors.
pub fn distance(v1: &[f64; 3], v2: &[f64; 3]) -> f64 {
    distance_sq(v1, v2).sqrt()
}

/// Return the square of the Euclidean distance between the given vectors.
pub fn distance_sq(v1: &[f64; 3], v2: &[f64; 3]) -> f64 {
    (v1[0] - v2[0]) * (v1[0] - v2[0]) +
    (v1[1] - v2[1]) * (v1[1] - v2[1]) +
    (v1[2] - v2[2]) * (v1[2] - v2[2])
}

/// Converts angle (radians) to distance between two unit vectors with that
/// angle between them.
pub fn distance_from_angle(angle: f64) -> f64 {
    2.0 * (angle / 2.0).sin()
}

/// Converts distance between two unit vectors the the angle between them.
pub fn angle_from_distance(distance: f64) -> f64 {
    2.0 * (0.5 * distance).asin()
}

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
    let ra_diff = p1_ra - p0_ra;
    let sin_term = (0.5 * ra_diff).sin();
    let y = (p1_dec - p0_dec).sin() +
        2.0 * p0_dec.sin() * p1_dec.cos() * sin_term * sin_term;
    let x = p0_dec.cos() * ra_diff.sin();

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

/// Port of Tetra3's _distort_centroids() function. Note that argument is
/// (x, y), in contrast to Tetra3 which reverses this.
pub fn distort_centroid(centroid: &[f64; 2], width: usize, height: usize,
                        distortion: f64) -> [f64; 2] {
    let tol = 1e-6;
    let maxiter = 30;
    let k = distortion;

    let (mut x, mut y) = (centroid[0], centroid[1]);
    let width = width as f64;
    let height = height as f64;

    // Center.
    x -= width / 2.0;
    y -= height / 2.0;
    let r_undist = 2.0 * (x * x + y * y).sqrt() / width;

    // Initial guess, distorted at same position.
    let mut r_dist = r_undist;
    for _i in 0..maxiter {
        let r_undist_est = r_dist * (1.0 - k * (r_dist * r_dist)) / (1.0 - k);
        let dru_drd = (1.0 - 3.0 * k * (r_dist * r_dist))/(1.0 - k);
        let error = r_undist - r_undist_est;
        r_dist += error / dru_drd;
        if error.abs() < tol {
            break
        }
    }
    x *= r_dist / r_undist;
    y *= r_dist / r_undist;
    // Decenter.
    [x + width / 2.0, y + height / 2.0]
}

/// Port of Tetra3's _undistort_centroids() function. Note that the arguments is
/// (x, y), in contrast to Tetra3 which reverses this.
pub fn undistort_centroid(centroid: &[f64; 2], width: usize, height: usize,
                          distortion: f64) -> [f64; 2] {
    let k = distortion;

    let (mut x, mut y) = (centroid[0], centroid[1]);
    let width = width as f64;
    let height = height as f64;
    // Center.
    x -= width / 2.0;
    y -= height / 2.0;
    let r_dist = 2.0 * (x * x + y * y).sqrt() / width;
    // Scale.
    let scale = (1.0 - k * (r_dist * r_dist)) / (1.0 - k);
    x *= scale;
    y *= scale;
    // Decenter.
    [x + width / 2.0, y + height / 2.0]
}

/// Port of Tetra3's transform_to_image_coords() function. Note that the
/// return is [x, y], in contrast to Tetra3 which reverses it.
pub fn transform_to_image_coord(celestial_coord: &[f64; 2],
                                width: usize, height: usize, fov: f64,
                                rotation_matrix: &[f64; 9], distortion: f64)
                                -> [f64; 2] {
    let ra = celestial_coord[0].to_radians();
    let dec = celestial_coord[1].to_radians();

    let celestial_vector = na::RowVector3::<f64>::new(
        ra.cos() * dec.cos(),  // x
        ra.sin() * dec.cos(),  // y
        dec.sin()              // z
    );
    let rot_matrix = na::Matrix3::from_row_slice(rotation_matrix);
    let celestial_vector_derot =
        rot_matrix * &celestial_vector.transpose();
    let binding = celestial_vector_derot.column(0);
    let slice = binding.as_slice();
    let vec = [slice[0], slice[1], slice[2]];

    distort_centroid(&compute_centroid(&vec, width, height, fov),
                     width, height, distortion)
}

/// Port of Tetra3's transform_to_celestial_coords() function. Note that the
/// coord arg is [x, y], in contrast to Tetra3 which reverses it.
pub fn transform_to_celestial_coords(image_coord: &[f64; 2],
                                     width: usize, height: usize, fov: f64,
                                     rotation_matrix: &[f64; 9], distortion: f64)
                                     -> [f64; 2] {
    let rot_matrix = na::Matrix3::from_row_slice(rotation_matrix);
    let image_coord = undistort_centroid(
        &image_coord, width, height, distortion);
    let vec = compute_vector(&image_coord, width, height, fov);
    let image_vector = na::RowVector3::<f64>::new(vec[0], vec[1], vec[2]);
    let rotated_image_vector =
        rot_matrix.transpose() * &image_vector.transpose();

    let ra = rotated_image_vector[1].atan2(rotated_image_vector[0]).
        to_degrees() % 360.0;
    let dec = 90.0 - rotated_image_vector[2].acos().to_degrees();

    [ra, dec]
}

/// Port (with minor changes) of Tetra3's _compute_vectors() function.
fn compute_vector(centroid: &[f64; 2], width: usize, height: usize,
                  fov: f64) -> [f64; 3] {
    let width = width as f64;
    let height = height as f64;
    let scale_factor = 2.0 * (fov / 2.0).tan() / width;
    let y = (width / 2.0 - centroid[0]) * scale_factor;
    let z = (height / 2.0 - centroid[1]) * scale_factor;
    let norm = (z * z + y * y + 1.0).sqrt();
    [1.0 / norm, y / norm, z / norm]
}

/// Port (with minor changes) of Tetra3's _compute_centroids() function.
fn compute_centroid(vector: &[f64; 3], width: usize, height: usize, fov: f64)
                    -> [f64; 2] {
    let width = width as f64;
    let height = height as f64;
    let (i, j, k) = (vector[0], vector[1], vector[2]);
    let scale_factor = -width / 2.0 / (fov / 2.0).tan();
    let x = scale_factor * j / i;
    let y = scale_factor * k / i;

    [x + width / 2.0, y + height / 2.0]
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
    fn test_ra_dec_xyz() {
        let mut v = to_unit_vector(0.0, PI / 4.0);
        let (mut ra, mut dec) = from_unit_vector(&v);
        assert_abs_diff_eq!(ra, 0.0, epsilon = 0.001);
        assert_abs_diff_eq!(dec, PI / 4.0, epsilon = 0.001);

        v = to_unit_vector(PI / 2.0, -PI / 4.0);
        (ra, dec) = from_unit_vector(&v);
        assert_abs_diff_eq!(ra, PI / 2.0, epsilon = 0.001);
        assert_abs_diff_eq!(dec, -PI / 4.0, epsilon = 0.001);

        v = to_unit_vector(PI, PI / 3.0);
        (ra, dec) = from_unit_vector(&v);
        assert_abs_diff_eq!(ra, PI, epsilon = 0.001);
        assert_abs_diff_eq!(dec, PI / 3.0, epsilon = 0.001);

        v = to_unit_vector(3.0 * PI / 2.0, 0.0);
        (ra, dec) = from_unit_vector(&v);
        assert_abs_diff_eq!(ra, 3.0 * PI / 2.0, epsilon = 0.001);
        assert_abs_diff_eq!(dec, 0.0, epsilon = 0.001);
    }

    #[test]
    fn test_distance() {
        assert_eq!(distance(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(distance(&[1.0, 2.0, 3.0], &[1.0, 1.0, 3.0]), 1.0);
        assert_abs_diff_eq!(distance(&[1.0, 2.0, 3.0], &[0.0, 0.0, 0.0]), 3.741,
                            epsilon = 0.001);
    }

    #[test]
    fn test_distance_angle() {
        assert_abs_diff_eq!(distance_from_angle(PI / 2.0), 1.414, epsilon = 0.001);
        assert_abs_diff_eq!(distance_from_angle(PI), 2.0, epsilon = 0.001);

        assert_abs_diff_eq!(angle_from_distance(0.0), 0.0, epsilon = 0.001);
        assert_abs_diff_eq!(angle_from_distance(2_f64.sqrt()), PI / 2.0, epsilon = 0.001);
        assert_abs_diff_eq!(angle_from_distance(2.0), PI, epsilon = 0.001);
    }

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

    #[test]
    fn test_distort_undistort() {
        let centroid = [20.0, 100.0];
        let distorted = distort_centroid(&centroid, 1024, 800, 0.01);
        assert_abs_diff_eq!(distorted[0], 18.636, epsilon = 0.001);
        assert_abs_diff_eq!(distorted[1], 99.168, epsilon = 0.001);
        let undistorted = undistort_centroid(&distorted, 1024, 800, 0.01);
        assert_abs_diff_eq!(undistorted[0], 20.0, epsilon = 0.001);
        assert_abs_diff_eq!(undistorted[1], 100.0, epsilon = 0.001);
    }

    #[test]
    fn test_transform_to_image_coord() {
        let rotation_matrix = [0.5143930851217422, 0.4705764222800965, 0.7169083517249608,
                               0.32501576652434216, 0.6666418828994508, -0.670785622591055,
                               -0.7935770318560958, 0.5780540033235123, 0.18997121822036758];
        let celestial_coords = [35.0, 50.0];
        let img_coords = transform_to_image_coord(
            &celestial_coords, 1024, 800, 10.0, &rotation_matrix, 0.01);
        assert_abs_diff_eq!(img_coords[0], 497.371, epsilon = 0.001);
        assert_abs_diff_eq!(img_coords[1], 391.065, epsilon = 0.001);
        let out_celestial_coords = transform_to_celestial_coords(
            &img_coords, 1024, 800, 10.0, &rotation_matrix, 0.01);
        assert_abs_diff_eq!(out_celestial_coords[0], 35.0, epsilon = 0.001);
        assert_abs_diff_eq!(out_celestial_coords[1], 50.0, epsilon = 0.001);
    }

}  // mod tests.
