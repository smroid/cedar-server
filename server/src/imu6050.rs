// Copyright (c) 2025 Steven Rosenthal smr@dt3.org
// See LICENSE file in root directory for license terms.

use rppal::i2c::I2c;
use canonical_error::{CanonicalError,
                      failed_precondition_error, internal_error,
                      invalid_argument_error, unavailable_error};
use log::{info, debug};

#[derive(Debug, Clone, Copy)]
pub struct AccelData {
    // m/s².
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct GyroData {
    // Degrees/second.
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

// MPU-6050 constants.
const MPU6050_ADDR: u16 = 0x68;
const WHO_AM_I_REG: u8 = 0x75;
const PWR_MGMT_1_REG: u8 = 0x6B;
const SMPLRT_DIV: u8 = 0x19;
const CONFIG: u8 = 0x1A;
const GYRO_CONFIG: u8 = 0x1B;
const EXPECTED_WHO_AM_I: u8 = 0x68;

// Data register addresses (starting addresses for consecutive reads).
const ACCEL_XOUT_H: u8 = 0x3B;  // 6 bytes: X_H, X_L, Y_H, Y_L, Z_H, Z_L
const GYRO_XOUT_H: u8 = 0x43;   // 6 bytes: X_H, X_L, Y_H, Y_L, Z_H, Z_L

// Scale factors for converting raw values to physical units.
const ACCEL_SCALE_FACTOR: f64 = 16384.0;  // For ±2g range.
const GYRO_SCALE_FACTOR: f64 = 131.0;  // For ±250°/s range.
const G: f64 = 9.81;  // Standard gravity in m/s².

pub struct Mpu6050 {
    i2c: I2c,
}

/// Low level interface to IMU. Accumulates IMU data via a worker task (TBD)
/// and provides access to the collected acceleration and angle rate data.
/// This layer does not provide zero calibration or data integration.
impl Mpu6050 {
    /// Create a new MPU-6050 instance; returns error if device presence could
    /// not be verified.
    pub fn new() -> Result<Self, CanonicalError> {
        let mut i2c = I2c::new()
            .map_err(|e| unavailable_error(
                &format!("Failed to initialize I2C: {:?}", e)))?;
        i2c.set_slave_address(MPU6050_ADDR)
            .map_err(|e| invalid_argument_error(
                &format!("Failed to set I2C slave address: {:?}", e)))?;

        let mut mpu = Mpu6050 { i2c };

        // Test communication and verify device
        mpu.verify_device()?;

        // Wake up the device (it starts in sleep mode)
        mpu.wake_up()?;

        info!("MPU-6050 successfully initialized!");

        Ok(mpu)
    }

    /// Verify this is actually an MPU-6050.
    fn verify_device(&mut self) -> Result<(), CanonicalError> {
        let who_am_i = self.read_register(WHO_AM_I_REG)?;

        if who_am_i == EXPECTED_WHO_AM_I {
            info!("Device verified: MPU-6050 (WHO_AM_I: 0x{:02X})", who_am_i);
            Ok(())
        } else {
            Err(failed_precondition_error(
                &format!("Wrong device ID: expected 0x{:02X}, got 0x{:02X}",
                         EXPECTED_WHO_AM_I, who_am_i)))
        }
    }

    /// Wake up the device from sleep mode and initialize it.
    fn wake_up(&mut self) -> Result<(), CanonicalError> {
        // Perform device reset first for clean state.
        self.write_register(PWR_MGMT_1_REG, 0x80)?;
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Wake up the device (clock source = X gyro).
        self.write_register(PWR_MGMT_1_REG, 0x01)?;
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Set sample rate to 50Hz (1kHz / (19 + 1) = 50Hz).
        self.write_register(SMPLRT_DIV, 19)?;

        // Configure low-pass filter (5Hz bandwidth for smooth data).
        self.write_register(CONFIG, 6)?;

        // Set gyro full scale to ±250°/s (default, but explicit).
        self.write_register(GYRO_CONFIG, 0)?;

        // Verify we can still communicate.
        let _who_am_i = self.read_register(WHO_AM_I_REG)?;

        info!("Device configured: 50Hz sample rate, 5Hz filter, ±250°/s gyro, ±2g accel");
        Ok(())
    }

    /// Read a single register.
    fn read_register(&mut self, register: u8) -> Result<u8, CanonicalError> {
        let mut buffer = [0u8; 1];
        self.i2c.write_read(&[register], &mut buffer)
            .map_err(|e| internal_error(
                &format!("Failed to read register: {:?}", e)))?;
        Ok(buffer[0])
    }

    /// Write to a single register.
    fn write_register(&mut self, register: u8, value: u8) -> Result<(), CanonicalError> {
        self.i2c.write(&[register, value])
            .map_err(|e| internal_error(
                &format!("Failed to write register: {:?}", e)))?;
        Ok(())
    }

    /// Read all 6 bytes of XYZ data in a single I2C transaction.
    fn read_xyz_data(&mut self, start_reg: u8) -> Result<(i16, i16, i16), CanonicalError> {
        let mut buffer = [0u8; 6];
        self.i2c.write_read(&[start_reg], &mut buffer)
            .map_err(|e| internal_error(
                &format!("Failed to read XYZ data: {:?}", e)))?;

        // Parse 3 consecutive 16-bit values (big-endian).
        let x = ((buffer[0] as i16) << 8) | (buffer[1] as i16);
        let y = ((buffer[2] as i16) << 8) | (buffer[3] as i16);
        let z = ((buffer[4] as i16) << 8) | (buffer[5] as i16);

        Ok((x, y, z))
    }

    /// Get current acceleration data in m/s².
    pub fn get_acceleration(&mut self) -> Result<AccelData, CanonicalError> {
        let (accel_x_raw, accel_y_raw, accel_z_raw) = self.read_xyz_data(ACCEL_XOUT_H)?;
        debug!("Raw accel: x={}, y={}, z={}", accel_x_raw, accel_y_raw, accel_z_raw);

        // Convert raw to g-force, then to m/s².
        Ok(AccelData {
            x: (accel_x_raw as f64 / ACCEL_SCALE_FACTOR) * G,
            y: (accel_y_raw as f64 / ACCEL_SCALE_FACTOR) * G,
            z: (accel_z_raw as f64 / ACCEL_SCALE_FACTOR) * G,
        })
    }

    /// Get current angular velocity data in degrees/second.
    pub fn get_angular_velocity(&mut self) -> Result<GyroData, CanonicalError> {
        let (gyro_x_raw, gyro_y_raw, gyro_z_raw) = self.read_xyz_data(GYRO_XOUT_H)?;
        debug!("Raw gyro: x={}, y={}, z={}", gyro_x_raw, gyro_y_raw, gyro_z_raw);

        Ok(GyroData {
            x: gyro_x_raw as f64 / GYRO_SCALE_FACTOR,
            y: gyro_y_raw as f64 / GYRO_SCALE_FACTOR,
            z: gyro_z_raw as f64 / GYRO_SCALE_FACTOR,
        })
    }
}
