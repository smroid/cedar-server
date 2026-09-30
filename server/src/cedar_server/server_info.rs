// Copyright (c) 2026 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use std::{
    sync::{atomic::Ordering as AtomicOrdering, Arc},
    time::SystemTime,
};

use cedar_camera::abstract_camera::AbstractCamera;
use cedar_elements::{
    cedar::{
        CameraModel, ClientConnection, ConnectionStatus, FeatureLevel,
        ImuState, ImuTrackerState, ServerInformation, WiFiAccessPoint,
        WifiClient as WifiClientProto,
        WifiClientState as WifiClientStateProto,
    },
    cedar_common::WifiMode as WifiModeProto,
    wifi_trait::{
        WifiClientState as WifiClientStateDomain, WifiMode as WifiModeDomain,
    },
};
use log::warn;

use super::{device_name, ConnectionCounters, MyCedar};
use crate::cpu_stats::CpuStats;

fn wifi_mode_to_proto(mode: &WifiModeDomain) -> WifiModeProto {
    match mode {
        WifiModeDomain::AccessPoint => WifiModeProto::AccessPoint,
        WifiModeDomain::Client { .. } => WifiModeProto::Client,
        WifiModeDomain::Inactive => WifiModeProto::Inactive,
    }
}

fn wifi_client_state_to_proto(
    state: WifiClientStateDomain,
) -> WifiClientStateProto {
    match state {
        WifiClientStateDomain::Connecting => WifiClientStateProto::Connecting,
        WifiClientStateDomain::Connected => WifiClientStateProto::Connected,
        WifiClientStateDomain::NetworkNotFound => {
            WifiClientStateProto::NetworkNotFound
        }
        WifiClientStateDomain::AuthFailed => WifiClientStateProto::AuthFailed,
        WifiClientStateDomain::NoIp => WifiClientStateProto::NoIp,
    }
}

// Captures the MyCedar fields needed by get_server_information, all of which
// are Clone (strings or Arcs). Used to pass context into spawned tasks that
// don't have access to &self.
pub(super) struct ServerInfoCtx {
    state: Arc<tokio::sync::Mutex<super::CedarState>>,
    test_image_camera:
        Option<Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>>,
    demo_images: Vec<String>,
    product_name: String,
    copyright: String,
    feature_level: FeatureLevel,
    cedar_version: String,
    processor_model: String,
    os_version: String,
    serial_number: String,
    connection_counters: Arc<ConnectionCounters>,
    cpu_stats: Arc<CpuStats>,
}

impl MyCedar {
    pub(super) async fn camera_model_from_arc(
        camera_arc: &Arc<tokio::sync::Mutex<Box<dyn AbstractCamera + Send>>>,
        model_override: Option<String>,
        include_detail: bool,
    ) -> CameraModel {
        let locked = camera_arc.lock().await;
        let (w, h) = locked.dimensions().await;
        let model = match model_override {
            Some(m) => m,
            None => locked.model().await,
        };
        let model_detail = if include_detail {
            locked.model_detail().await
        } else {
            None
        };
        CameraModel {
            model,
            model_detail,
            image_width: w as i32,
            image_height: h as i32,
        }
    }

    pub(super) fn server_info_ctx(&self) -> ServerInfoCtx {
        ServerInfoCtx {
            state: self.state.clone(),
            test_image_camera: self.test_image_camera.clone(),
            demo_images: self.demo_images.clone(),
            product_name: self.product_name.clone(),
            copyright: self.copyright.clone(),
            feature_level: self.feature_level,
            cedar_version: self.cedar_version.clone(),
            processor_model: self.processor_model.clone(),
            os_version: self.os_version.clone(),
            serial_number: self.serial_number.clone(),
            connection_counters: self.connection_counters.clone(),
            cpu_stats: self.cpu_stats.clone(),
        }
    }

    pub(super) async fn get_server_information(&self) -> ServerInformation {
        MyCedar::get_server_information_ctx(&self.server_info_ctx()).await
    }

    pub(super) async fn get_server_information_ctx(
        ctx: &ServerInfoCtx,
    ) -> ServerInformation {
        // Extract all data we need first, then release state lock.
        let (
            demo_image,
            camera_arc,
            attached_camera_arc,
            wifi_arc,
            imu_tracker_arc,
        ) = {
            let locked_state = ctx.state.lock().await;
            (
                locked_state.operation_settings.demo_image_filename.clone(),
                locked_state.camera.clone(),
                locked_state.attached_camera.clone(),
                locked_state.wifi.clone(),
                locked_state.imu_tracker.clone(),
            )
        }; // State lock released here!

        // Process camera info.
        let camera = if let Some(demo_image) = &demo_image {
            Some(
                MyCedar::camera_model_from_arc(
                    &camera_arc,
                    Some(demo_image.to_string()),
                    // include_detail=
                    false,
                )
                .await,
            )
        } else if let Some(test_image_camera) = &ctx.test_image_camera {
            Some(
                MyCedar::camera_model_from_arc(
                    test_image_camera,
                    None,
                    // include_detail=
                    false,
                )
                .await,
            )
        } else if let Some(attached_camera) = &attached_camera_arc {
            Some(
                MyCedar::camera_model_from_arc(
                    attached_camera,
                    None,
                    // include_detail=
                    true,
                )
                .await,
            )
        } else {
            None
        };

        let mut server_info = ServerInformation {
            product_name: ctx.product_name.clone(),
            copyright: ctx.copyright.clone(),
            cedar_server_version: ctx.cedar_version.clone(),
            feature_level: ctx.feature_level as i32,
            processor_model: ctx.processor_model.clone(),
            os_version: ctx.os_version.clone(),
            serial_number: ctx.serial_number.clone(),
            // Filled in below, once the access point's SSID is known.
            device_name: None,
            cpu_temperature: 0.0,
            server_time: None,
            camera,
            imu: None,
            imu_angular_speed: None,
            imu_model: None,
            imu_tracker_state: None,
            wifi_mode: None,
            wifi_access_point: None,
            wifi_client: None,
            connection_status: Some({
                let cedar_wifi_clients: Vec<ClientConnection> = ctx
                    .connection_counters
                    .cedar_wifi_clients
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(addr, entry)| ClientConnection {
                        device_model: entry.device_model.clone(),
                        address: Some(addr.to_string()),
                    })
                    .collect();
                let cedar_bluetooth_clients: Vec<ClientConnection> = ctx
                    .connection_counters
                    .cedar_bluetooth_clients
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(addr, entry)| ClientConnection {
                        device_model: entry.device_model.clone(),
                        address: Some(addr.to_string()),
                    })
                    .collect();
                #[allow(deprecated)]
                ConnectionStatus {
                    // Deprecated legacy counts, kept for old clients;
                    // superseded by the length of the *_clients lists.
                    cedar_wifi: cedar_wifi_clients.len() as i32,
                    cedar_bluetooth: cedar_bluetooth_clients.len() as i32,
                    lx200_wifi: ctx
                        .connection_counters
                        .lx200_wifi
                        .load(AtomicOrdering::Relaxed)
                        as i32,
                    lx200_bluetooth: ctx
                        .connection_counters
                        .lx200_bluetooth
                        .load(AtomicOrdering::Relaxed)
                        as i32,
                    cedar_wifi_clients,
                    cedar_bluetooth_clients,
                }
            }),
            demo_image_names: ctx.demo_images.clone(),
            system_load_average: None,
            cpu_core_count: None,
            cedar_load_average: None,
        };

        // Process IMU info (outside state lock).
        if let Some(imu_tracker) = &imu_tracker_arc {
            let locked_imu = imu_tracker.lock().await;
            // Get IMU model and tracker state.
            server_info.imu_model = Some(locked_imu.get_model());
            let tracker_state = locked_imu.get_tracker_state().await;
            server_info.imu_tracker_state = Some(match tracker_state {
                cedar_elements::imu_trait::TrackerState::Motionless => {
                    ImuTrackerState::Motionless as i32
                }
                cedar_elements::imu_trait::TrackerState::Moving => {
                    ImuTrackerState::Moving as i32
                }
                cedar_elements::imu_trait::TrackerState::Lost => {
                    ImuTrackerState::Lost as i32
                }
            });

            // Get the most recent IMU state.
            match locked_imu.get_state().await {
                Ok((imu_state, _)) => {
                    server_info.imu = Some(ImuState {
                        accel_x: imu_state.accel.x,
                        accel_y: imu_state.accel.y,
                        accel_z: imu_state.accel.z,
                        angle_rate_x: imu_state.gyro.x,
                        angle_rate_y: imu_state.gyro.y,
                        angle_rate_z: imu_state.gyro.z,
                    });
                }
                Err(e) => {
                    warn!("Failed to get IMU state: {:?}", e);
                }
            }
            if let Ok((angle_speed, _)) =
                locked_imu.get_angular_velocity_magnitude().await
            {
                server_info.imu_angular_speed = Some(angle_speed);
            }
        }

        // Process wifi info (outside state lock). Left as None if this server
        // has no WiFi control at all.
        let mut ap_ssid = None;
        if let Some(wifi) = &wifi_arc {
            // Read guard: this is on the get_frame hot path, and must not
            // wait behind a slow WiFi operation such as a ~3 second scan.
            let locked_wifi = wifi.read().await;

            let mode = locked_wifi.mode();
            server_info.wifi_mode = Some(wifi_mode_to_proto(&mode) as i32);

            // The access point config is reported whenever one is configured,
            // regardless of the current mode -- `enabled` conveys whether it
            // is the mode in effect.
            if let Some(ap) = locked_wifi.access_point() {
                ap_ssid = Some(ap.ssid.clone());
                server_info.wifi_access_point = Some(WiFiAccessPoint {
                    ssid: Some(ap.ssid),
                    psk: Some(ap.psk),
                    channel: Some(ap.channel),
                    enabled: Some(mode == WifiModeDomain::AccessPoint),
                });
            }

            if let Some(status) = locked_wifi.client_status() {
                server_info.wifi_client = Some(WifiClientProto {
                    ssid: Some(status.ssid),
                    state: Some(wifi_client_state_to_proto(status.state) as i32),
                    ip_address: status.ip_address,
                });
            }
        }
        server_info.device_name =
            Some(device_name(ap_ssid, &ctx.serial_number));

        server_info.cpu_temperature = ctx.cpu_stats.get_temperature().await;
        server_info.system_load_average =
            Some(ctx.cpu_stats.get_system_load().await);
        server_info.cpu_core_count = Some(ctx.cpu_stats.core_count());
        server_info.cedar_load_average =
            Some(ctx.cpu_stats.get_cedar_process_load().await);

        server_info.server_time =
            Some(prost_types::Timestamp::from(SystemTime::now()));

        server_info
    }
}
