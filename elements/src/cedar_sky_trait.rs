// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::SystemTime;

use crate::cedar_common::CelestialCoord;
use crate::cedar_sky::{CatalogDescription, CatalogEntryKey,
                       CatalogEntry, Constellation,
                       ObjectType, Ordering, SelectedCatalogEntry};
use crate::cedar::LatLong;
use canonical_error::CanonicalError;

pub struct LocationInfo {
    pub observer_location: LatLong,
    pub observing_time: SystemTime,
}

pub trait CedarSkyTrait {
    fn get_catalog_descriptions(&self) -> Vec<CatalogDescription>;
    fn get_object_types(&self) -> Vec<ObjectType>;
    fn get_constellations(&self) -> Vec<Constellation>;

    /// Returns the selected catalog entries, plus the number of entries left off
    /// because of `limit_result`.
    fn query_catalog_entries(&self,
                             max_distance: Option<f64>,
                             min_elevation: Option<f64>,
                             faintest_magnitude: Option<i32>,
                             match_catalog_label: bool,
                             catalog_label: &Vec<String>,
                             match_object_type_label: bool,
                             object_type_label: &Vec<String>,
                             text_search: Option<String>,
                             ordering: Option<Ordering>,
                             decrowd_distance: Option<f64>,
                             limit_result: Option<usize>,
                             sky_location: Option<CelestialCoord>,
                             location_info: Option<LocationInfo>)
                             -> Result<(Vec<SelectedCatalogEntry>, usize), CanonicalError>;

    /// Return the selected catalog entry. If it is a solar system object the
    /// current position is calculated using `location_info`.
    fn get_catalog_entry(&mut self,
                         entry_key: CatalogEntryKey,
                         timestamp: SystemTime)
                         -> Result<CatalogEntry, CanonicalError>;
}
