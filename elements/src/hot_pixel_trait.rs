// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use crate::cedar::ImageCoord;
use crate::cedar_common::CelestialCoord;

use cedar_detect::algorithm::StarDescription;

use canonical_error::CanonicalError;

// Abstract interface for creating and using a hot pixel map.
pub trait HotPixelTrait {
    // Classifies a list of detected stars (or hot pixels masquerading as stars)
    // against this hot pixel map. Returns the candidates that are classified as
    // stars and the candidates that are classified as hot pixels. Order is
    // preserved.
    // If is_ready() is false all candidates are returned as stars.
    fn classify_candidates(&self, candidates: &Vec<StarDescription>) ->
      (/*stars*/Vec<StarDescription>, /*hot_pixels*/Vec<StarDescription>);

    // Updates the hot pixel map with a list of detected star centroids (or hot
    // pixels masquerading as stars).
    // If sky_pos is not given, the candidates are presumed to be from a dark
    // frame and are used to definitively replace the hot pixel map; is_ready()
    // becomes true immediately.
    // If sky_pos is given, the candidates are heuristically combined with
    // candidates from previous calls to update_hot_pixel_map() to discriminate
    // which candidates are (likely to be) real stars and which candidates are
    // (likely to be) hot pixels; the hot pixel map is updated accordingly.
    // is_ready() becomes true after a sufficient number of
    // update_hot_pixel_map() calls are made with sufficiently differing sky_pos
    // values.
    fn update_hot_pixel_map(&mut self,
                            candidates: &Vec<StarDescription>,
                            sky_pos: Option<CelestialCoord>);

    // Indicates whether this hot pixel map is sufficiently initialized for
    // classify_candidates() to be effective.
    fn is_ready(&self) -> bool;

    // Returns this map's hot pixels. If is_ready() is false returns empty list.
    fn get_hot_pixels(&self) -> Vec<ImageCoord>;

    // Saves the hot pixel map. This call blocks on IO.
    fn save_state(&self) -> Result<(), CanonicalError>;
}
