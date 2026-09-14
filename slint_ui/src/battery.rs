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

use crate::app_window::MainWindow;
use crate::{syscall_error, uptime_millis};
use librs::{c_str::CStr, syscall::Syscall};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const BATTERY_DEVICE_PATH: &[u8] = b"/dev/battery\0";
const BATTERY_REPORT_SIZE: usize = 8;
const BATTERY_REPORT_VERSION: u8 = 1;
const POLL_INTERVAL_MS: u128 = 2_000;
const TIMER_INTERVAL_MS: u64 = 250;

struct BatteryReading {
    voltage_mv: u16,
    percent: u8,
    charging_state: u8,
}

fn read_battery() -> IoResult<BatteryReading> {
    let path = CStr::from_bytes_with_nul(BATTERY_DEVICE_PATH)
        .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    let mut report = [0u8; BATTERY_REPORT_SIZE];
    let result = librs::syscall::sys::Sys::read(fd, &mut report);
    let _ = librs::syscall::sys::Sys::close(fd);

    let size = result.map_err(|librs::errno::Errno(errno)| Error::from_raw_os_error(errno))?;
    if size != BATTERY_REPORT_SIZE {
        return Err(Error::new(ErrorKind::UnexpectedEof, "short battery report"));
    }
    if report[0] != BATTERY_REPORT_VERSION {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "unsupported battery report",
        ));
    }

    Ok(BatteryReading {
        voltage_mv: u16::from_le_bytes([report[1], report[2]]),
        percent: report[3].min(100),
        charging_state: report[4],
    })
}

fn charging_state_text(state: u8) -> &'static str {
    match state {
        0 => "待机",
        1 => "正在充电",
        2 => "正在放电",
        3 => "恒压充电",
        4 => "充电完成",
        5 => "未充电",
        _ => "状态未知",
    }
}

struct BatteryMonitor {
    active: bool,
    refresh_requested: bool,
    last_poll_ms: u128,
}

impl BatteryMonitor {
    fn new() -> Self {
        Self {
            active: false,
            refresh_requested: true,
            last_poll_ms: 0,
        }
    }

    fn set_active(&mut self, active: bool) {
        if active && !self.active {
            self.refresh_requested = true;
        }
        self.active = active;
    }

    fn tick(&mut self, ui: &MainWindow) {
        if !self.active {
            return;
        }

        let now = uptime_millis();
        if !self.refresh_requested && now.saturating_sub(self.last_poll_ms) < POLL_INTERVAL_MS {
            return;
        }
        self.refresh_requested = false;
        self.last_poll_ms = now;

        match read_battery() {
            Ok(reading) => {
                let present = reading.voltage_mv != 0;
                ui.set_battery_available(present);
                ui.set_battery_percent(reading.percent as i32);
                ui.set_battery_voltage(
                    format!(
                        "{}.{:03} V",
                        reading.voltage_mv / 1000,
                        reading.voltage_mv % 1000
                    )
                    .into(),
                );
                ui.set_battery_state(charging_state_text(reading.charging_state).into());
                ui.set_battery_status(
                    if present {
                        ""
                    } else {
                        "未检测到电池"
                    }
                    .into(),
                );
            }
            Err(error) => {
                println!("[BATTERY] read failed: {error}");
                ui.set_battery_available(false);
                ui.set_battery_status("无法读取 /dev/battery".into());
            }
        }
    }
}

pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let monitor = Rc::new(RefCell::new(BatteryMonitor::new()));

    let active_monitor = monitor.clone();
    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_battery_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} battery", if active { "enter" } else { "exit" });
        active_monitor.borrow_mut().set_active(active);
    });

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(TIMER_INTERVAL_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                monitor.borrow_mut().tick(&ui);
            }
        },
    );
    timer
}
