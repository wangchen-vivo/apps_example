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

//! Audio playback page — integrates liuchang's ES8311 audio playback into the Slint UI.
//!
//! Exposes an `install()` function that attaches callbacks for the audio-page.slint
//! component: play/pause, and status display.
//!
//! Playback runs in the Slint event loop via a 10ms timer, writing chunks to
//! `/dev/i2s0`. This avoids threading issues since `MainWindow` is !Send.

use crate::app_window::MainWindow;
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::io::{Error, ErrorKind, Result as IoResult, Write};
use std::rc::Rc;

/// TTS PCM audio (2 seconds, 16-bit mono @ 16 kHz), same source as the
/// audio_example app (output.wav).
include!("audio_pcm_short.rs");

/// 16kHz, 16-bit, mono → 4-slot TDM, 32-bit left-justified slots.
/// Each mono sample (2 bytes) expands to 16 bytes (4 × 32-bit slots);
/// the sample goes into slot0's high 16 bits, slots 1-3 are zero.
const LJ_CHUNK: usize = 4088; // 511 frames × 8 bytes
const RAW_CHUNK: usize = LJ_CHUNK / 8; // 511 samples × 2 bytes = 1022

struct AudioPlayer {
    offset: usize,
    buf: Vec<u8>,
    file: Option<std::fs::File>,
}

thread_local! {
    static PLAYER: Rc<RefCell<AudioPlayer>> = Rc::new(RefCell::new(AudioPlayer {
        offset: 0,
        buf: vec![0u8; LJ_CHUNK],
        file: None,
    }));
}

/// Convert mono PCM (2 bytes/sample) to I2S 4-slot TDM frames, same
/// format as audio_example's play_tts: each 16-bit mono sample is
/// left-justified into slot0's high 16 bits (4 slots × 32-bit = 16
/// bytes per frame), slots 1-3 zero.
fn convert_to_lj(pcm_src: &[u8], buf: &mut [u8], offset: usize, raw_len: usize) {
    let frames = raw_len / 2;
    for f in 0..frames {
        let src = offset + f * 2;
        let dst = f * 16;
        let m = [pcm_src[src], pcm_src[src + 1]];
        // slot 0: mono left-justified into 32-bit slot (high 16 bits)
        buf[dst] = 0x00;
        buf[dst + 1] = 0x00;
        buf[dst + 2] = m[0];
        buf[dst + 3] = m[1];
        // slot 1-3: 0
        buf[dst + 4..dst + 16].fill(0);
    }
}

/// Timer callback — writes one chunk of PCM data to /dev/i2s0.
fn playback_tick(ui: &MainWindow) {
    PLAYER.with(|player_rc| {
        let mut player = player_rc.borrow_mut();
        let pcm = EXAMPLE_PCM.as_slice();

        // Lazily open /dev/i2s0 on first tick.
        if player.file.is_none() {
            match std::fs::OpenOptions::new().write(true).open("/dev/i2s0") {
                Ok(f) => {
                    println!("[AUDIO] /dev/i2s0 opened, starting playback");
                    player.file = Some(f);
                }
                Err(e) => {
                    println!("[AUDIO] Cannot open /dev/i2s0: {}", e);
                    ui.set_audio_status(format!("打开 I2S 失败: {}", e).into());
                    return;
                }
            }
        }

        let raw_len = RAW_CHUNK.min(pcm.len() - player.offset) & !1;
        if raw_len == 0 {
            // Playback complete — close /dev/i2s0 so the kernel drains the
            // TX ring and stops the DMA engine (File drop → close → drain_and_stop).
            println!("[AUDIO] Playback complete ({} bytes)", player.offset);
            ui.set_audio_status("播放完成".into());
            ui.set_audio_playing(false);
            drop(player.file.take());
            return;
        }

        let lj_len = (raw_len / 2) * 16;
        let chunk_index = player.offset / RAW_CHUNK;
        if chunk_index < 3 || chunk_index % 16 == 0 {
            println!(
                "[AUDIO] chunk={} pcm_offset={} raw_len={} i2s_len={}",
                chunk_index, player.offset, raw_len, lj_len
            );
        }
        // Copy PCM slice first to avoid borrowing player.buf while pcm borrows player.
        let pcm_chunk: Vec<u8> = pcm[player.offset..player.offset + raw_len].to_vec();
        convert_to_lj(&pcm_chunk, &mut player.buf, 0, raw_len);

        // Split borrows: take file out, write, then put back.
        let mut file_opt = player.file.take();
        let write_result = match file_opt.as_mut() {
            Some(f) => f.write_all(&player.buf[..lj_len]),
            None => return,
        };
        player.file = file_opt;

        match write_result {
            Ok(()) => {
                player.offset += raw_len;
                // Update status periodically
                if player.offset % (16 * RAW_CHUNK) < RAW_CHUNK {
                    let pct = player.offset * 100 / pcm.len();
                    ui.set_audio_status(format!("播放中… {}%", pct).into());
                }
            }
            Err(e) => {
                println!(
                    "[AUDIO] Write error: chunk={} pcm_offset={} raw_len={} i2s_len={} error={}",
                    chunk_index, player.offset, raw_len, lj_len, e
                );
                ui.set_audio_status(format!("播放错误: {}", e).into());
                ui.set_audio_playing(false);
            }
        }
    });
}

const AUDIO_VOLUME_DEVICE: &[u8] = b"/dev/audio_volume\0";

struct AudioVolumeFd(libc::c_int);

impl AudioVolumeFd {
    fn open() -> IoResult<Self> {
        let path = librs::c_str::CStr::from_bytes_with_nul(AUDIO_VOLUME_DEVICE)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            Err(syscall_error(fd))
        } else {
            Ok(Self(fd))
        }
    }

    fn read_volume(&self) -> IoResult<u8> {
        let mut buf = [0u8; 8];
        let len = match librs::syscall::sys::Sys::read(self.0, &mut buf) {
            Ok(n) => n,
            Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
        };
        if len == 0 {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "audio_volume read empty",
            ));
        }
        let text = core::str::from_utf8(&buf[..len])
            .map_err(|_| Error::new(ErrorKind::InvalidData, "audio_volume read non-utf8"))?;
        let value: u8 = text
            .trim()
            .parse()
            .map_err(|_| Error::new(ErrorKind::InvalidData, "audio_volume read non-numeric"))?;
        Ok(value)
    }

    fn write_volume(&self, value: u8) -> IoResult<()> {
        let text = format!("{}\n", value);
        self.write_bytes(text.as_bytes())
    }

    /// Write a textual command (e.g. "mute\n", "unmute\n") to the
    /// audio_volume device. The kernel parses these case-insensitively.
    fn write_command(&self, cmd: &[u8]) -> IoResult<()> {
        self.write_bytes(cmd)
    }

    fn write_bytes(&self, mut bytes: &[u8]) -> IoResult<()> {
        while !bytes.is_empty() {
            match librs::syscall::sys::Sys::write(self.0, bytes) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write audio_volume",
                    ))
                }
                Ok(n) => bytes = &bytes[n..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }
}

impl Drop for AudioVolumeFd {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

fn syscall_error(ret: libc::c_int) -> Error {
    if ret == -1 {
        Error::last_os_error()
    } else {
        Error::from_raw_os_error(-ret)
    }
}

struct AudioVolumeController {
    fd: Option<AudioVolumeFd>,
    last_set: Cell<Option<u8>>,
}

impl AudioVolumeController {
    fn new() -> IoResult<Self> {
        let fd = AudioVolumeFd::open()?;
        Ok(Self {
            fd: Some(fd),
            last_set: Cell::new(None),
        })
    }

    /// Construct a controller with no device — `set`/`get` become no-ops.
    fn disabled() -> Self {
        Self {
            fd: None,
            last_set: Cell::new(None),
        }
    }

    /// Set the volume from a 0-100 percentage value.
    ///
    /// 100% maps to 217 (0xD9, ~85% of the codec's full scale) — the PA
    /// clips badly above that, so the top of the register range is kept
    /// out of the UI's 100%.
    ///
    /// Measured on this board: register values below 78 (~-10 dB rel full
    /// scale) are inaudible through the speaker. 0% is true mute (reg 0);
    /// 1% lands directly at that audibility floor and 1-100% spreads
    /// quadratically across the audible window so the low end is usable
    /// instead of dying into the threshold.
    fn set(&self, pct: u8) {
        let pct = pct.min(100);
        if self.last_set.get() == Some(pct) {
            return;
        }
        // 0 = mute; 1..100 -> 78 + 139 * ((pct-1)/99)²
        let hw_value = if pct == 0 {
            0
        } else {
            let t = (pct as u32 - 1) * (pct as u32 - 1);
            (78 + (139 * t / (99 * 99))) as u8
        };
        println!("[AUDIO_VOL] set {} ({}%)", hw_value, pct);
        let Some(fd) = self.fd.as_ref() else {
            return;
        };
        if let Err(error) = fd.write_volume(hw_value) {
            println!("[AUDIO_VOL] set failed: {error}");
            self.last_set.set(None);
        } else {
            self.last_set.set(Some(pct));
        }
    }

    /// Soft-mute/unmute the DAC. Used to suppress the I2S startup/stop pop:
    /// mute before DMA starts/stops, unmute after the DMA ring has filled.
    fn set_mute(&self, mute: bool) {
        let Some(fd) = self.fd.as_ref() else {
            return;
        };
        let cmd: &[u8] = if mute { b"mute\n" } else { b"unmute\n" };
        if let Err(error) = fd.write_command(cmd) {
            println!("[AUDIO_VOL] mute={} failed: {error}", mute);
        } else {
            println!("[AUDIO_VOL] mute={}", mute);
        }
    }

    /// Read the current volume, returned as a 0-100 percentage.
    fn get(&self) -> u8 {
        let Some(fd) = self.fd.as_ref() else {
            return 100;
        };
        match fd.read_volume() {
            Ok(hw_value) => {
                // Inverse of set()'s audible-window quadratic:
                // reg 0 -> 0%, 1..78 -> 1%, 79..217 -> 1 + 99·√((reg-78)/139).
                let pct = if hw_value == 0 {
                    0
                } else if hw_value <= 78 {
                    1
                } else {
                    let t = ((hw_value as u32 - 78) * 99 * 99 / 139) as f32;
                    (1 + t.sqrt() as u32).min(100) as u8
                };
                println!("[AUDIO_VOL] read {} ({}%)", hw_value, pct);
                pct
            }
            Err(error) => {
                println!("[AUDIO_VOL] read failed: {error}");
                100
            }
        }
    }
}

pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let ui_weak = ui.as_weak();
    let ui_weak2 = ui.as_weak();

    // Wire up the volume controller. Failure to open the device is non-fatal:
    // playback still works at the codec's init volume, only runtime control is
    // unavailable.
    let volume_controller = match AudioVolumeController::new() {
        Ok(c) => Rc::new(RefCell::new(c)),
        Err(error) => {
            println!("[AUDIO_VOL] failed to open audio_volume device: {error}");
            // Use a sentinel empty controller so callbacks stay simple.
            Rc::new(RefCell::new(AudioVolumeController::disabled()))
        }
    };

    let vol_ui_weak = ui.as_weak();
    let vol_controller = volume_controller.clone();
    ui.on_set_audio_volume(move |value| {
        if let Some(_ui) = vol_ui_weak.upgrade() {
            vol_controller.borrow().set(value as u8);
        }
    });

    let vol_active_weak = ui.as_weak();
    let active_controller = volume_controller.clone();
    ui.on_audio_page_active_changed(move |active| {
        if active {
            if let Some(ui) = vol_active_weak.upgrade() {
                let value = active_controller.borrow().get();
                ui.set_audio_volume(value as i32);
            }
        }
    });

    let play_controller = volume_controller.clone();
    let unmute_controller = volume_controller.clone();
    // One-shot timer reused across playbacks to unmute the DAC after the
    // DMA ring has filled. Owned here and moved into the play callback.
    let unmute_timer = slint::Timer::default();
    let unmute_timer = Rc::new(unmute_timer);
    let unmute_timer_play = unmute_timer.clone();
    ui.on_audio_play(move || {
        let ui = match ui_weak.upgrade() {
            Some(ui) => ui,
            None => return,
        };

        if ui.get_audio_playing() {
            println!("[AUDIO] Already playing, ignoring play request");
            return;
        }

        // Reset player state
        PLAYER.with(|p| {
            let mut player = p.borrow_mut();
            player.offset = 0;
            player.file = None;
        });

        // Mute the DAC before starting DMA so the I2S startup pop is
        // suppressed. The one-shot timer unmutes after the DMA ring has
        // had time to fill and the DAC output has settled.
        play_controller.borrow().set_mute(true);

        ui.set_audio_playing(true);
        println!("[AUDIO] Play started: TTS PCM ({} bytes)", EXAMPLE_PCM.len());

        let unmute_ctrl = unmute_controller.clone();
        unmute_timer_play.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(30),
            move || {
                unmute_ctrl.borrow().set_mute(false);
            },
        );
    });

    let stop_controller = volume_controller.clone();
    ui.on_audio_stop(move || {
        let ui = match ui_weak2.upgrade() {
            Some(ui) => ui,
            None => return,
        };

        ui.set_audio_playing(false);
        ui.set_audio_status("已停止".into());
        // Mute before draining so the stop transition is silent.
        stop_controller.borrow().set_mute(true);
        // Drop the I2S file handle so the kernel drains the TX ring and
        // stops the DMA engine (File drop → close → drain_and_stop).
        PLAYER.with(|p| drop(p.borrow_mut().file.take()));
        println!("[AUDIO] Stopped");
    });

    // Install a 10ms timer that drives playback chunks while playing.
    let timer_ui = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(10),
        move || {
            let ui = match timer_ui.upgrade() {
                Some(ui) => ui,
                None => return,
            };

            if ui.get_audio_playing() {
                playback_tick(&ui);
            }
        },
    );

    timer
}
