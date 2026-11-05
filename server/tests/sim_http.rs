//! HTTP control API of the simulated camera (`--sim_source`), exercised
//! through the router without binding a port. An integration test rather than
//! a unit test so it builds independently of the lib's own `#[cfg(test)]` code.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use cedar_camera::sim_camera::SimController;
use cedar_server::sim_http::router;
use image::GrayImage;
use tower::ServiceExt;

// Tests run in parallel; each gets its own directory.
static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Sim {
    ctl: SimController,
    root: PathBuf,
}

// Frames differ by their first pixel, so served files can be told apart.
fn sim() -> Sim {
    let dir = std::env::temp_dir().join(format!(
        "sim_http_test_{}_{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("seq")).unwrap();
    for (i, name) in ["a.png", "seq/b.png", "seq/c.png"].iter().enumerate() {
        let mut img = GrayImage::new(4, 4);
        img.put_pixel(0, 0, image::Luma([i as u8 + 1]));
        img.save(dir.join(name)).unwrap();
    }
    Sim {
        ctl: SimController::new(&dir, Path::new("a.png")).unwrap(),
        root: dir.canonicalize().unwrap(),
    }
}

async fn call(
    sim: &Sim,
    uri: &str,
    body: Option<&str>,
) -> (StatusCode, String) {
    let req = match body {
        None => Request::post(uri).body(Body::empty()),
        Some(b) => Request::post(uri)
            .header("content-type", "application/json")
            .body(Body::from(b.to_string())),
    }
    .unwrap();
    let resp = router(sim.ctl.clone(), sim.root.clone())
        .oneshot(req)
        .await
        .unwrap();
    let code = resp.status();
    let bytes = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    (code, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn source_step_and_errors() {
    let sim = sim();
    let (code, body) =
        call(&sim, "/sim/source", Some(r#"{"path": "seq", "loop": false}"#))
            .await;
    assert_eq!(code, StatusCode::OK, "{}", body);
    assert!(body.contains(r#""count":2"#), "{}", body);
    assert!(body.contains(r#""looping":false"#), "{}", body);

    let (code, body) = call(&sim, "/sim/step", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains(r#""frame_name":"c.png""#), "{}", body);

    let (code, _) =
        call(&sim, "/sim/source", Some(r#"{"path": "../.."}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&sim, "/sim/seek", Some(r#"{"index": 9}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&sim, "/sim/speed", Some(r#"{"speed": -1}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(sim.ctl.status().source, "seq");
}

async fn get(sim: &Sim, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let req = Request::get(uri).body(Body::empty()).unwrap();
    let resp = router(sim.ctl.clone(), sim.root.clone())
        .oneshot(req)
        .await
        .unwrap();
    let code = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let bytes = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    (code, content_type, bytes.to_vec())
}

#[tokio::test]
async fn serves_control_page() {
    let (code, content_type, body) = get(&sim(), "/").await;
    assert_eq!(code, StatusCode::OK);
    assert!(content_type.starts_with("text/html"), "{}", content_type);
    assert!(String::from_utf8_lossy(&body).contains("/sim/status"));
}

#[tokio::test]
async fn frame_serves_the_current_file() {
    let sim = sim();
    // Single-file source.
    let (code, content_type, body) = get(&sim, "/sim/frame").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(content_type, "image/png");
    assert_eq!(body, std::fs::read(sim.root.join("a.png")).unwrap());

    // Directory source, after a step.
    let (code, _) = call(&sim, "/sim/source", Some(r#"{"path": "seq"}"#)).await;
    assert_eq!(code, StatusCode::OK);
    call(&sim, "/sim/step", None).await;
    let (code, _, body) = get(&sim, "/sim/frame").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, std::fs::read(sim.root.join("seq/c.png")).unwrap());
}
