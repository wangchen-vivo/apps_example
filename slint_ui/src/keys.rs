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

//! Board key polling with edge detection.
//!
//! Reads `/dev/keys` (1-byte event bitmap) and exposes edge-detected key
//! events through atomics for the audio and brightness pages' existing
//! timers. Key2 and Key3 are active-low GPIO levels; each consumer sees
//! one event per press.

use librs::syscall::Syscall;
use librs::c_str::CStr;
use std::sync::atomic::{AtomicBool, Ordering};

const KEYS_DEVICE: &[u8] = b"/dev/keys\0";

pub const KEY2_HELD: u8 = 1 << 0;
pub const KEY3_HELD: u8 = 1 << 1;

/// Edge-detected events, consumed by the audio/brightness pages.
/// Cleared by the consumer after handling.
static KEY2_PRESSED: AtomicBool = AtomicBool::new(false);
static KEY3_PRESSED: AtomicBool = AtomicBool::new(false);

struct KeysFd(libc::c_int);

impl KeysFd {
    fn open() -> std::io::Result<Self> {
        let path = CStr::from_bytes_with_nul(KEYS_DEVICE)
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY, 0);
        if fd < 0 {
            Err(std::io::Error::from_raw_os_error(-fd))
        } else {
            Ok(Self(fd))
        }
    }
}

impl Drop for KeysFd {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

thread_local! {
    static KEYS_FD: RefCell<Option<KeysFd>> = const { RefCell::new(None) };
    /// Previous key levels for edge detection.
    static KEY2_LAST_HELD: Cell<bool> = const { Cell::new(false) };
    static KEY3_LAST_HELD: Cell<bool> = const { Cell::new(false) };
    static OPEN_ERROR_LOGGED: Cell<bool> = const { Cell::new(false) };
    static READ_ERROR_LOGGED: Cell<bool> = const { Cell::new(false) };
}

use std::cell::{Cell, RefCell};

/// Poll /dev/keys once. Call from an existing periodic timer. Ignores
/// I2C/read failures (keys just don't update that round).
pub(crate) fn poll() {
    KEYS_FD.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            match KeysFd::open() {
                Ok(fd) => {
                    println!("[KEYS] opened /dev/keys");
                    OPEN_ERROR_LOGGED.set(false);
                    *slot = Some(fd);
                }
                Err(error) => {
                    if !OPEN_ERROR_LOGGED.replace(true) {
                        println!("[KEYS] failed to open /dev/keys: {error}");
                    }
                    return;
                }
            }
        }
        let Some(fd) = slot.as_ref() else { return };
        let mut buf = [0u8; 1];
        let n = match librs::syscall::sys::Sys::read(fd.0, &mut buf) {
            Ok(n) => {
                READ_ERROR_LOGGED.set(false);
                n
            }
            Err(librs::errno::Errno(errno)) => {
                if !READ_ERROR_LOGGED.replace(true) {
                    println!("[KEYS] /dev/keys read failed: errno={errno}");
                }
                return;
            }
        };
        if n < 1 {
            return;
        }
        let events = buf[0];

        let key2_held = events & KEY2_HELD != 0;
        let key2_was_held = KEY2_LAST_HELD.replace(key2_held);
        if key2_held && !key2_was_held {
            KEY2_PRESSED.store(true, Ordering::Relaxed);
        }

        let key3_held = events & KEY3_HELD != 0;
        let key3_was_held = KEY3_LAST_HELD.replace(key3_held);
        if key3_held && !key3_was_held {
            KEY3_PRESSED.store(true, Ordering::Relaxed);
        }
    });
}

/// Take the pending Key2 press event (true at most once per press).
pub(crate) fn take_key2() -> bool {
    KEY2_PRESSED.swap(false, Ordering::Relaxed)
}

/// Take the pending Key3 press event (true at most once per press).
pub(crate) fn take_key3() -> bool {
    KEY3_PRESSED.swap(false, Ordering::Relaxed)
}
