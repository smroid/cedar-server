// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use canonical_error::{CanonicalError, unimplemented_error};

use cedar_server::cedar_server::server_main;
use cedar_server::cedar_sky::{CatalogConstraint, CatalogDescription,
                              Constellation, LocationInfo, ObjectType,
                              ObjectTypeConstraint, Ordering,
                              SkyLocationConstraint, SelectedCatalogEntry};
use cedar_server::cedar_sky_trait::CedarSkyTrait;

struct Nothing {}

impl CedarSkyTrait for Nothing {
    fn get_catalog_descriptions(&self) -> Vec<CatalogDescription> {
        Vec::<CatalogDescription>::new()
    }
    fn get_object_types(&self) -> Vec<ObjectType> {
        Vec::<ObjectType>::new()
    }
    fn get_constellations(&self) -> Vec<Constellation> {
        Vec::<Constellation>::new()
    }
    fn query_catalog_entries(&self,
                             _sky_location_constraint: Option<&SkyLocationConstraint>,
                             _min_elevation: Option<f32>,
                             _faintest_magnitude: Option<f32>,
                             _catalog_constraint: Option<&CatalogConstraint>,
                             _object_type_constraint: Option<&ObjectTypeConstraint>,
                             _ordering: Option<Ordering>,
                             _dedup_distance: Option<f32>,
                             _decrowd_distance: Option<f32>,
                             _location_info: Option<&LocationInfo>)
                             -> Result<Vec<SelectedCatalogEntry>, CanonicalError> {
        Err(unimplemented_error(""))
    }
}

fn main() {
    server_main("cedar-box",
                "Copyright (c) 2024 Steven Rosenthal smr@dt3.org. \
                 Licensed for non-commercial use. See LICENSE.md at \
                 https://github.com/smroid/cedar-server",
                &Nothing{});
}
