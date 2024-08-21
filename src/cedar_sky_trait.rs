// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::time::SystemTime;

use crate::cedar_sky::{CatalogDescription, CatalogEntryKey,
                       CatalogEntry, Constellation,
                       ObjectType, Ordering, SelectedCatalogEntry};
use crate::cedar::LatLong;
use crate::tetra3_server::CelestialCoord;
use canonical_error::CanonicalError;

pub struct LocationInfo {
    pub observer_location: LatLong,
    pub observing_time: SystemTime,
}

pub trait CedarSkyTrait {
    /// Initiates processing of solar system ephemeris entries.
    fn initiate_solar_system_processing(&mut self, time: SystemTime);

    /// Checks to see if the solar system ephemeris has completed processing,
    /// and if so, absorbs its contents.
    fn check_solar_system_completion(&mut self);

    fn get_catalog_descriptions(&self) -> Vec<CatalogDescription>;
    fn get_object_types(&self) -> Vec<ObjectType>;
    fn get_constellations(&self) -> Vec<Constellation>;

    /// Returns the selected catalog entries, plus the number of entries left off
    /// because of `limit_result`.
    fn query_catalog_entries(&self,
                             max_distance: Option<f64>,
                             min_elevation: Option<f64>,
                             faintest_magnitude: Option<i32>,
                             catalog_label: &Vec<String>,
                             object_type_label: &Vec<String>,
                             text_search: Option<String>,
                             ordering: Option<Ordering>,
                             decrowd_distance: Option<f64>,
                             limit_result: Option<usize>,
                             sky_location: Option<CelestialCoord>,
                             location_info: Option<LocationInfo>)
                             -> Result<(Vec<SelectedCatalogEntry>, usize), CanonicalError>;
    fn get_catalog_entry(&self,
                         entry_key: CatalogEntryKey,
                         location_info: Option<LocationInfo>)
                         -> Result<CatalogEntry, CanonicalError>;
}
