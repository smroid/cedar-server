// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use crate::cedar_sky::{CatalogConstraint, CatalogDescription,
                       Constellation, LocationInfo, ObjectType,
                       ObjectTypeConstraint, Ordering,
                       SkyLocationConstraint, SelectedCatalogEntry};
use canonical_error::CanonicalError;

pub trait CedarSkyTrait {
    fn get_catalog_descriptions(&self) -> Vec<CatalogDescription>;
    fn get_object_types(&self) -> Vec<ObjectType>;
    fn get_constellations(&self) -> Vec<Constellation>;
    fn query_catalog_entries(&self,
                             sky_location_constraint: Option<&SkyLocationConstraint>,
                             min_elevation: Option<f32>,
                             faintest_magnitude: Option<f32>,
                             catalog_constraint: Option<&CatalogConstraint>,
                             object_type_constraint: Option<&ObjectTypeConstraint>,
                             ordering: Option<Ordering>,
                             dedup_distance: Option<f32>,
                             decrowd_distance: Option<f32>,
                             location_info: Option<&LocationInfo>)
                             -> Result<Vec<SelectedCatalogEntry>, CanonicalError>;
}
