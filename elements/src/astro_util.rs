// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{f64::consts::PI, time::SystemTime};

use astro::{
    angle::{anglr_sepr, limit_to_two_PI},
    coords::{alt_frm_eq, az_frm_eq, hr_angl_frm_hz},
    time::{julian_day, mn_sidr, CalType, Date},
};
use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::cedar::{FovCatalogEntry, ImageCoord, StarCentroid};
use crate::imu_trait::{EquatorialCoordinates, HorizonCoordinates};

extern crate nalgebra as na;

/// Convert ra/dec (radians) to x/y/z on unit sphere.
pub fn to_unit_vector(ra: f64, dec: f64) -> [f64; 3] {
    [
        (ra.cos() * dec.cos()), // x
        (ra.sin() * dec.cos()), // y
        dec.sin(),
    ] // z
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
    (v1[0] - v2[0]) * (v1[0] - v2[0])
        + (v1[1] - v2[1]) * (v1[1] - v2[1])
        + (v1[2] - v2[2]) * (v1[2] - v2[2])
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
pub fn angular_separation(
    p0_ra: f64,
    p0_dec: f64,
    p1_ra: f64,
    p1_dec: f64,
) -> f64 {
    anglr_sepr(p0_ra, p0_dec, p1_ra, p1_dec)
}

/// Returns the position angle of p1 relative to p0. Range is -pi..pi,
/// increasing counter-clockwise from zero at north.
/// Args and return value in radians.
/// Returns 0 if p0 and p1 are degenerate (same).
pub fn position_angle(p0_ra: f64, p0_dec: f64, p1_ra: f64, p1_dec: f64) -> f64 {
    // Adapted from
    // https://astronomy.stackexchange.com/questions/25306
    // (measuring-misalignment-between-two-positions-on-sky)
    let ra_diff = p1_ra - p0_ra;
    let sin_term = (0.5 * ra_diff).sin();
    let y = (p1_dec - p0_dec).sin()
        + 2.0 * p0_dec.sin() * p1_dec.cos() * sin_term * sin_term;
    let x = p0_dec.cos() * ra_diff.sin();

    x.atan2(y)
}

/// Returns (alt, az, ha) in radians. Returned azimuth is clockwise from north.
/// Returned hour angle is -PI..PI.
/// ra: right ascension in radians.
/// dec: declination in radians.
/// lat: observer latitude in radians.
/// long: observer longitude in radians.
pub fn alt_az_from_equatorial(
    ra: f64,
    dec: f64,
    lat: f64,
    long: f64,
    time: &SystemTime,
) -> (/* alt */ f64, /* az */ f64, /* ha */ f64) {
    let gmst = greenwich_mean_sidereal_time_from_system_time(time);

    // Note that astro::coords::hr_angl_frm_observer_long() has a bug.
    // Fortunately the correct relation is trivial.
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
pub fn equatorial_from_alt_az(
    alt: f64,
    az: f64,
    lat: f64,
    long: f64,
    time: &SystemTime,
) -> (f64, f64) {
    let meeus_az = limit_to_two_PI(az - PI);
    let gmst = greenwich_mean_sidereal_time_from_system_time(time);

    // astro::coords::dec_frm_hz() is incorrect.
    let dec =
        (lat.sin() * alt.sin() - lat.cos() * alt.cos() * meeus_az.cos()).asin();
    let hour_angle = hr_angl_frm_hz(meeus_az, alt, lat);
    let ra = gmst + long - hour_angle;

    (ra, dec)
}

/// Converts from equatorial camera coordinates to horizon coordinates.
/// equatorial: Camera pointing in equatorial coordinates (degrees).
/// lat: Observer latitude in radians.
/// long: Observer longitude in radians.
/// time: Current time.
/// Returns: Camera pointing in horizon coordinates.
pub fn horizon_from_equatorial_camera(
    equatorial: &EquatorialCoordinates,
    lat: f64,
    long: f64,
    time: &SystemTime,
) -> HorizonCoordinates {
    let ra = equatorial.ra.to_radians();
    let dec = equatorial.dec.to_radians();
    let north_roll_angle = equatorial.north_roll_angle.to_radians();

    // First convert from equatorial to horizon coordinates.
    let (alt, az, _ha) = alt_az_from_equatorial(ra, dec, lat, long, time);

    // Calculate the position angle of the zenith relative to north at the
    // target location. The zenith is at alt=90 degrees at any azimuth.
    let zenith_ra_dec = equatorial_from_alt_az(PI / 2.0, az, lat, long, time);
    let zenith_position_angle =
        position_angle(ra, dec, zenith_ra_dec.0, zenith_ra_dec.1);

    // The zenith roll angle is the sum of the north roll angle and the zenith
    // position angle. This represents how much the zenith direction is rotated
    // from image "up".
    let zenith_roll_angle = north_roll_angle + zenith_position_angle;

    HorizonCoordinates {
        zenith_roll_angle: zenith_roll_angle.to_degrees(),
        altitude: alt.to_degrees(),
        azimuth: az.to_degrees(),
    }
}

/// Converts from horizon camera coordinates to equatorial coordinates.
/// horizon: Camera pointing in horizon coordinates (degrees).
/// lat: Observer latitude in radians.
/// long: Observer longitude in radians.
/// time: Current time.
/// Returns: Camera pointing in equatorial coordinates.
pub fn equatorial_from_horizon_camera(
    horizon: &HorizonCoordinates,
    lat: f64,
    long: f64,
    time: &SystemTime,
) -> EquatorialCoordinates {
    let alt = horizon.altitude.to_radians();
    let az = horizon.azimuth.to_radians();
    let zenith_roll_angle = horizon.zenith_roll_angle.to_radians();

    // First convert from horizon to equatorial coordinates.
    let (ra, dec) = equatorial_from_alt_az(alt, az, lat, long, time);

    // Calculate the position angle of the zenith relative to the target
    // location. The zenith is at alt=90 degrees at the same azimuth.
    let zenith_ra_dec = equatorial_from_alt_az(PI / 2.0, az, lat, long, time);
    let zenith_position_angle =
        position_angle(ra, dec, zenith_ra_dec.0, zenith_ra_dec.1);

    // The north roll angle is the zenith roll angle minus the position angle
    // of zenith. This reverses the calculation from
    // horizon_from_equatorial_camera.
    let north_roll_angle = zenith_roll_angle - zenith_position_angle;

    EquatorialCoordinates {
        north_roll_angle: north_roll_angle.to_degrees(),
        ra: ra.to_degrees(),
        dec: dec.to_degrees(),
    }
}

fn greenwich_mean_sidereal_time_from_system_time(time: &SystemTime) -> f64 {
    let dt_utc = DateTime::<Utc>::from(*time);
    let date = Date {
        year: dt_utc.date_naive().year() as i16,
        month: dt_utc.date_naive().month() as u8,
        decimal_day: dt_utc.date_naive().day() as f64,
        cal_type: CalType::Gregorian,
    };
    let jd = julian_day(&date);

    let utc_hours = dt_utc.time().num_seconds_from_midnight() as f64 / 3600.0;
    let gmst_hours =
        mn_sidr(jd).to_degrees() / 15.0 + utc_hours * 1.00273790935;

    limit_to_two_PI((gmst_hours * 15.0).to_radians())
}

/// Port of Tetra3's _distort_centroids() function. Note that argument is
/// (x, y), in contrast to Tetra3 which reverses this.
fn distort_centroid(
    centroid: &[f64; 2],
    width: usize,
    height: usize,
    distortion: f64,
) -> [f64; 2] {
    let tol = 1e-6;
    let maxiter = 30;
    let k = distortion;

    let (mut x, mut y) = (centroid[0], centroid[1]);
    let width = width as f64;
    let height = height as f64;
    let kp = k * (2.0 / width) * (2.0 / width); // k prime.

    // Center.
    x -= width / 2.0;
    y -= height / 2.0;
    let r_undist = (x * x + y * y).sqrt();

    // Initial distorted guess, undistorted are the same position.
    let mut r_dist = r_undist;
    for _i in 0..maxiter {
        let r_undist_est = r_dist * (1.0 - kp * r_dist * r_dist) / (1.0 - k);
        let dru_drd = (1.0 - 2.0 * kp * r_dist) / (1.0 - k);
        let error = r_undist - r_undist_est;
        r_dist += error / dru_drd;
        if error.abs() < tol {
            break;
        }
    }
    x *= r_dist / r_undist;
    y *= r_dist / r_undist;
    // Decenter.
    [x + width / 2.0, y + height / 2.0]
}

/// Port of Tetra3's _undistort_centroids() function. Note that the arguments is
/// (x, y), in contrast to Tetra3 which reverses this.
fn undistort_centroid(
    centroid: &[f64; 2],
    width: usize,
    height: usize,
    distortion: f64,
) -> [f64; 2] {
    let k = distortion;

    let (mut x, mut y) = (centroid[0], centroid[1]);
    let width = width as f64;
    let height = height as f64;
    let kp = k * (2.0 / width) * (2.0 / width); // k prime.

    // Center.
    x -= width / 2.0;
    y -= height / 2.0;
    let r_dist = (x * x + y * y).sqrt();
    // Scale.
    let scale = (1.0 - kp * r_dist * r_dist) / (1.0 - k);
    x *= scale;
    y *= scale;
    // Decenter.
    [x + width / 2.0, y + height / 2.0]
}

/// Port of Tetra3's transform_to_image_coords() function. Note that the
/// return is [x, y], in contrast to Tetra3 which reverses it.
pub fn transform_to_image_coord(
    celestial_coord: &[f64; 2],
    width: usize,
    height: usize,
    fov: f64,
    rotation_matrix: &[f64; 9],
    distortion: f64,
) -> [f64; 2] {
    let ra = celestial_coord[0].to_radians();
    let dec = celestial_coord[1].to_radians();

    let celestial_vector = na::RowVector3::<f64>::new(
        ra.cos() * dec.cos(), // x
        ra.sin() * dec.cos(), // y
        dec.sin(),            // z
    );
    let rot_matrix = na::Matrix3::from_row_slice(rotation_matrix);
    let celestial_vector_derot = rot_matrix * celestial_vector.transpose();
    let binding = celestial_vector_derot.column(0);
    let slice = binding.as_slice();
    let vec = [slice[0], slice[1], slice[2]];

    distort_centroid(
        &compute_centroid(&vec, width, height, fov.to_radians()),
        width,
        height,
        distortion,
    )
}

/// Port of Tetra3's transform_to_celestial_coords() function. Note that the
/// coord arg is [x, y], in contrast to Tetra3 which reverses it.
pub fn transform_to_celestial_coords(
    image_coord: &[f64; 2],
    width: usize,
    height: usize,
    fov: f64,
    rotation_matrix: &[f64; 9],
    distortion: f64,
) -> [f64; 2] {
    let rot_matrix = na::Matrix3::from_row_slice(rotation_matrix);
    let image_coord =
        undistort_centroid(image_coord, width, height, distortion);
    let vec = compute_vector(&image_coord, width, height, fov.to_radians());
    let image_vector = na::RowVector3::<f64>::new(vec[0], vec[1], vec[2]);
    let rotated_image_vector =
        rot_matrix.transpose() * image_vector.transpose();

    let ra = rotated_image_vector[1]
        .atan2(rotated_image_vector[0])
        .to_degrees()
        % 360.0;
    let dec = 90.0 - rotated_image_vector[2].acos().to_degrees();

    [ra, dec]
}

/// Port (with minor changes) of Tetra3's _compute_vectors() function.
fn compute_vector(
    centroid: &[f64; 2],
    width: usize,
    height: usize,
    fov_rad: f64,
) -> [f64; 3] {
    let width = width as f64;
    let height = height as f64;
    let scale_factor = 2.0 * (fov_rad / 2.0).tan() / width;
    let y = (width / 2.0 - centroid[0]) * scale_factor;
    let z = (height / 2.0 - centroid[1]) * scale_factor;
    let norm = (z * z + y * y + 1.0).sqrt();
    [1.0 / norm, y / norm, z / norm]
}

/// Port (with minor changes) of Tetra3's _compute_centroids() function.
fn compute_centroid(
    vector: &[f64; 3],
    width: usize,
    height: usize,
    fov_rad: f64,
) -> [f64; 2] {
    let width = width as f64;
    let height = height as f64;
    let (i, j, k) = (vector[0], vector[1], vector[2]);
    let scale_factor = -width / 2.0 / (fov_rad / 2.0).tan();
    let x = scale_factor * j / i;
    let y = scale_factor * k / i;

    [x + width / 2.0, y + height / 2.0]
}

/// Give the intensity ratio for second / first corresponding to the
/// passed stellar magnitudes.
pub fn magnitude_intensity_ratio(m1: f64, m2: f64) -> f64 {
    2.512f64.powf(m1 - m2)
}

/// When exposing for plate solving, we increase exposure until a desired
/// number of stars (typically 20) are detected. In so doing, a very bright
/// star (or planet) in the FOV might be overexposed and due to blooming
/// might not be detected as a star.
///
/// For plate solving, a missing star is tolerable and a solution will be found
/// from the other stars. However, in Cedar's SETUP alignment mode, Cedar Aim
/// draws a selection target on the brightest stars in the list of detected
/// stars. Thus, the absence of the brightest star (or planet) in the list of
/// detected stars is quite unfortunate in SETUP alignment mode, where the user
/// has been prompted to point the telescope at the brightest star in its part
/// of the sky. That star will not have a selection target, oops.
///
/// We solve this by taking advantage of the fact that the plate solution will
/// have the bright star (or planet) in its list of catalog entries in the FOV.
/// This function returns the original `detections` list augmented by item(s)
/// from `catalog_entries`.
///
/// Args must be in order of descending brightness. Caution: complexity is the
/// product of the vector sizes.
pub fn fill_in_detections(
    detections: &Vec<StarCentroid>,
    catalog_entries: &Vec<FovCatalogEntry>,
) -> Vec<StarCentroid> {
    const IMAGE_DISTANCE_THRESHOLD_SQ: f64 = 4.0;

    // Find the brightest `catalog_entries` item that also exists in
    // `detections`. We do this so we can relate catalog magnitudes to
    // StarCentroid.brightness values.
    let mut found_match = false;
    let mut match_magnitude = 0.0;
    let mut match_brightness = 0.0;
    for catalog_entry in catalog_entries {
        let cat_coord = catalog_entry.image_pos.as_ref().unwrap();
        for detection in detections {
            let det_coord = detection.centroid_position.as_ref().unwrap();
            if image_distance_sq(det_coord, cat_coord)
                < IMAGE_DISTANCE_THRESHOLD_SQ
            {
                // Found a same-location item between catalog_entries and
                // detections.
                match_magnitude =
                    catalog_entry.entry.as_ref().unwrap().magnitude;
                match_brightness = detection.brightness;
                found_match = true;
                break;
            }
        }
        if found_match {
            break;
        }
    }
    if !found_match {
        return detections.clone(); // Bail out.
    }

    // Gather `catalog_entries` that do not have corresponding `detections`
    // entries
    let mut detections_for_catalog_entries = Vec::<StarCentroid>::new();
    for catalog_entry in catalog_entries {
        let cat_coord = catalog_entry.image_pos.as_ref().unwrap();
        let mut found_detection = false;
        for detection in detections {
            let det_coord = detection.centroid_position.as_ref().unwrap();
            if image_distance_sq(det_coord, cat_coord)
                < IMAGE_DISTANCE_THRESHOLD_SQ
            {
                found_detection = true;
                break;
            }
        }
        if !found_detection {
            // Synthesize a StarCentroid corresponding to `catalog_entry`.
            let brightness = match_brightness
                * magnitude_intensity_ratio(
                    match_magnitude,
                    catalog_entry.entry.as_ref().unwrap().magnitude,
                );
            detections_for_catalog_entries.push(StarCentroid {
                centroid_position: Some(cat_coord.clone()),
                brightness,
                num_saturated: 0,
            });
        }
    }

    let mut merged = Vec::<StarCentroid>::with_capacity(
        detections.len() + detections_for_catalog_entries.len(),
    );
    let mut i = 0;
    let mut j = 0;
    while i < detections.len() && j < detections_for_catalog_entries.len() {
        if detections[i].brightness
            > detections_for_catalog_entries[j].brightness
        {
            merged.push(detections[i].clone());
            i += 1;
        } else {
            merged.push(detections_for_catalog_entries[j].clone());
            j += 1;
        }
    }
    merged.extend_from_slice(&detections[i..]);
    merged.extend_from_slice(&detections_for_catalog_entries[j..]);

    merged
}

// Return the square of the Euclidean distance between the given image
// coordinates.
fn image_distance_sq(c1: &ImageCoord, c2: &ImageCoord) -> f64 {
    (c1.x - c2.x) * (c1.x - c2.x) + (c1.y - c2.y) * (c1.y - c2.y)
}

#[cfg(test)]
mod tests {
    extern crate approx;
    use std::time::Duration;

    use approx::assert_abs_diff_eq;
    use astro::angle::{deg_frm_dms, deg_frm_hms};
    use chrono::{FixedOffset, TimeZone};

    use super::*;
    use crate::{
        cedar_common::CelestialCoord,
        cedar_sky::{CatalogEntry, ObjectType},
    };

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
        assert_abs_diff_eq!(
            distance(&[1.0, 2.0, 3.0], &[0.0, 0.0, 0.0]),
            3.741,
            epsilon = 0.001
        );
    }

    #[test]
    fn test_distance_angle() {
        assert_abs_diff_eq!(
            distance_from_angle(PI / 2.0),
            1.414,
            epsilon = 0.001
        );
        assert_abs_diff_eq!(distance_from_angle(PI), 2.0, epsilon = 0.001);

        assert_abs_diff_eq!(angle_from_distance(0.0), 0.0, epsilon = 0.001);
        assert_abs_diff_eq!(
            angle_from_distance(2_f64.sqrt()),
            PI / 2.0,
            epsilon = 0.001
        );
        assert_abs_diff_eq!(angle_from_distance(2.0), PI, epsilon = 0.001);
    }

    #[test]
    fn test_angular_separation() {
        let p0_ra = PI;
        let p0_dec = 0.0;

        let p1_ra = PI + 1.0;
        let p1_dec = 1.0;

        let sep = angular_separation(p0_ra, p0_dec, p1_ra, p1_dec);
        assert_abs_diff_eq!(sep, 1.27, epsilon = 0.01);

        // Compute it a different way.
        let vec0 = to_unit_vector(p0_ra, p0_dec);
        let vec1 = to_unit_vector(p1_ra, p1_dec);
        let vec_dist = distance(&vec0, &vec1);
        let sep2 = angle_from_distance(vec_dist);
        assert_abs_diff_eq!(sep, sep2, epsilon = 0.01);
    }

    #[test]
    fn test_p1_north_of_p0() {
        // Two points with same RA, differing only in DEC.
        let p0_ra = PI;
        let p0_dec = 0.0;

        let p1_ra = PI;
        let p1_dec = 1.0;

        assert_abs_diff_eq!(
            position_angle(p0_ra, p0_dec, p1_ra, p1_dec),
            0.0,
            epsilon = 0.01
        );
    }

    #[test]
    fn test_alt_az_equatorial_conversion() {
        let mizar_ra = deg_frm_hms(13, 23, 55.5).to_radians();
        let mizar_dec = deg_frm_dms(54, 55, 31.3).to_radians();

        let dt = FixedOffset::west_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2024, 3, 7, 23, 56, 0)
            .unwrap();
        let time = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs_f64(
                dt.timestamp_millis() as f64 / 1000.0,
            ))
            .unwrap();

        let lat = 37_f64.to_radians();
        let long = -122_f64.to_radians();

        let (alt, az, ha) =
            alt_az_from_equatorial(mizar_ra, mizar_dec, lat, long, &time);

        // Expected values obtained from SkySafari.
        assert_abs_diff_eq!(
            alt,
            deg_frm_dms(58, 52, 14.3).to_radians(),
            epsilon = 0.01
        );
        assert_abs_diff_eq!(
            az,
            deg_frm_dms(42, 59, 36.7).to_radians(),
            epsilon = 0.01
        );
        assert_abs_diff_eq!(
            ha,
            -deg_frm_hms(2, 29, 50.9).to_radians(),
            epsilon = 0.01
        );

        // Now go the other way.
        let (ra, dec) = equatorial_from_alt_az(alt, az, lat, long, &time);
        assert_abs_diff_eq!(ra, mizar_ra, epsilon = 0.01);
        assert_abs_diff_eq!(dec, mizar_dec, epsilon = 0.01);
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
        let rotation_matrix = [
            0.5143930851217422,
            0.4705764222800965,
            0.7169083517249608,
            0.32501576652434216,
            0.6666418828994508,
            -0.670785622591055,
            -0.7935770318560958,
            0.5780540033235123,
            0.18997121822036758,
        ];
        let celestial_coords = [38.0, 45.0];
        let img_coords = transform_to_image_coord(
            &celestial_coords,
            1024,
            800,
            10.0,
            &rotation_matrix,
            0.01,
        );
        assert_abs_diff_eq!(img_coords[0], 529.486, epsilon = 0.001);
        assert_abs_diff_eq!(img_coords[1], 727.513, epsilon = 0.001);
        let out_celestial_coords = transform_to_celestial_coords(
            &img_coords,
            1024,
            800,
            10.0,
            &rotation_matrix,
            0.01,
        );
        assert_abs_diff_eq!(out_celestial_coords[0], 38.0, epsilon = 0.001);
        assert_abs_diff_eq!(out_celestial_coords[1], 45.0, epsilon = 0.001);
    }

    #[test]
    fn test_magnitude_intensity_ratio() {
        let ratio = magnitude_intensity_ratio(2.0, 1.0);
        assert_abs_diff_eq!(ratio, 2.51, epsilon = 0.01);
    }

    #[test]
    fn test_horizon_equatorial_camera_conversion() {
        // Test round-trip conversion between equatorial and horizon camera coordinates
        let mizar_ra_deg = deg_frm_hms(13, 23, 55.5);
        let mizar_dec_deg = deg_frm_dms(54, 55, 31.3);
        let north_roll_angle_deg = 30.0; // degrees

        let dt = FixedOffset::west_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2024, 3, 7, 23, 56, 0)
            .unwrap();
        let time = SystemTime::UNIX_EPOCH
            .checked_add(Duration::from_secs_f64(
                dt.timestamp_millis() as f64 / 1000.0,
            ))
            .unwrap();

        let lat = 37_f64.to_radians();
        let long = -122_f64.to_radians();

        let equatorial = EquatorialCoordinates {
            north_roll_angle: north_roll_angle_deg,
            ra: mizar_ra_deg,
            dec: mizar_dec_deg,
        };

        // Forward conversion: equatorial -> horizon
        let horizon = horizon_from_equatorial_camera(&equatorial, lat, long, &time);

        // Reverse conversion: horizon -> equatorial
        let equatorial_out = equatorial_from_horizon_camera(&horizon, lat, long, &time);

        // Verify round-trip
        assert_abs_diff_eq!(equatorial_out.ra, mizar_ra_deg, epsilon = 0.001);
        assert_abs_diff_eq!(equatorial_out.dec, mizar_dec_deg, epsilon = 0.001);
        assert_abs_diff_eq!(
            equatorial_out.north_roll_angle,
            north_roll_angle_deg,
            epsilon = 0.001
        );
    }

    #[test]
    fn test_fill_in_detections() {
        let detections = vec![
            // d1.
            StarCentroid {
                centroid_position: Some(ImageCoord { x: 12.0, y: 15.0 }),
                brightness: 1200.0,
                num_saturated: 0,
            },
            // d2.
            StarCentroid {
                centroid_position: Some(ImageCoord { x: 22.0, y: 35.0 }),
                brightness: 900.0,
                num_saturated: 0,
            },
            // d3.
            StarCentroid {
                centroid_position: Some(ImageCoord { x: 42.0, y: 350.0 }),
                brightness: 700.0,
                num_saturated: 0,
            },
        ];
        let catalog_entries = vec![
            FovCatalogEntry {
                entry: Some(CatalogEntry {
                    catalog_label: "PL".to_string(),
                    catalog_entry: "jupiter".to_string(),
                    coord: Some(CelestialCoord { ra: 0.0, dec: 0.0 }),
                    constellation: None,
                    object_type: Some(ObjectType {
                        label: "xx".to_string(),
                        broad_category: "yy".to_string(),
                    }),
                    magnitude: -1.5,
                    angular_size: None,
                    common_name: None,
                    notes: None,
                }),
                image_pos: Some(ImageCoord { x: 50.0, y: 60.0 }),
            },
            FovCatalogEntry {
                entry: Some(CatalogEntry {
                    catalog_label: "IAU".to_string(),
                    catalog_entry: "some_star".to_string(),
                    coord: Some(CelestialCoord { ra: 0.0, dec: 0.0 }),
                    constellation: None,
                    object_type: Some(ObjectType {
                        label: "xx".to_string(),
                        broad_category: "yy".to_string(),
                    }),
                    magnitude: 2.5,
                    angular_size: None,
                    common_name: None,
                    notes: None,
                }),
                image_pos: Some(ImageCoord { x: 21.5, y: 35.2 }), // Match d2.
            },
        ];

        let filled_in = fill_in_detections(&detections, &catalog_entries);
        assert_eq!(filled_in.len(), 4);

        let jupiter = &filled_in[0];
        assert_eq!(jupiter.centroid_position.as_ref().unwrap().x, 50.0);
        assert_eq!(jupiter.centroid_position.as_ref().unwrap().y, 60.0);
        assert_abs_diff_eq!(jupiter.brightness, 35836.0, epsilon = 1.0);
        let d1 = &filled_in[1];
        assert_eq!(d1.centroid_position.as_ref().unwrap().x, 12.0);
        assert_eq!(d1.centroid_position.as_ref().unwrap().y, 15.0);
        assert_eq!(d1.brightness, 1200.0);
        let d2 = &filled_in[2];
        assert_eq!(d2.centroid_position.as_ref().unwrap().x, 22.0);
        assert_eq!(d2.centroid_position.as_ref().unwrap().y, 35.0);
        assert_eq!(d2.brightness, 900.0);
        let d3 = &filled_in[3];
        assert_eq!(d3.centroid_position.as_ref().unwrap().x, 42.0);
        assert_eq!(d3.centroid_position.as_ref().unwrap().y, 350.0);
        assert_eq!(d3.brightness, 700.0);
    }
} // mod tests.
