// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

// HTTP/JSON control of the simulated camera (--sim_source). Served on its own
// port, separate from the client-facing gRPC/web service, and only when the
// simulator is in use. A control page is served at "/" (sim_ui.html); the
// JSON API underneath it, for scripting:
//   curl localhost:8090/sim/status
//   curl -X POST localhost:8090/sim/play
//   curl -X POST -H 'Content-Type: application/json' \
//        -d '{"path": "bursts1", "index": 40}' localhost:8090/sim/source

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Extension, Json, Router,
};
use canonical_error::CanonicalError;
use cedar_camera::sim_camera::{SimController, SimStatus};
use log::info;
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
struct StatusJson {
    source: String,
    frame_name: String,
    index: usize,
    count: usize,
    playing: bool,
    speed: f64,
    looping: bool,
    width: u32,
    height: u32,
    frame_id: i32,
}

impl From<SimStatus> for StatusJson {
    fn from(s: SimStatus) -> Self {
        StatusJson {
            source: s.source,
            frame_name: s.frame_name,
            index: s.index,
            count: s.count,
            playing: s.playing,
            speed: s.speed,
            looping: s.looping,
            width: s.width,
            height: s.height,
            frame_id: s.frame_id,
        }
    }
}

#[derive(Deserialize)]
struct SourceRequest {
    // File or directory, relative to the sim root.
    path: PathBuf,
    index: Option<usize>,
    #[serde(rename = "loop")]
    looping: Option<bool>,
    play: Option<bool>,
}

#[derive(Deserialize)]
struct StepRequest {
    n: Option<i64>,
}

#[derive(Deserialize)]
struct SeekRequest {
    index: usize,
}

#[derive(Deserialize)]
struct SpeedRequest {
    speed: f64,
}

#[derive(Deserialize)]
struct LoopRequest {
    #[serde(rename = "loop")]
    looping: bool,
}

type SimResult = Result<Json<StatusJson>, (StatusCode, String)>;

fn ok(status: SimStatus) -> SimResult {
    Ok(Json(status.into()))
}

fn bad_request(e: CanonicalError) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.message)
}

const UI_HTML: &str = include_str!("sim_ui.html");

async fn ui() -> Html<&'static str> {
    Html(UI_HTML)
}

// The sim root that SimStatus::source is relative to.
#[derive(Clone)]
struct SimRoot(Arc<PathBuf>);

// The file of the frame at the playback position, sent as is: no decode or
// re-encode, so it costs next to nothing. While playing, the position has
// already moved on to the next frame to emit, so this can be one frame ahead
// of what Cedar is processing. Browsers display PNG/JPEG/BMP; TIFF frames are
// served but most browsers won't show them.
async fn frame(
    State(ctl): State<SimController>,
    Extension(SimRoot(root)): Extension<SimRoot>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    // The controller confines the source to the root, and the frame name is a
    // bare file name, so the path stays under the root.
    let status = ctl.status();
    let source = root.join(&status.source);
    let path = if source.is_dir() {
        source.join(&status.frame_name)
    } else {
        source
    };
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Cannot read {:?}: {}", path, e),
        )
    })?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let content_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    ))
}

async fn status(State(ctl): State<SimController>) -> SimResult {
    ok(ctl.status())
}

async fn set_source(
    State(ctl): State<SimController>,
    Json(req): Json<SourceRequest>,
) -> SimResult {
    // Reads image headers of every frame; keep it off the async workers.
    let status = tokio::task::spawn_blocking(move || {
        let status = ctl.set_source(&req.path, req.index)?;
        let status = req.looping.map_or(status, |l| ctl.set_looping(l));
        Ok(match req.play {
            Some(true) => ctl.play(),
            Some(false) => ctl.pause(),
            None => status,
        })
    })
    .await
    .unwrap()
    .map_err(bad_request)?;
    ok(status)
}

async fn play(State(ctl): State<SimController>) -> SimResult {
    ok(ctl.play())
}

async fn pause(State(ctl): State<SimController>) -> SimResult {
    ok(ctl.pause())
}

async fn step(
    State(ctl): State<SimController>,
    req: Option<Json<StepRequest>>,
) -> SimResult {
    let n = req.and_then(|Json(r)| r.n).unwrap_or(1);
    ok(ctl.step(n))
}

async fn seek(
    State(ctl): State<SimController>,
    Json(req): Json<SeekRequest>,
) -> SimResult {
    ok(ctl.seek(req.index).map_err(bad_request)?)
}

async fn speed(
    State(ctl): State<SimController>,
    Json(req): Json<SpeedRequest>,
) -> SimResult {
    ok(ctl.set_speed(req.speed).map_err(bad_request)?)
}

async fn set_loop(
    State(ctl): State<SimController>,
    Json(req): Json<LoopRequest>,
) -> SimResult {
    ok(ctl.set_looping(req.looping))
}

/// `root` is the simulator's root directory (--sim_root), which the
/// controller's sources are relative to.
pub fn router(ctl: SimController, root: PathBuf) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/sim/frame", get(frame))
        .route("/sim/status", get(status))
        .route("/sim/source", post(set_source))
        .route("/sim/play", post(play))
        .route("/sim/pause", post(pause))
        .route("/sim/step", post(step))
        .route("/sim/seek", post(seek))
        .route("/sim/speed", post(speed))
        .route("/sim/loop", post(set_loop))
        .layer(Extension(SimRoot(Arc::new(root))))
        .with_state(ctl)
}

/// Serves the simulator control API on `addr` until the process exits.
pub async fn serve(
    ctl: SimController,
    root: PathBuf,
    addr: SocketAddr,
) -> Result<(), hyper::Error> {
    info!("Simulator control listening at {:?}", addr);
    axum::Server::try_bind(&addr)?
        .serve(router(ctl, root).into_make_service())
        .await
}
