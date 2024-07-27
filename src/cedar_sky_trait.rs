// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use crate::cedar_sky::{CatalogDescription, Constellation, LocationInfo,
                       ObjectType, Ordering, SelectedCatalogEntry};
use crate::tetra3_server::CelestialCoord;
use canonical_error::CanonicalError;

pub trait CedarSkyTrait {
    fn get_catalog_descriptions(&self) -> Vec<CatalogDescription>;
    fn get_object_types(&self) -> Vec<ObjectType>;
    fn get_constellations(&self) -> Vec<Constellation>;
    fn query_catalog_entries(&self,
                             max_distance: Option<f32>,
                             min_elevation: Option<f32>,
                             faintest_magnitude: Option<f32>,
                             catalog_match: &Vec<String>,
                             object_type_match: &Vec<String>,
                             ordering: Option<Ordering>,
                             dedup_distance: Option<f32>,
                             decrowd_distance: Option<f32>,
                             sky_location: Option<CelestialCoord>,
                             location_info: Option<&LocationInfo>)
                             -> Result<Vec<SelectedCatalogEntry>, CanonicalError>;
}
