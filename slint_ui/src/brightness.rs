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
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const BACKLIGHT_DEVICE: &[u8] = b"/dev/backlight\0";

struct BacklightFd(libc::c_int);

impl BacklightFd {
    fn open() -> IoResult<Self> {
        let path = librs::c_str::CStr::from_bytes_with_nul(BACKLIGHT_DEVICE)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            Err(syscall_error(fd))
        } else {
            Ok(Self(fd))
        }
    }

    fn read_brightness(&self) -> IoResult<u8> {
        let mut buf = [0u8; 8];
        let len = match librs::syscall::sys::Sys::read(self.0, &mut buf) {
            Ok(n) => n,
            Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
        };
        if len == 0 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "backlight read empty"));
        }
        let text = core::str::from_utf8(&buf[..len])
            .map_err(|_| Error::new(ErrorKind::InvalidData, "backlight read non-utf8"))?;
        let value: u8 = text
            .trim()
            .parse()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "backlight read non-numeric"))?;
        Ok(value)
    }

    fn write_brightness(&self, value: u8) -> IoResult<()> {
        let text = format!("{}\n", value);
        let mut bytes = text.as_bytes();
        while !bytes.is_empty() {
            match librs::syscall::sys::Sys::write(self.0, bytes) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write backlight",
                    ))
                }
                Ok(n) => bytes = &bytes[n..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }
}

impl Drop for BacklightFd {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

struct BrightnessController {
    fd: BacklightFd,
    last_set: Cell<Option<u8>>,
}

impl BrightnessController {
    fn new() -> IoResult<Self> {
        let fd = BacklightFd::open()?;
        Ok(Self {
            fd,
            last_set: Cell::new(None),
        })
    }

    const MIN_PCT: u8 = 30;

    fn set(&self, value: u8) {
        // value is 30-100, round to nearest 0-255 step so the value round-trips
        let pct = value.max(Self::MIN_PCT);
        if self.last_set.get() == Some(pct) {
            return;
        }
        let hw_value = ((pct as u16 * 255 + 50) / 100) as u8;
        if let Err(error) = self.fd.write_brightness(hw_value) {
            println!("[BACKLIGHT] set failed: {error}");
            self.last_set.set(None);
        } else {
            self.last_set.set(Some(pct));
        }
    }

    // Log the final brightness after a drag or button press ends. Called once
    // per interaction rather than on every intermediate value during dragging.
    fn commit(&self, value: u8) {
        let pct = value.max(Self::MIN_PCT);
        let hw_value = ((pct as u16 * 255 + 50) / 100) as u8;
        println!("[BACKLIGHT] set {} ({}%)", hw_value, pct);
    }

    fn get(&self) -> u8 {
        match self.fd.read_brightness() {
            Ok(hw_value) => {
                // hw_value is 0-255, round to nearest percent
                let pct = ((hw_value as u16 * 100 + 127) / 255) as u8;
                let pct = pct.max(Self::MIN_PCT).min(100);
                println!("[BACKLIGHT] read {} ({}%)", hw_value, pct);
                pct
            }
            Err(error) => {
                println!("[BACKLIGHT] read failed: {error}");
                100
            }
        }
    }
}

fn syscall_error(ret: libc::c_int) -> Error {
    if ret == -1 {
        Error::last_os_error()
    } else {
        Error::from_raw_os_error(-ret)
    }
}

/// Connect the brightness control to the shared launcher window.
pub(crate) fn install(ui: &MainWindow) {
    let controller = match BrightnessController::new() {
        Ok(controller) => Rc::new(RefCell::new(controller)),
        Err(error) => {
            println!("[BACKLIGHT] failed to open backlight device: {error}");
            return;
        }
    };

    let ui_weak = ui.as_weak();
    let callback_controller = controller.clone();
    ui.on_set_brightness(move |value| {
        if let Some(_ui) = ui_weak.upgrade() {
            callback_controller.borrow().set(value as u8);
        }
    });

    let ui_weak = ui.as_weak();
    let commit_controller = controller.clone();
    ui.on_commit_brightness(move |value| {
        if let Some(_ui) = ui_weak.upgrade() {
            commit_controller.borrow().commit(value as u8);
        }
    });

    let ui_weak = ui.as_weak();
    let active_controller = controller.clone();
    ui.on_brightness_page_active_changed(move |active| {
        if active {
            if let Some(ui) = ui_weak.upgrade() {
                let value = active_controller.borrow().get();
                println!("[BACKLIGHT] set ui brightness: {}", value);
                ui.set_brightness(value as i32);
            }
        }
    });
}
