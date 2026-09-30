// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{io, sync::Arc, time::Duration};

use cedar_elements::{
    cedar::{
        DisplayOrientation, FrameRequest, FrameResult, Image,
        ImageFormat as ProtoImageFormat, ImageRequest, ImageResult,
    },
    thread_name::ThreadName,
};
use image::GrayImage;
use log::warn;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    bluetooth_transport::{BluetoothRequest, ConnectionKeyExtension},
    cedar_rpcs::{logged_status, GrpcTimer},
    note_rpc_received, CedarState, ConnectionKey, MyCedar,
};

// gRPC metadata header a client sends to report its device model, e.g.
// "Pixel 7" or "iPhone". Recorded per-connection for diagnostics; see
// ConnectionStatus.cedar_wifi_clients/cedar_bluetooth_clients.
const CLIENT_DEVICE_MODEL_HEADER: &str = "x-cedar-client-device-model";

impl MyCedar {
    // Quality:
    // 75: 40x compression, bad artifacts.
    // 90: 20x compression, mild artifacts.
    // 95: 13x compression, almost no artifacts.
    pub(super) fn jpeg_encode(img: &GrayImage, jpeg_quality: u8) -> Vec<u8> {
        let (width, height) = img.dimensions();
        let image = turbojpeg::Image {
            pixels: img.as_raw().as_slice(),
            width: width as usize,
            pitch: width as usize, // Grayscale: 1 byte per pixel.
            height: height as usize,
            format: turbojpeg::PixelFormat::GRAY,
        };
        let mut compressor = turbojpeg::Compressor::new().unwrap();
        compressor.set_quality(jpeg_quality as i32).unwrap();
        compressor.set_subsamp(turbojpeg::Subsamp::Gray).unwrap();
        compressor.compress_to_vec(image).unwrap()
    }

    pub(super) async fn get_next_frame(
        state: Arc<tokio::sync::Mutex<CedarState>>,
        prev_frame_id: Option<i32>,
        prev_solution_id: Option<i32>,
        non_blocking: bool,
        landscape: bool,
        is_bluetooth: bool,
    ) -> Option<FrameResult> {
        let mut frame_result = FrameResult {
            ..Default::default()
        };
        let mut fixed_settings;
        // Extract all data we need first, then release state lock.
        let (
            calibrating,
            calibration_data_arc,
            preferences_arc,
            calibration_start,
            calibration_duration_estimate,
            scaled_image_data,
            operation_settings,
            fixed_settings_arc,
            skip_focus_active,
        ) = {
            let locked_state = state.lock().await;

            // Extract calibration and image data.
            let calibrating = locked_state.calibrating;
            let calibration_data_arc = locked_state.calibration_data.clone();
            let preferences_arc = locked_state.preferences.clone();
            let calibration_start = locked_state.calibration_start;
            let calibration_duration_estimate =
                locked_state.calibration_duration_estimate;
            let operation_settings = locked_state.operation_settings.clone();

            // Clone image data if it exists (cheap Arc clone).
            let scaled_image_data =
                if calibrating && locked_state.scaled_image.is_some() {
                    Some((
                        locked_state.scaled_image.clone(),
                        locked_state.scaled_image_binning_factor,
                        locked_state.scaled_image_frame_id,
                        locked_state.scaled_image_rectangle.clone(),
                    ))
                } else {
                    None
                };

            (
                calibrating,
                calibration_data_arc,
                preferences_arc,
                calibration_start,
                calibration_duration_estimate,
                scaled_image_data,
                operation_settings,
                locked_state.fixed_settings.clone(),
                locked_state.skip_focus_active,
            )
        }; // State lock released here!

        // Access fixed_settings outside the state lock
        fixed_settings = fixed_settings_arc.lock().await.clone();

        // Fill in our current time (outside lock).
        MyCedar::fill_in_time(&mut fixed_settings);

        frame_result.skip_focus_active = skip_focus_active;

        let jpeg_quality = if is_bluetooth { 20_u8 } else { 75_u8 };

        if calibrating {
            frame_result.calibrating = true;
            let time_spent_calibrating = calibration_start.elapsed();
            let mut fraction = time_spent_calibrating.as_secs_f64()
                / calibration_duration_estimate.as_secs_f64();
            if fraction > 1.0 {
                fraction = 1.0;
            }
            frame_result.calibration_progress = Some(fraction);

            // Get calibration data (outside state lock).
            frame_result.calibration_data =
                Some(calibration_data_arc.lock().await.clone());

            if let Some((
                scaled_image_opt,
                binning_factor,
                frame_id,
                snapshot_rectangle,
            )) = scaled_image_data
            {
                if let Some(img) = scaled_image_opt {
                    let jpg_buf = tokio::task::spawn_blocking(move || {
                        let _name = ThreadName::new("jpeg-calib");
                        MyCedar::jpeg_encode(&img, jpeg_quality)
                    })
                    .await
                    .unwrap_or_default();
                    frame_result.image = Some(Image {
                        binning_factor: binning_factor as i32,
                        rotation_size_ratio: 1.0,
                        // The rectangle ServeEngine paired with these pixels:
                        // the centered square crop the live view was showing,
                        // in full resolution coordinates.
                        rectangle: snapshot_rectangle,
                        image_data: jpg_buf,
                    });
                    frame_result.frame_id = frame_id;
                    frame_result.fixed_settings = Some(fixed_settings.clone());
                    // Get preferences (outside state lock)
                    frame_result.preferences =
                        Some(preferences_arc.lock().await.clone());
                    frame_result.operation_settings = Some(operation_settings);
                }
            }
            if non_blocking {
                frame_result.has_result = Some(true);
            }
            return Some(frame_result);
        } // Calibrating.

        // Update per-request render parameters before polling.
        let serve_engine = state.lock().await.serve_engine.clone();
        serve_engine
            .lock()
            .await
            .update_render_params(landscape, jpeg_quality)
            .await;

        // Delegate all image processing to the ServeEngine.
        let serve_result = serve_engine
            .lock()
            .await
            .get_next_result(prev_frame_id, prev_solution_id, non_blocking)
            .await;
        if serve_result.is_none() {
            return None;
        }

        let mut frame_result = serve_result.unwrap().frame_result;
        frame_result.skip_focus_active = skip_focus_active;

        Some(frame_result)
    } // get_next_frame().

    pub(super) async fn get_frame_impl(
        &self,
        request: tonic::Request<FrameRequest>,
    ) -> Result<tonic::Response<FrameResult>, tonic::Status> {
        let _timer =
            GrpcTimer::with_threshold("get_frame", Duration::from_millis(200));
        let is_bluetooth =
            request.extensions().get::<BluetoothRequest>().is_some();
        self.note_client_device_model(&request);
        self.note_rpc_received().await;

        let req: FrameRequest = request.into_inner();
        let non_blocking =
            req.non_blocking.is_some() && req.non_blocking.unwrap();
        let landscape = req.display_orientation.is_none()
            || req.display_orientation.unwrap()
                == DisplayOrientation::Landscape as i32;
        let fr = Self::get_next_frame(
            self.state.clone(),
            req.prev_frame_id,
            req.prev_solution_id,
            non_blocking,
            landscape,
            is_bluetooth,
        )
        .await;
        let mut frame_result = FrameResult {
            ..Default::default()
        };
        if fr.is_none() {
            assert!(non_blocking);
            frame_result.has_result = Some(false);
        } else {
            frame_result = fr.unwrap();
            if non_blocking {
                frame_result.has_result = Some(true);
            }
        }
        frame_result.server_information =
            Some(self.get_server_information().await);
        Ok(tonic::Response::new(frame_result))
    }

    pub(super) async fn get_frames_impl(
        &self,
        request: tonic::Request<FrameRequest>,
    ) -> Result<
        tonic::Response<ReceiverStream<Result<FrameResult, tonic::Status>>>,
        tonic::Status,
    > {
        let is_bluetooth =
            request.extensions().get::<BluetoothRequest>().is_some();
        self.note_client_device_model(&request);
        self.note_rpc_received().await;

        let req: FrameRequest = request.into_inner();
        let landscape = req.display_orientation.is_none()
            || req.display_orientation.unwrap()
                == DisplayOrientation::Landscape as i32;

        // Producer task feeds frames into a "latest-only" slot; a separate
        // forwarder task hands them to tonic. When tonic is slow (WiFi
        // backpressure), the producer keeps updating the slot with fresher
        // frames rather than queueing stale ones. The forwarder always picks
        // up the most recent frame available.
        let state = self.state.clone();
        let server_info_ctx = self.server_info_ctx();
        let latest: Arc<tokio::sync::Mutex<Option<FrameResult>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let notify = Arc::new(tokio::sync::Notify::new());
        let (tx, rx) = tokio::sync::mpsc::channel(1);

        // Producer: always writes the newest frame into `latest`, replacing
        // any previous one, then wakes the forwarder. Frames arriving faster
        // than the rate limit are silently dropped. Bluetooth bandwidth is
        // ~700 kbps usable vs. ~5 Mbps over WiFi, so we cap at 10 Hz on
        // Bluetooth and 20 Hz on WiFi.
        let latest_p = latest.clone();
        let notify_p = notify.clone();
        let producer = tokio::spawn(async move {
            let mut prev_frame_id = req.prev_frame_id;
            let mut prev_solution_id = req.prev_solution_id;
            let mut next_emit = tokio::time::Instant::now();
            loop {
                let fr = MyCedar::get_next_frame(
                    state.clone(),
                    prev_frame_id,
                    prev_solution_id,
                    // non_blocking=
                    false,
                    landscape,
                    is_bluetooth,
                )
                .await;
                let mut frame_result = fr.unwrap_or_default();
                prev_frame_id = Some(frame_result.frame_id);
                prev_solution_id = if frame_result.solution_id != 0 {
                    Some(frame_result.solution_id)
                } else {
                    None
                };
                // Throttle: sleep until the slot is due, then emit.
                tokio::time::sleep_until(next_emit).await;
                let interval = if is_bluetooth {
                    tokio::time::Duration::from_millis(100) // 10 Hz
                } else {
                    tokio::time::Duration::from_millis(50) // 20 Hz
                };
                next_emit = tokio::time::Instant::now() + interval;
                frame_result.server_information = Some(
                    MyCedar::get_server_information_ctx(&server_info_ctx).await,
                );
                *latest_p.lock().await = Some(frame_result);
                notify_p.notify_one();
            }
        });

        // Forwarder: waits for a frame, sends to tonic (may block on WiFi),
        // then loops. If it sees `latest` already populated when it comes
        // back around, it drains it immediately without extra wait.
        tokio::spawn(async move {
            loop {
                let frame = {
                    let mut guard = latest.lock().await;
                    guard.take()
                };
                let frame = match frame {
                    Some(f) => f,
                    None => {
                        notify.notified().await;
                        continue;
                    }
                };
                if tx.send(Ok(frame)).await.is_err() {
                    // Client disconnected; stop the producer too.
                    producer.abort();
                    break;
                }
            }
        });

        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }

    pub(super) async fn get_image_impl(
        &self,
        request: tonic::Request<ImageRequest>,
    ) -> Result<
        tonic::Response<ReceiverStream<Result<ImageResult, tonic::Status>>>,
        tonic::Status,
    > {
        // No GrpcTimer here: get_image() is expected to block for up to the
        // current exposure duration.
        let req: ImageRequest = request.into_inner();
        let format = ProtoImageFormat::try_from(req.format)
            .unwrap_or(ProtoImageFormat::Unspecified);

        let (detect_engine, camera_arc, calibrating) = {
            let locked_state = self.state.lock().await;
            (
                locked_state.detect_engine.clone(),
                locked_state.camera.clone(),
                locked_state.calibrating,
            )
        };
        if calibrating {
            return Err(logged_status!(
                failed_precondition,
                "Cannot get image while a calibration is in progress."
            ));
        }
        let detect_result = detect_engine
            .lock()
            .await
            .get_next_result(req.prev_frame_id, /*non_blocking=*/ false)
            .await
            .ok_or_else(|| {
                logged_status!(
                    failed_precondition,
                    "Images are not currently being acquired."
                )
            })?;
        let frame_id = detect_result.frame_id;
        let captured_image = detect_result.captured_image;
        let camera_model = MyCedar::camera_model_from_arc(
            &camera_arc,
            /*model_override=*/ None,
            /*include_detail=*/ true,
        )
        .await;

        let quality = req.quality.unwrap_or(90).clamp(1, 100) as u8;
        let image = captured_image.image.clone();
        // Encode on Tokio's blocking thread pool so this CPU-bound work doesn't
        // stall the async runtime.
        let encoded =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
                let _name = ThreadName::new("jpeg-getimage");
                match format {
                    ProtoImageFormat::Bmp => {
                        let mut buf = io::Cursor::new(Vec::<u8>::new());
                        image
                            .write_to(&mut buf, image::ImageFormat::Bmp)
                            .map_err(|e| {
                                format!("Failed to encode BMP: {:?}", e)
                            })?;
                        Ok(buf.into_inner())
                    }
                    ProtoImageFormat::Jpeg | ProtoImageFormat::Unspecified => {
                        Ok(MyCedar::jpeg_encode(&image, quality))
                    }
                }
            })
            .await
            .map_err(|e| {
                tonic::Status::internal(format!(
                    "Encoding task failed: {:?}",
                    e
                ))
            })?
            .map_err(tonic::Status::internal)?;

        let (width, height) = captured_image.image.dimensions();
        let first_result = ImageResult {
            frame_id: Some(frame_id),
            width: Some(width as i32),
            height: Some(height as i32),
            acquire_time: Some(prost_types::Timestamp::from(
                captured_image.readout_time,
            )),
            exposure_time: Some(
                prost_types::Duration::try_from(
                    captured_image.capture_params.exposure_duration,
                )
                .unwrap(),
            ),
            camera_gain: Some(captured_image.capture_params.gain.value()),
            camera: Some(camera_model),
            ..Default::default()
        };

        // Chunk the encoded image to stay under gRPC's default per-message
        // size limit.
        const CHUNK_SIZE: usize = 1 << 20; // 1 MiB.
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move {
            let mut first = Some(first_result);
            if encoded.is_empty() {
                let mut result = first.take().unwrap();
                result.image_chunk = Vec::new();
                let _ = tx.send(Ok(result)).await;
                return;
            }
            for chunk in encoded.chunks(CHUNK_SIZE) {
                let mut result = first.take().unwrap_or_default();
                result.image_chunk = chunk.to_vec();
                if tx.send(Ok(result)).await.is_err() {
                    return;
                }
            }
        });

        Ok(tonic::Response::new(ReceiverStream::new(rx)))
    }

    /// Notifies the activity LED and (if present) the Wifi implementation
    /// that an RPC was received.
    async fn note_rpc_received(&self) {
        let (activity_led, wifi) = {
            let locked_state = self.state.lock().await;
            (locked_state.activity_led.clone(), locked_state.wifi.clone())
        };
        note_rpc_received(&activity_led, &wifi).await;
    }

    /// If `request` carries a `ConnectionKeyExtension` and a non-empty
    /// `CLIENT_DEVICE_MODEL_HEADER` metadata value, records that device
    /// model on the connection's entry in `connection_counters`. Silently
    /// does nothing if either is absent, or if the connection's entry is
    /// no longer present (should not normally happen).
    fn note_client_device_model<T>(&self, request: &tonic::Request<T>) {
        let Some(ConnectionKeyExtension(key)) = request
            .extensions()
            .get::<ConnectionKeyExtension>()
            .copied()
        else {
            return;
        };
        let Some(device_model) = request
            .metadata()
            .get(CLIENT_DEVICE_MODEL_HEADER)
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
        else {
            return;
        };
        match key {
            ConnectionKey::Wifi(addr) => {
                if let Some(entry) = self
                    .connection_counters
                    .cedar_wifi_clients
                    .lock()
                    .unwrap()
                    .get_mut(&addr)
                {
                    entry.device_model = Some(device_model);
                }
            }
            ConnectionKey::Bluetooth(addr) => {
                if let Some(entry) = self
                    .connection_counters
                    .cedar_bluetooth_clients
                    .lock()
                    .unwrap()
                    .get_mut(&addr)
                {
                    entry.device_model = Some(device_model);
                }
            }
        }
    }
}
