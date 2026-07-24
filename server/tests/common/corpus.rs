// Corpus loading and precondition probing for the e2e plate-solve harness.
//
// The image corpus is not committed; it is staged by cedar-solve's
// tools/fetch_corpus.py and located via the CEDAR_E2E_DATA_DIR env var.

use std::path::{Path, PathBuf};
use std::process::Command;

use image::GrayImage;

/// Ground truth for one synthetic field, from manifest.csv.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub rotation_deg: f64,
    pub fov_x_deg: f64,
    pub fov_y_deg: f64,
    pub pixscale_arcsec: f64,
    pub nx: u32,
    pub ny: u32,
    pub n_rendered: i32,
}

impl Field {
    /// The field's true horizontal FOV, in degrees.
    ///
    /// NOT `fov_x_deg`. The manifest records `fov_x_deg = nx * pixscale`, a
    /// small-angle approximation, but synthstars.py renders the image on a
    /// TAN/gnomonic WCS (`ctype = RA---TAN, DEC--TAN`), whose true angular
    /// width is `2 * atan(nx/2 * pixscale)`. At this corpus's 12.71 deg the two
    /// differ by -0.4071%, which is exactly the "0.4%-low FOV bias" previously
    /// attributed to tetra3. Both tetra3 and tetra3rs report this value; the
    /// manifest column is what is wrong.
    pub fn true_fov_x_deg(&self) -> f64 {
        let pixscale_rad = (self.pixscale_arcsec / 3600.0).to_radians();
        (2.0 * ((self.nx as f64 / 2.0) * pixscale_rad).atan()).to_degrees()
    }
}

/// The exact header the generator (cedar-solve/tools/gen_corpus.py) writes.
/// Parsing is positional, so a header drift must be a hard error rather than a
/// silent column shift.
const MANIFEST_HEADER: &str = "name,ra,dec,rotation_deg,fov_x_deg,fov_y_deg,\
                               pixscale_arcsec,nx,ny,maglim,n_catalog,n_rendered,seed";

pub fn parse_manifest(text: &str) -> Result<Vec<Field>, String> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());

    let header = lines.next().ok_or("manifest.csv is empty")?.trim();
    if header != MANIFEST_HEADER {
        return Err(format!(
            "manifest.csv header drift.\n  expected: {MANIFEST_HEADER}\n  found:    {header}"
        ));
    }

    let mut fields = Vec::new();
    for (n, line) in lines.enumerate() {
        let row: Vec<&str> = line.trim().split(',').collect();
        if row.len() != 13 {
            return Err(format!(
                "manifest.csv row {} has {} columns, expected 13",
                n + 2,
                row.len()
            ));
        }
        let num = |i: usize| -> Result<f64, String> {
            row[i]
                .parse::<f64>()
                .map_err(|e| format!("manifest.csv row {} col {}: {e}", n + 2, i))
        };
        fields.push(Field {
            name: row[0].to_string(),
            ra_deg: num(1)?,
            dec_deg: num(2)?,
            rotation_deg: num(3)?,
            fov_x_deg: num(4)?,
            fov_y_deg: num(5)?,
            pixscale_arcsec: num(6)?,
            nx: num(7)? as u32,
            ny: num(8)? as u32,
            n_rendered: num(11)? as i32,
        });
    }
    Ok(fields)
}

/// Loads `<dir>/<name>.png` as 8-bit grayscale, the prod-faithful format the
/// camera yields.
pub fn load_image(dir: &Path, field: &Field) -> Result<GrayImage, String> {
    let path = dir.join(format!("{}.png", field.name));
    let img = image::open(&path)
        .map_err(|e| format!("opening {}: {e}", path.display()))?
        .to_luma8();
    let (w, h) = (img.width(), img.height());
    if (w, h) != (field.nx, field.ny) {
        return Err(format!(
            "{}: image is {w}x{h}, manifest says {}x{}",
            path.display(),
            field.nx,
            field.ny
        ));
    }
    Ok(img)
}

/// Everything the real-solver path needs, or a reason it cannot run.
pub enum Preconditions {
    Ready(Env),
    Skip(String),
}

pub struct Env {
    pub data_dir: PathBuf,
    pub tetra3_script: PathBuf,
    /// The tetra3 database *name*, not a path: tetra3_server.py passes it to
    /// `tetra3.Tetra3(load_database=...)`, which resolves it inside the tetra3
    /// package's data dir.
    pub tetra3_database: String,
}

/// Repo root (`/…/cedar`), derived from this package's manifest dir
/// (`/…/cedar/cedar-server/server`) so nothing is hardcoded to one machine.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("CARGO_MANIFEST_DIR has >=2 ancestors")
        .to_path_buf()
}

fn env_path(var: &str, default: PathBuf) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or(default)
}

pub fn preconditions() -> Preconditions {
    let root = repo_root();

    let data_dir = match std::env::var_os("CEDAR_E2E_DATA_DIR") {
        Some(d) => PathBuf::from(d),
        None => {
            return Preconditions::Skip(
                "CEDAR_E2E_DATA_DIR is not set. Stage the corpus with:\n  \
                 cd cedar-solve && source .cedar_venv/bin/activate && \
                 python tools/fetch_corpus.py\n  \
                 export CEDAR_E2E_DATA_DIR=$PWD/tests/data/synthetic_large"
                    .to_string(),
            );
        }
    };
    if !data_dir.join("manifest.csv").is_file() {
        return Preconditions::Skip(format!(
            "no manifest.csv under CEDAR_E2E_DATA_DIR ({}). Run \
             cedar-solve/tools/fetch_corpus.py.",
            data_dir.display()
        ));
    }

    // Tetra3Solver spawns a bare `python`, inheriting PATH. If the cedar-solve
    // venv is not active, the subprocess dies a second after launch and the
    // failure surfaces far from its cause -- so probe for it here.
    match Command::new("python").args(["-c", "import tetra3"]).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            return Preconditions::Skip(format!(
                "`python -c \"import tetra3\"` failed. Activate the venv:\n  \
                 source {}/cedar-solve/.cedar_venv/bin/activate\n  stderr: {}",
                root.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Err(e) => {
            return Preconditions::Skip(format!(
                "cannot run `python` ({e}). Activate the venv:\n  \
                 source {}/cedar-solve/.cedar_venv/bin/activate",
                root.display()
            ));
        }
    }

    let tetra3_script = env_path(
        "CEDAR_E2E_TETRA3_SCRIPT",
        root.join("tetra3_server/python/tetra3_server.py"),
    );
    if !tetra3_script.is_file() {
        return Preconditions::Skip(format!(
            "tetra3 server script not found at {}",
            tetra3_script.display()
        ));
    }

    // What `tetra3.Tetra3(load_database="default_database")` actually loads.
    let npz = env_path(
        "CEDAR_E2E_TETRA3_DB_NPZ",
        root.join("cedar-solve/tetra3/data/default_database.npz"),
    );
    if !npz.is_file() {
        return Preconditions::Skip(format!(
            "tetra3 database not found at {}",
            npz.display()
        ));
    }

    Preconditions::Ready(Env {
        data_dir,
        tetra3_script,
        tetra3_database: "default_database".to_string(),
    })
}

pub fn load_manifest(data_dir: &Path) -> Result<Vec<Field>, String> {
    let path = data_dir.join("manifest.csv");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse_manifest(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "name,ra,dec,rotation_deg,fov_x_deg,fov_y_deg,pixscale_arcsec,nx,ny,maglim,n_catalog,n_rendered,seed\n\
                          grid_000,0.0,-60.0,-180.0,12.71,7.149375,23.83125,1920,1080,7.5,159,70,0\n";

    #[test]
    fn parses_a_known_row() {
        let fields = parse_manifest(SAMPLE).expect("parse");
        assert_eq!(fields.len(), 1);
        let f = &fields[0];
        assert_eq!(f.name, "grid_000");
        assert_eq!(f.ra_deg, 0.0);
        assert_eq!(f.dec_deg, -60.0);
        assert_eq!(f.rotation_deg, -180.0);
        assert_eq!(f.fov_x_deg, 12.71);
        assert_eq!(f.nx, 1920);
        assert_eq!(f.ny, 1080);
        assert_eq!(f.n_rendered, 70);
    }

    /// The manifest's fov_x_deg is `nx * pixscale` (small-angle); the true FOV
    /// of a TAN-projected image is `2*atan(nx/2 * pixscale)`. They differ by
    /// -0.4071% here, which is the entire "tetra3 FOV bias". Pin both, so the
    /// distinction cannot be quietly undone.
    #[test]
    fn true_fov_is_gnomonic_not_small_angle() {
        let f = &parse_manifest(SAMPLE).expect("parse")[0];

        // The manifest column is exactly the small-angle product.
        assert!((f.fov_x_deg - f.nx as f64 * f.pixscale_arcsec / 3600.0).abs() < 1e-9);

        let true_fov = f.true_fov_x_deg();
        assert!((true_fov - 12.658_26).abs() < 1e-4, "got {true_fov}");

        let rel = (true_fov - f.fov_x_deg) / f.fov_x_deg;
        assert!((rel + 0.004_071).abs() < 1e-5, "relative offset {rel}");
    }

    #[test]
    fn rejects_header_drift() {
        let bad = "name,ra,dec\ngrid_000,0.0,-60.0\n";
        assert!(parse_manifest(bad).unwrap_err().contains("header drift"));
    }

    #[test]
    fn rejects_short_row() {
        let bad = format!("{MANIFEST_HEADER}\ngrid_000,0.0,-60.0\n");
        assert!(parse_manifest(&bad).unwrap_err().contains("expected 13"));
    }
}
