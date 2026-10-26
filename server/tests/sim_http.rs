//! HTTP control API of the simulated camera (`--sim_source`), exercised
//! through the router without binding a port. An integration test rather than
//! a unit test so it builds independently of the lib's own `#[cfg(test)]` code.

use std::path::Path;

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use cedar_camera::sim_camera::SimController;
use cedar_server::sim_http::router;
use image::GrayImage;
use tower::ServiceExt;

fn controller() -> SimController {
    let dir = std::env::temp_dir()
        .join(format!("sim_http_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("seq")).unwrap();
    for name in ["a.png", "seq/b.png", "seq/c.png"] {
        GrayImage::new(4, 4).save(dir.join(name)).unwrap();
    }
    SimController::new(&dir, Path::new("a.png")).unwrap()
}

async fn call(
    ctl: &SimController,
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
    let resp = router(ctl.clone()).oneshot(req).await.unwrap();
    let code = resp.status();
    let bytes = hyper::body::to_bytes(resp.into_body()).await.unwrap();
    (code, String::from_utf8_lossy(&bytes).to_string())
}

#[tokio::test]
async fn source_step_and_errors() {
    let ctl = controller();
    let (code, body) =
        call(&ctl, "/sim/source", Some(r#"{"path": "seq", "loop": false}"#))
            .await;
    assert_eq!(code, StatusCode::OK, "{}", body);
    assert!(body.contains(r#""count":2"#), "{}", body);
    assert!(body.contains(r#""looping":false"#), "{}", body);

    let (code, body) = call(&ctl, "/sim/step", None).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.contains(r#""frame_name":"c.png""#), "{}", body);

    let (code, _) =
        call(&ctl, "/sim/source", Some(r#"{"path": "../.."}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&ctl, "/sim/seek", Some(r#"{"index": 9}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let (code, _) = call(&ctl, "/sim/speed", Some(r#"{"speed": -1}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(ctl.status().source, "seq");
}
