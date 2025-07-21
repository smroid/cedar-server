// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use cedar_server::imu6050::Mpu6050;

use canonical_error::CanonicalError;
use env_logger;

fn main() -> Result<(), CanonicalError> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info")).init();

    log::info!("Initializing MPU-6050...");
    let mut mpu = Mpu6050::new()?;
    log::info!("MPU-6050 found and initialized successfully!");

    // Read some sample data.
    for i in 0..5 {
        let accel = mpu.get_acceleration()?;
        let gyro = mpu.get_angular_velocity()?;
        log::info!("Sample {}: Accel: x={:.2}m/s², y={:.2}m/s², z={:.2}m/s²",
                   i+1, accel.x, accel.y, accel.z);
        log::info!("Sample {}: Gyro: x={:.1}°/s, y={:.1}°/s, z={:.1}°/s",
                   i+1, gyro.x, gyro.y, gyro.z);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    Ok(())
}
