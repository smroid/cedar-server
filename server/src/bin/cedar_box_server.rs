// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::path::Path;
use std::sync::Arc;

use cypress_solver::Tetra3Solver;
use pico_args::Arguments;
use tetra3::Solver;
use tokio::sync::Mutex;

use cedar_elements::solver_trait::SolverTrait;
use cedar_server::cedar_server::server_main;

fn main() {
    server_main(
        "Copyright (c) 2026 Steven Rosenthal smr@dt3.org.\n\
         Licensed for non-commercial use.\n\
         See LICENSE.md at https://github.com/smroid/cedar-server",
        /*flutter_app_path=*/ "../cedar/cedar-aim/cedar_flutter/build/web",
        /*get_dependencies=*/
        |_pargs: Arguments| {
            let mut pargs = Arguments::from_env();
            let db_name: String = pargs
                .value_from_str("--tetra3_database")
                .unwrap_or("default_database".to_string());
            let db_path_str = format!("../cedar/data/{}.npz", db_name);
            let db_path = Path::new(&db_path_str);
            let solver = Tetra3Solver::new(
                Solver::load_database(db_path).expect("Failed to load Tetra3 database"),
            );
            let solver_arc: Arc<Mutex<dyn SolverTrait + Send + Sync>> =
                Arc::new(Mutex::new(solver));
            (None, None, None, Some(solver_arc))
        },
    );
}
