// Copyright (c) 2026 vivo Mobile Communication Co., Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//       http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// QMI8658 IMU sensor data poller for the ImuPage.

use crate::app_window::MainWindow;
use crate::syscall_error;
use crate::uptime_millis;
use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const IMU_DEVICE_PATH: &[u8] = b"/dev/qmi86580\0";
const IMU_REPORT_SIZE: usize = 16;
const IMU_REPORT_VERSION: u8 = 1;
const POLL_MS: u64 = 1000; // 1 Hz refresh rate

/// Low-pass filter coefficient when the value is stable (small changes).
/// Used when the change from raw to old is below the threshold.
const FILTER_ALPHA_STABLE: f32 = 0.15;

/// Low-pass filter coefficient when the value is changing rapidly.
/// Used when the change from raw to old exceeds the threshold.
const FILTER_ALPHA_FAST: f32 = 0.7;

/// Threshold for switching between stable and fast filter (m/s² for accel, °/s for gyro).
/// If |raw - old| > threshold, use fast filter.
const FILTER_THRESHOLD: f32 = 1.0;

/// Gyro dead zone: values below this threshold (°/s) are clamped to zero.
/// Applied both before and after low-pass filtering.
const GYRO_DEAD_ZONE: f32 = 4.0;

/// Accel noise gate: max absolute change per tick (m/s²) to accept.
/// Changes larger than this are clamped.  Helps suppress random spikes.
const ACCEL_MAX_DELTA: f32 = 5.0;

/// Decoded QMI8658 sensor data from the kernel driver's binary report.
///
/// Layout (little-endian, version byte at offset 0):
///   [0]  version (u8)
///   [1]  reserved
///   [2..8]  accel X, Y, Z (3 × i16 LE, raw ADC)
///   [8..14] gyro  X, Y, Z (3 × i16 LE, raw ADC)
///   [14] temperature high (u8)
///   [15] temperature low (u8)
///
/// Scale factors for the configured ranges:
///   accel: ±2g → 0.009576 m/s² per LSB  (2.0 / 32768.0 * 9.80665)
///   gyro:  ±512°/s → 0.015625 °/s per LSB  (512.0 / 32768.0)
struct ImuReport {
    accel_x: f32,
    accel_y: f32,
    accel_z: f32,
    gyro_x: f32,
    gyro_y: f32,
    gyro_z: f32,
}

impl ImuReport {
    fn decode(bytes: &[u8; IMU_REPORT_SIZE]) -> IoResult<Self> {
        if bytes[0] != IMU_REPORT_VERSION {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid IMU report version",
            ));
        }

        // Accel: raw ADC × (±2g / 32768) × 9.80665 m/s²/g
        let accel_scale = 2.0 / 32768.0 * 9.80665;
        let accel_x = i16::from_le_bytes([bytes[2], bytes[3]]) as f32 * accel_scale;
        let accel_y = i16::from_le_bytes([bytes[4], bytes[5]]) as f32 * accel_scale;
        let accel_z = i16::from_le_bytes([bytes[6], bytes[7]]) as f32 * accel_scale;

        // Gyro: raw ADC × (±512°/s / 32768)
        let gyro_scale = 512.0 / 32768.0;
        let mut gyro_x = i16::from_le_bytes([bytes[8], bytes[9]]) as f32 * gyro_scale;
        let mut gyro_y = i16::from_le_bytes([bytes[10], bytes[11]]) as f32 * gyro_scale;
        let mut gyro_z = i16::from_le_bytes([bytes[12], bytes[13]]) as f32 * gyro_scale;

        // Gyro dead zone: clamp near-zero drift to 0
        if gyro_x.abs() < GYRO_DEAD_ZONE { gyro_x = 0.0; }
        if gyro_y.abs() < GYRO_DEAD_ZONE { gyro_y = 0.0; }
        if gyro_z.abs() < GYRO_DEAD_ZONE { gyro_z = 0.0; }

        Ok(Self {
            accel_x,
            accel_y,
            accel_z,
            gyro_x,
            gyro_y,
            gyro_z,
        })
    }
}

struct ImuFile {
    fd: libc::c_int,
}

impl ImuFile {
    fn open() -> IoResult<Self> {
        let path = CStr::from_bytes_with_nul(IMU_DEVICE_PATH)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }
        Ok(Self { fd })
    }

    fn read_report(&self) -> IoResult<ImuReport> {
        let mut bytes = [0u8; IMU_REPORT_SIZE];
        match librs::syscall::sys::Sys::read(self.fd, &mut bytes) {
            Ok(IMU_REPORT_SIZE) => ImuReport::decode(&bytes),
            Ok(n) => Err(Error::new(
                ErrorKind::UnexpectedEof,
                format!("short IMU report: {}/{}", n, IMU_REPORT_SIZE),
            )),
            Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
        }
    }
}

impl Drop for ImuFile {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

struct ImuPoller {
    imu: Option<ImuFile>,
    // Low-pass filtered values
    last_accel_x: f32,
    last_accel_y: f32,
    last_accel_z: f32,
    last_gyro_x: f32,
    last_gyro_y: f32,
    last_gyro_z: f32,
    // Debug: print raw values every N ticks
    print_counter: u32,
}

impl ImuPoller {
    fn new() -> Self {
        Self {
            imu: None,
            last_accel_x: 0.0,
            last_accel_y: 0.0,
            last_accel_z: 0.0,
            last_gyro_x: 0.0,
            last_gyro_y: 0.0,
            last_gyro_z: 0.0,
            print_counter: 0,
        }
    }

    /// Apply adaptive low-pass filter.
    /// Uses stronger smoothing (FILTER_ALPHA_STABLE) when raw is close to old,
    /// and faster response (FILTER_ALPHA_FAST) when raw changes significantly.
    fn lowpass_adaptive(new: f32, old: f32) -> f32 {
        let delta = new - old;
        let alpha = if delta.abs() > FILTER_THRESHOLD {
            FILTER_ALPHA_FAST
        } else {
            FILTER_ALPHA_STABLE
        };
        alpha * new + (1.0 - alpha) * old
    }

    /// Rate-limit a value: cap the change from `old` to `new` at `max_delta`.
    fn ratelimit(new: f32, old: f32, max_delta: f32) -> f32 {
        let delta = new - old;
        if delta.abs() > max_delta {
            old + delta.signum() * max_delta
        } else {
            new
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        // Skip IMU reads when the IMU page is not active.
        if ui.get_current_app() != 12 {
            return;
        }
        // Suspend reads while the user is touching the screen: a redraw
        // triggered by new values takes ~500ms of SPI IO on this page and
        // would block the event loop, dropping the swipe's move events and
        // leaving GestureLayer with dx=0/dy=0 (misdetected as a tap).
        if crate::touch_is_pressed() {
            return;
        }

        // Lazily open the IMU device on first tick (the kernel driver may not
        // be ready immediately at boot).
        if self.imu.is_none() {
            match ImuFile::open() {
                Ok(imu) => {
                    println!("[IMU] dev open ok");
                    self.imu = Some(imu);
                }
                Err(_) => {
                    return;
                }
            }
        }

        let imu = self.imu.as_ref().unwrap();
        let read_start = uptime_millis();
        let raw = match imu.read_report() {
            Ok(r) => r,
            Err(_) => return,
        };
        let read_ms = uptime_millis().saturating_sub(read_start);
        if read_ms > 5 {
            println!("[IMU] read_report blocked {} ms", read_ms);
        }

        // Apply low-pass filter, then rate-limit to suppress spikes
        let accel_x = Self::ratelimit(
            Self::lowpass_adaptive(raw.accel_x, self.last_accel_x),
            self.last_accel_x, ACCEL_MAX_DELTA);
        let accel_y = Self::ratelimit(
            Self::lowpass_adaptive(raw.accel_y, self.last_accel_y),
            self.last_accel_y, ACCEL_MAX_DELTA);
        let accel_z = Self::ratelimit(
            Self::lowpass_adaptive(raw.accel_z, self.last_accel_z),
            self.last_accel_z, ACCEL_MAX_DELTA);
        let gyro_x = Self::lowpass_adaptive(raw.gyro_x, self.last_gyro_x);
        let gyro_y = Self::lowpass_adaptive(raw.gyro_y, self.last_gyro_y);
        let gyro_z = Self::lowpass_adaptive(raw.gyro_z, self.last_gyro_z);

        // Re-apply dead zone after filtering to suppress low-pass filter "tail"
        fn gyro_deadzone(v: f32) -> f32 {
            if v.abs() < GYRO_DEAD_ZONE { 0.0 } else { v }
        }
        let gyro_x = gyro_deadzone(gyro_x);
        let gyro_y = gyro_deadzone(gyro_y);
        let gyro_z = gyro_deadzone(gyro_z);

        // Only write to Slint properties when the value actually changes
        // (within a small epsilon), to avoid dirtying the frame buffer.
        let eps = 0.01;

        fn fmt_float(v: f32) -> slint::SharedString {
            // Show sign + 2 decimal places, e.g. "+1.23"
            let s = if v >= 0.0 { "+" } else { "" };
            format!("{}{:.2}", s, v).into()
        }

        if (accel_x - self.last_accel_x).abs() > eps {
            ui.set_accel_x(fmt_float(accel_x));
        }
        if (accel_y - self.last_accel_y).abs() > eps {
            ui.set_accel_y(fmt_float(accel_y));
        }
        if (accel_z - self.last_accel_z).abs() > eps {
            ui.set_accel_z(fmt_float(accel_z));
        }
        if (gyro_x - self.last_gyro_x).abs() > eps {
            ui.set_gyro_x(fmt_float(gyro_x));
        }
        if (gyro_y - self.last_gyro_y).abs() > eps {
            ui.set_gyro_y(fmt_float(gyro_y));
        }
        if (gyro_z - self.last_gyro_z).abs() > eps {
            ui.set_gyro_z(fmt_float(gyro_z));
        }

        self.last_accel_x = accel_x;
        self.last_accel_y = accel_y;
        self.last_accel_z = accel_z;
        self.last_gyro_x = gyro_x;
        self.last_gyro_y = gyro_y;
        self.last_gyro_z = gyro_z;

        // Debug: print raw and filtered values every 20 ticks (≈1s at 50ms POLL_MS)
        self.print_counter += 1;
        if self.print_counter % 20 == 0 {
            println!(
                "[IMU] raw a({:.3},{:.3},{:.3}) g({:.1},{:.1},{:.1}) | filtered a({:.3},{:.3},{:.3}) g({:.2},{:.2},{:.2})",
                raw.accel_x, raw.accel_y, raw.accel_z,
                raw.gyro_x, raw.gyro_y, raw.gyro_z,
                accel_x, accel_y, accel_z,
                gyro_x, gyro_y, gyro_z,
            );
        }
    }
}

/// Connect the IMU poller to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let poller = Rc::new(RefCell::new(ImuPoller::new()));

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(POLL_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                poller.borrow_mut().tick(&ui);
            }
        },
    );

    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_imu_page_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} imu", if active { "enter" } else { "exit" });
    });
    timer
}