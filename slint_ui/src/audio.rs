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
use std::io::{Error, ErrorKind, Read as _, Result as IoResult, Write};
use std::rc::Rc;

/// TTS PCM audio (2 seconds, 16-bit mono @ 16 kHz), same source as the
/// audio_example app (output.wav).
include!("audio_pcm_short.rs");

/// Where the played PCM comes from. The audio page plays the built-in PCM;
/// the SD card browser starts playback from a WAV file on the card.
pub(crate) enum AudioSource {
    Builtin,
    /// Path to a 16-bit mono 16 kHz WAV on the SD card.
    File { path: String, pcm_len: usize },
}

impl AudioSource {
    fn total_len(&self) -> usize {
        match self {
            AudioSource::Builtin => EXAMPLE_PCM.len(),
            AudioSource::File { pcm_len, .. } => *pcm_len,
        }
    }

    fn label(&self) -> String {
        match self {
            AudioSource::Builtin => "内置 TTS".to_string(),
            AudioSource::File { path, .. } => path.clone(),
        }
    }

    fn clone(&self) -> AudioSource {
        match self {
            AudioSource::Builtin => AudioSource::Builtin,
            AudioSource::File { path, pcm_len } => AudioSource::File {
                path: path.clone(),
                pcm_len: *pcm_len,
            },
        }
    }
}

/// Parse a RIFF WAVE header. Returns the byte length of the PCM data chunk.
/// Only 16-bit mono 16 kHz PCM is accepted, matching the fixed I2S TDM
/// configuration (4 slots × 32-bit, slot0 = mono sample).
pub(crate) fn inspect_wav(path: &str) -> IoResult<usize> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err(Error::new(ErrorKind::InvalidData, "not a RIFF/WAVE file"));
    }
    let mut pcm_len = None;
    loop {
        let mut chunk = [0u8; 8];
        match file.read_exact(&mut chunk) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let id = &chunk[0..4];
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as usize;
        if id == b"fmt " {
            let mut fmt = [0u8; 16];
            if size < 16 {
                return Err(Error::new(ErrorKind::InvalidData, "fmt chunk too small"));
            }
            file.read_exact(&mut fmt)?;
            // Skip the rest of an extended fmt chunk.
            for _ in 16..size {
                let mut byte = [0u8; 1];
                file.read_exact(&mut byte)?;
            }
            let audio_format = u16::from_le_bytes(fmt[0..2].try_into().unwrap());
            let channels = u16::from_le_bytes(fmt[2..4].try_into().unwrap());
            let sample_rate = u32::from_le_bytes(fmt[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes(fmt[14..16].try_into().unwrap());
            if audio_format != 1 {
                return Err(Error::new(ErrorKind::InvalidData, "not PCM (format != 1)"));
            }
            if channels != 1 {
                return Err(Error::new(ErrorKind::InvalidData, "only mono WAV supported"));
            }
            if sample_rate != 16_000 {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("sample rate {sample_rate} != 16000"),
                ));
            }
            if bits != 16 {
                return Err(Error::new(ErrorKind::InvalidData, "only 16-bit WAV supported"));
            }
        } else if id == b"data" {
            pcm_len = Some(size);
            break;
        } else {
            // Skip unknown chunk (round up to even alignment per RIFF spec).
            let skip = size.div_ceil(2) * 2;
            let taken = std::io::copy(&mut (&mut file).take(skip as u64), &mut std::io::sink())?;
            if taken != skip as u64 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "truncated WAV chunk"));
            }
        }
    }
    pcm_len.ok_or_else(|| Error::new(ErrorKind::InvalidData, "no data chunk found"))
}

/// 16kHz, 16-bit, mono → 4-slot TDM, 32-bit left-justified slots.
/// Each mono sample (2 bytes) expands to 16 bytes (4 × 32-bit slots);
/// the sample goes into slot0's high 16 bits, slots 1-3 are zero.
const LJ_CHUNK: usize = 4088; // 511 frames × 8 bytes
const RAW_CHUNK: usize = LJ_CHUNK / 8; // 511 samples × 2 bytes = 1022

struct AudioPlayer {
    offset: usize,
    buf: Vec<u8>,
    /// Handle to /dev/i2s0 while playing.
    file: Option<std::fs::File>,
    /// Handle to the source WAV on the SD card (AudioSource::File only),
    /// already seeked to the start of the data chunk.
    wav_file: Option<std::fs::File>,
    source: AudioSource,
}

impl AudioPlayer {
    fn new() -> Self {
        Self {
            offset: 0,
            buf: vec![0u8; LJ_CHUNK],
            file: None,
            wav_file: None,
            source: AudioSource::Builtin,
        }
    }
}

thread_local! {
    static PLAYER: Rc<RefCell<AudioPlayer>> = Rc::new(RefCell::new(AudioPlayer::new()));
    /// Shared volume controller, installed once by `install()` and used by
    /// both the audio page and the SD card WAV player.
    static VOLUME_CONTROLLER: RefCell<Option<Rc<RefCell<AudioVolumeController>>>> =
        const { RefCell::new(None) };
    /// Unmute timer for the shared start sequence. Must outlive the callback:
    /// a dropped Slint timer cancels its pending callback, which would leave
    /// the DAC muted forever.
    static UNMUTE_TIMER: slint::Timer = slint::Timer::default();
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
        let total = player.source.total_len();

        // Lazily open /dev/i2s0 on first tick.
        if player.file.is_none() {
            match std::fs::OpenOptions::new().write(true).open("/dev/i2s0") {
                Ok(f) => player.file = Some(f),
                Err(e) => {
                    println!("[AUDIO] /dev/i2s0 open failed: {e}");
                    set_status(ui, &player.source, format!("打开 I2S 失败: {}", e));
                    set_playing(ui, &player.source, false);
                    return;
                }
            }
        }

        // Fetch the next raw PCM chunk: slice from the built-in buffer or
        // read from the source WAV file on the SD card.
        let raw_len = RAW_CHUNK.min(total - player.offset) & !1;
        if raw_len == 0 {
            // Playback complete — close /dev/i2s0 so the kernel drains the
            // TX ring and stops the DMA engine (File drop → close → drain_and_stop).
            println!("[AUDIO] playback done ({} bytes)", player.offset);
            set_status(ui, &player.source, "播放完成".to_string());
            set_playing(ui, &player.source, false);
            drop(player.file.take());
            drop(player.wav_file.take());
            return;
        }

        let lj_len = (raw_len / 2) * 16;
        // Copy PCM slice first to avoid borrowing player.buf while pcm borrows player.
        let is_file_source = matches!(player.source, AudioSource::File { .. });
        if player.offset == 0 {
            println!("[AUDIO] tick source={} is_file={}", player.source.label(), is_file_source);
        }
        let pcm_chunk: Vec<u8> = if !is_file_source {
            let pcm = EXAMPLE_PCM.as_slice();
            pcm[player.offset..player.offset + raw_len].to_vec()
        } else if let Some(wav) = player.wav_file.as_mut() {
            let mut chunk = vec![0u8; raw_len];
            if let Err(e) = wav.read_exact(&mut chunk) {
                println!("[AUDIO] WAV read error at {}: {}", player.offset, e);
                set_status(ui, &player.source, format!("读取 WAV 失败: {}", e));
                set_playing(ui, &player.source, false);
                drop(player.file.take());
                drop(player.wav_file.take());
                return;
            }
            chunk
        } else {
            set_status(ui, &player.source, "WAV 文件未打开".to_string());
            set_playing(ui, &player.source, false);
            drop(player.file.take());
            return;
        };
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
                    let pct = player.offset * 100 / total;
                    set_status(ui, &player.source, format!("播放中… {}%", pct));
                }
            }
            Err(e) => {
                println!("[AUDIO] write error at {}: {}", player.offset, e);
                set_status(ui, &player.source, format!("播放错误: {}", e));
                set_playing(ui, &player.source, false);
            }
        }
    });
}

/// Route a playback status update to the UI that owns the current source:
/// the audio page for the built-in PCM, the SD card viewer for WAV files.
fn set_status(ui: &MainWindow, source: &AudioSource, text: String) {
    match source {
        AudioSource::Builtin => ui.set_audio_status(text.into()),
        AudioSource::File { .. } => ui.set_sd_audio_status(text.into()),
    }
}

/// Route the playing flag to the UI that owns the current source.
fn set_playing(ui: &MainWindow, source: &AudioSource, playing: bool) {
    match source {
        AudioSource::Builtin => ui.set_audio_playing(playing),
        AudioSource::File { .. } => ui.set_sd_audio_playing(playing),
    }
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

/// Lowest audible codec value, equal to the previous curve's value at 60%.
const AUDIO_VOLUME_MIN: u32 = 127;
/// Highest codec value exposed by the UI; higher values clip the speaker PA.
const AUDIO_VOLUME_MAX: u32 = 217;

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
    /// 0% is true mute (reg 0). The previous curve's 60% value (reg 127)
    /// becomes the new 1% floor, and 1-100% is linear across 127-217.
    fn set(&self, pct: u8) {
        let pct = pct.min(100);
        if self.last_set.get() == Some(pct) {
            return;
        }
        // 0 = mute; 1..100 -> 127..217, with rounded linear interpolation.
        let hw_value = if pct == 0 {
            0
        } else {
            let offset = (AUDIO_VOLUME_MAX - AUDIO_VOLUME_MIN) * (pct as u32 - 1);
            (AUDIO_VOLUME_MIN + (offset + 49) / 99) as u8
        };
        let Some(fd) = self.fd.as_ref() else {
            return;
        };
        if let Err(error) = fd.write_volume(hw_value) {
            println!("[AUDIO] volume set failed: {error}");
            self.last_set.set(None);
        } else {
            self.last_set.set(Some(pct));
            println!("[AUDIO] volume {pct}%");
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
            println!("[AUDIO] mute={} failed: {error}", mute);
        }
    }

    /// Read the current volume, returned as a 0-100 percentage.
    fn get(&self) -> u8 {
        let Some(fd) = self.fd.as_ref() else {
            return 100;
        };
        match fd.read_volume() {
            Ok(hw_value) => {
                // Inverse of set()'s audible-window linear mapping.
                let pct = if hw_value == 0 {
                    0
                } else if hw_value as u32 <= AUDIO_VOLUME_MIN {
                    1
                } else {
                    let offset = (hw_value as u32 - AUDIO_VOLUME_MIN) * 99;
                    let range = AUDIO_VOLUME_MAX - AUDIO_VOLUME_MIN;
                    (1 + (offset + range / 2) / range).min(100) as u8
                };
                pct
            }
            Err(error) => {
                println!("[AUDIO] volume read failed: {error}");
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
            println!("[AUDIO] volume device open failed: {error}");
            // Use a sentinel empty controller so callbacks stay simple.
            Rc::new(RefCell::new(AudioVolumeController::disabled()))
        }
    };
    VOLUME_CONTROLLER.with(|slot| *slot.borrow_mut() = Some(volume_controller.clone()));

    let vol_ui_weak = ui.as_weak();
    let vol_controller = volume_controller.clone();
    ui.on_set_audio_volume(move |value| {
        if let Some(_ui) = vol_ui_weak.upgrade() {
            vol_controller.borrow().set(value as u8);
        }
    });

    let vol_active_weak = ui.as_weak();
    let active_controller = volume_controller.clone();
    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_audio_page_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} audio", if active { "enter" } else { "exit" });
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
            return;
        }

        // The audio page always plays the built-in PCM; the SD-card audio
        // viewer re-arms whatever WAV the browser last opened (its path is
        // kept in the File source) so the play button does not silently
        // fall back to the built-in TTS.
        if ui.get_sd_audio_open() {
            let path = PLAYER.with(|p| match p.borrow().source.clone() {
                AudioSource::File { path, .. } => Some(path),
                _ => None,
            });
            if let Some(path) = path {
                if let Err(error) = play_file(&ui, &path) {
                    println!("[AUDIO] re-open failed: {error}");
                }
                return;
            }
        }

        PLAYER.with(|p| {
            let mut player = p.borrow_mut();
            *player = AudioPlayer::new();
        });

        start_playback(&ui, &format!("TTS PCM ({} bytes)", EXAMPLE_PCM.len()));
    });

    let stop_controller = volume_controller.clone();
    ui.on_audio_stop(move || {
        let ui = match ui_weak2.upgrade() {
            Some(ui) => ui,
            None => return,
        };
        println!("[AUDIO] stop");

        // Clear the flag of whichever UI owns the current source.
        PLAYER.with(|p| {
            let source = p.borrow().source.clone();
            set_playing(&ui, &source, false);
        });
        ui.set_audio_status("已停止".into());
        // Mute before draining so the stop transition is silent.
        stop_controller.borrow().set_mute(true);
        // Drop the I2S file handle so the kernel drains the TX ring and
        // stops the DMA engine (File drop → close → drain_and_stop).
        PLAYER.with(|p| {
            let mut player = p.borrow_mut();
            drop(player.file.take());
            drop(player.wav_file.take());
        });
    });

    // Install a 10ms timer that drives playback chunks while playing, and
    // polls board keys for volume control while an audio UI is on screen.
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

            // Volume keys (Key2/GPIO9 = down, Key3/GPIO10 = up) apply while the audio
            // page or the SD-card audio viewer is on screen.
            crate::keys::poll();
            let on_audio_ui =
                ui.get_current_app() == 15 || ui.get_sd_audio_open();
            if on_audio_ui {
                let step = if crate::keys::take_key2() {
                    -10
                } else if crate::keys::take_key3() {
                    10
                } else {
                    0
                };
                if step != 0 {
                    let pct = (ui.get_audio_volume() as i32 + step).clamp(0, 100) as u8;
                    ui.set_audio_volume(pct as i32);
                    VOLUME_CONTROLLER.with(|slot| {
                        if let Some(controller) = slot.borrow().as_ref() {
                            controller.borrow().set(pct);
                        }
                    });
                    ui.set_audio_status(format!("音量 {}%", pct).into());
                }
            }

            if ui.get_audio_playing() || ui.get_sd_audio_playing() {
                playback_tick(&ui);
            }
        },
    );

    timer
}

/// Common start sequence for both sources: mute the DAC, mark playing, then
/// unmute once the DMA ring has filled. The caller must have reset PLAYER.
/// Common start sequence for both sources: mute the DAC, mark playing, then
/// unmute once the DMA ring has filled. The caller must have reset PLAYER.
/// The playing flag goes to the UI that owns the current source.
fn start_playback(ui: &MainWindow, label: &str) {
    VOLUME_CONTROLLER.with(|slot| {
        if let Some(controller) = slot.borrow().as_ref() {
            // Mute the DAC before starting DMA so the I2S startup pop is
            // suppressed. The one-shot timer unmutes after the DMA ring has
            // had time to fill and the DAC output has settled.
            controller.borrow().set_mute(true);
        }
    });

    PLAYER.with(|p| {
        let source = p.borrow().source.clone();
        set_playing(ui, &source, true);
    });
    println!("[AUDIO] play: {}", label);

    UNMUTE_TIMER.with(|timer| {
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(30),
            || {
                VOLUME_CONTROLLER.with(|slot| {
                    if let Some(controller) = slot.borrow().as_ref() {
                        controller.borrow().set_mute(false);
                    }
                });
            },
        );
    });
}

/// Seek a freshly opened WAV file to the start of its data chunk.
/// `inspect_wav` already validated the format; this only walks chunks.
fn skip_wav_to_data(file: &mut std::fs::File) -> IoResult<()> {
    use std::io::Read;
    let mut header = [0u8; 12];
    file.read_exact(&mut header)?;
    loop {
        let mut chunk = [0u8; 8];
        file.read_exact(&mut chunk)?;
        let id = &chunk[0..4];
        let size = u32::from_le_bytes(chunk[4..8].try_into().unwrap()) as u64;
        if id == b"data" {
            return Ok(());
        }
        let skip = size.div_ceil(2) * 2;
        let taken = std::io::copy(&mut file.take(skip), &mut std::io::sink())?;
        if taken != skip {
            return Err(Error::new(ErrorKind::UnexpectedEof, "truncated WAV chunk"));
        }
    }
}

/// Entry point for the SD card browser: play a WAV file from the card.
/// Returns Err with a user-readable reason when the file is not a
/// supported WAV (16-bit mono 16 kHz PCM). Status updates go to the SD
/// card viewer (`sd-audio-*` properties), not the audio page.
pub(crate) fn play_file(ui: &MainWindow, path: &str) -> IoResult<()> {
    println!("[AUDIO] play_file path={path}");
    if PLAYER.with(|p| p.borrow().file.is_some()) {
        return Err(Error::new(ErrorKind::Other, "正在播放中"));
    }
    let pcm_len = inspect_wav(path)?;
    let mut wav_file = std::fs::File::open(path)?;
    skip_wav_to_data(&mut wav_file)?;

    PLAYER.with(|p| {
        let mut player = p.borrow_mut();
        *player = AudioPlayer::new();
        player.source = AudioSource::File {
            path: path.to_string(),
            pcm_len,
        };
        player.wav_file = Some(wav_file);
    });

    start_playback(ui, path);
    Ok(())
}
