// Copyright (c) 2024 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use cedar_server::cedar_server::server_main;

fn main() {
    server_main(/*args=*/None,
                "Cedar-Box",
                "Copyright (c) 2024 Steven Rosenthal smr@dt3.org.\n\
                 Licensed for non-commercial use.\n\
                 See LICENSE.md at https://github.com/smroid/cedar-server",
                None);
}
