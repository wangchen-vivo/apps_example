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

use crate::{app_window::MainWindow, uptime_micros};
use librs::{c_str::CStr, syscall::Syscall};
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    io::{Error, ErrorKind, Result},
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
};

const DEVICE_PATH: &[u8] = b"/dev/esp32-flash0\0";
const ERASE_RANGE_IOCTL: libc::c_ulong = 0x40;
const IOCTL_ABI_VERSION: u32 = 1;
const SECTOR_SIZE: usize = 4096;
const SLOT_COUNT: usize = 2048;
const HEADER_SIZE: usize = 32;
const PAYLOAD_SIZE: usize = SECTOR_SIZE - HEADER_SIZE;
const RECORDS_PER_RUN: usize = 128;
const MAGIC: u32 = 0x4246_494f;
const FORMAT_VERSION: u32 = 1;
const COMMIT_MARKER: u32 = 0x434f_4d4d;
const COMMIT_OFFSET: usize = 24;
/// The writer thread's locals are three 4 KiB buffers plus the journal; 32 KiB
/// gives headroom without stressing the heap-backed thread stacks.
const FLASH_THREAD_STACK_SIZE: usize = 32 * 1024;

// Playback state shared between the writer thread (producer) and the UI
// thread (consumer). MainWindow is !Send, so the writer thread cannot touch
// the UI; it publishes progress here and the UI timer polls these.
// KiB values fit i32; elapsed micros need u64.
static RUN_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECORDS_DONE: AtomicU32 = AtomicU32::new(0);
static SPEED_KBPS: AtomicI32 = AtomicI32::new(0);
static AVERAGE_KBPS: AtomicI32 = AtomicI32::new(0);
static LAST_SLOT: AtomicU32 = AtomicU32::new(0);
static LAST_SEQUENCE: AtomicU32 = AtomicU32::new(0);
// Wall-clock span of the run, published by the writer thread. The average
// speed divides bytes by this span, so the displayed rate matches a stopwatch
// instead of only counting the flash-busy window inside each record.
static RUN_WALL_MICROS: AtomicU64 = AtomicU64::new(0);
static RUN_FAILED: AtomicBool = AtomicBool::new(false);
/// Set when the run-end branch has done its one-shot work (join + verdict),
/// cleared by the next start. Without it the branch re-runs every 30ms tick.
static RUN_END_HANDLED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct EraseRangeRequest {
    version: u32,
    size: u32,
    flags: u32,
    region_offset: u32,
    length: u32,
}

struct FlashDevice(libc::c_int);

impl FlashDevice {
    fn open() -> Result<Self> {
        let path = CStr::from_bytes_with_nul(DEVICE_PATH)
            .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
        let fd = librs::syscall::sys::Sys::open(path, libc::O_RDWR, 0);
        if fd < 0 {
            Err(Error::from_raw_os_error(-fd))
        } else {
            Ok(Self(fd))
        }
    }

    fn seek(&self, offset: usize) -> Result<()> {
        let result = librs::syscall::sys::Sys::lseek(self.0, offset as libc::off_t, libc::SEEK_SET);
        if result < 0 {
            Err(Error::from_raw_os_error(-result as i32))
        } else {
            Ok(())
        }
    }

    fn read_exact_at(&self, offset: usize, mut data: &mut [u8]) -> Result<()> {
        self.seek(offset)?;
        while !data.is_empty() {
            match librs::syscall::sys::Sys::read(self.0, data) {
                Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "short Flash read")),
                Ok(count) => data = &mut data[count..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }

    fn write_all_at(&self, offset: usize, mut data: &[u8]) -> Result<()> {
        self.seek(offset)?;
        while !data.is_empty() {
            match librs::syscall::sys::Sys::write(self.0, data) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "short Flash write")),
                Ok(count) => data = &data[count..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }

    fn erase_slot(&self, slot: usize) -> Result<()> {
        let mut request = EraseRangeRequest {
            version: IOCTL_ABI_VERSION,
            size: core::mem::size_of::<EraseRangeRequest>() as u32,
            flags: 0,
            region_offset: (slot * SECTOR_SIZE) as u32,
            length: SECTOR_SIZE as u32,
        };
        unsafe {
            librs::syscall::sys::Sys::ioctl(
                self.0,
                ERASE_RANGE_IOCTL,
                &mut request as *mut _ as *mut libc::c_void,
            )
            .map(|_| ())
            .map_err(|librs::errno::Errno(errno)| Error::from_raw_os_error(errno))
        }
    }
}

impl Drop for FlashDevice {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

struct FlashJournal {
    used: [bool; SLOT_COUNT],
    next_slot: usize,
    next_sequence: u32,
}

impl FlashJournal {
    fn scan(device: &FlashDevice) -> Result<Self> {
        let mut used = [false; SLOT_COUNT];
        let mut latest: Option<(usize, u32)> = None;
        for slot in 0..SLOT_COUNT {
            let mut header = [0xff; HEADER_SIZE];
            device.read_exact_at(slot * SECTOR_SIZE, &mut header)?;
            used[slot] = header.iter().any(|byte| *byte != 0xff);
            let magic = read_u32(&header, 0);
            let version = read_u32(&header, 4);
            let sequence = read_u32(&header, 8);
            let length = read_u32(&header, 12) as usize;
            let commit = read_u32(&header, COMMIT_OFFSET);
            if magic == MAGIC
                && version == FORMAT_VERSION
                && sequence != 0
                && length <= PAYLOAD_SIZE
                && commit == COMMIT_MARKER
                && latest
                    .map(|(_, current)| sequence.wrapping_sub(current) < 0x8000_0000)
                    .unwrap_or(true)
            {
                latest = Some((slot, sequence));
            }
        }

        let start = latest.map(|(slot, _)| (slot + 1) % SLOT_COUNT).unwrap_or(0);
        let next_slot = (0..SLOT_COUNT)
            .map(|step| (start + step) % SLOT_COUNT)
            .find(|slot| !used[*slot])
            .unwrap_or(start);
        let next_sequence = latest
            .map(|(_, sequence)| sequence.wrapping_add(1).max(1))
            .unwrap_or(1);
        println!(
            "[FLASH] scan complete used_slots={} next_slot={} next_sequence={}",
            used.iter().filter(|is_used| **is_used).count(),
            next_slot,
            next_sequence
        );
        Ok(Self {
            used,
            next_slot,
            next_sequence,
        })
    }

    fn write_next(&mut self, device: &FlashDevice) -> Result<(usize, u32, u128)> {
        let slot = self.next_slot;
        let sequence = self.next_sequence;
        let base = slot * SECTOR_SIZE;

        device.erase_slot(slot).map_err(|error| {
            Error::new(
                error.kind(),
                format!("erase failed: slot={slot} offset=0x{base:08x}: {error}"),
            )
        })?;
        let mut erased = vec![0u8; SECTOR_SIZE];
        device.read_exact_at(base, &mut erased).map_err(|error| {
            Error::new(
                error.kind(),
                format!("erase readback failed: slot={slot} offset=0x{base:08x}: {error}"),
            )
        })?;
        if let Some((index, actual)) = erased
            .iter()
            .copied()
            .enumerate()
            .find(|(_, byte)| *byte != 0xff)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "erase verify failed: slot={slot} offset=0x{:08x} expected=ff actual={actual:02x}",
                    base + index
                ),
            ));
        }

        let mut payload = vec![0u8; PAYLOAD_SIZE];
        fill_payload(&mut payload, sequence);
        let crc = crc32(&payload);
        let mut header = [0xff; HEADER_SIZE];
        write_u32(&mut header, 0, MAGIC);
        write_u32(&mut header, 4, FORMAT_VERSION);
        write_u32(&mut header, 8, sequence);
        write_u32(&mut header, 12, PAYLOAD_SIZE as u32);
        write_u32(&mut header, 16, crc);

        device
            .write_all_at(base, &header[..COMMIT_OFFSET])
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("header write failed: slot={slot}: {error}"),
                )
            })?;
        device
            .write_all_at(base + HEADER_SIZE, &payload)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("payload write failed: slot={slot}: {error}"),
                )
            })?;
        let mut verify = vec![0u8; PAYLOAD_SIZE];
        device
            .read_exact_at(base + HEADER_SIZE, &mut verify)
            .map_err(|error| {
                Error::new(
                    error.kind(),
                    format!("payload readback failed: slot={slot}: {error}"),
                )
            })?;
        let actual_crc = crc32(&verify);
        if let Some((index, (expected, actual))) = payload
            .iter()
            .copied()
            .zip(verify.iter().copied())
            .enumerate()
            .find(|(_, (expected, actual))| expected != actual)
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "payload verify failed: slot={slot} offset=0x{:08x} expected={expected:02x} actual={actual:02x} crc={crc:08x}/{actual_crc:08x}",
                    base + HEADER_SIZE + index
                ),
            ));
        }
        if actual_crc != crc {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "payload CRC failed: slot={slot} expected={crc:08x} actual={actual_crc:08x}"
                ),
            ));
        }

        device.write_all_at(base + COMMIT_OFFSET, &COMMIT_MARKER.to_le_bytes())?;
        let mut committed = [0u8; 4];
        device.read_exact_at(base + COMMIT_OFFSET, &mut committed)?;
        if u32::from_le_bytes(committed) != COMMIT_MARKER {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "commit verify failed: slot={slot} expected={COMMIT_MARKER:08x} actual={:08x}",
                    u32::from_le_bytes(committed)
                ),
            ));
        }


        self.used[slot] = true;
        let search_start = (slot + 1) % SLOT_COUNT;
        self.next_slot = (0..SLOT_COUNT)
            .map(|step| (search_start + step) % SLOT_COUNT)
            .find(|candidate| !self.used[*candidate])
            .unwrap_or(search_start);
        self.next_sequence = sequence.wrapping_add(1).max(1);
        Ok((slot, sequence, 0))
    }
}

/// Writer-thread body: writes RECORDS_PER_RUN records back to back with no
/// pacing, then publishes the final state. Running without tick gaps keeps
/// the wall-clock span equal to the flash work, so the reported average
/// speed is the real throughput. UI updates happen via the atomics above;
/// this thread must never touch MainWindow (it is !Send).
///
/// Exit paths: completing all records, a write error, or RUN_ACTIVE going
/// false (page closed / new run) at the next record boundary. A record is
/// never abandoned mid-way: erase/program/verify/commit always complete.
fn run_write_thread() {
    RUN_FAILED.store(false, Ordering::Relaxed);
    RECORDS_DONE.store(0, Ordering::Relaxed);
    SPEED_KBPS.store(0, Ordering::Relaxed);
    AVERAGE_KBPS.store(0, Ordering::Relaxed);
    RUN_WALL_MICROS.store(0, Ordering::Relaxed);

    let run_start = uptime_micros();
    let device = match FlashDevice::open() {
        Ok(device) => device,
        Err(error) => {
            println!("[FLASH] open failed: {error}");
            RUN_FAILED.store(true, Ordering::Relaxed);
            RUN_ACTIVE.store(false, Ordering::Relaxed);
            return;
        }
    };
    let mut journal = match FlashJournal::scan(&device) {
        Ok(journal) => journal,
        Err(error) => {
            println!("[FLASH] scan failed: {error}");
            RUN_FAILED.store(true, Ordering::Relaxed);
            RUN_ACTIVE.store(false, Ordering::Relaxed);
            return;
        }
    };

    // Live-speed sliding window: the realtime figure is the wall span across
    // the last WINDOW records (so scheduling gaps count and the value settles
    // to a steady number instead of ramping up from the run's average).
    const SPEED_WINDOW: usize = 4;
    let mut window_micros = [0u128; SPEED_WINDOW];
    let mut window_idx = 0usize;
    for _ in 0..RECORDS_PER_RUN {
        if !RUN_ACTIVE.load(Ordering::Relaxed) {
            // A new run replaced this one; stop quietly.
            return;
        }
        match journal.write_next(&device) {
            Ok((slot, sequence, _busy_us)) => {
                let done = RECORDS_DONE.load(Ordering::Relaxed) + 1;
                RECORDS_DONE.store(done, Ordering::Relaxed);
                LAST_SLOT.store(slot as u32, Ordering::Relaxed);
                LAST_SEQUENCE.store(sequence, Ordering::Relaxed);
                // Publish through the last completed record: the wall span
                // includes every scheduling gap, so the average is honest.
                let wall = uptime_micros().saturating_sub(run_start);
                RUN_WALL_MICROS.store(wall as u64, Ordering::Relaxed);
                window_micros[window_idx % SPEED_WINDOW] = wall;
                window_idx += 1;
                // Average speed: total bytes over total wall time.
                let done = done as usize;
                AVERAGE_KBPS.store(throughput_kib(wall, done), Ordering::Relaxed);
                // Live speed: bytes across the sliding window once it is full.
                if window_idx >= SPEED_WINDOW {
                    let oldest = window_micros[window_idx % SPEED_WINDOW];
                    let span = wall.saturating_sub(oldest);
                    SPEED_KBPS.store(throughput_kib(span, SPEED_WINDOW), Ordering::Relaxed);
                } else {
                    SPEED_KBPS.store(throughput_kib(wall, window_idx), Ordering::Relaxed);
                }
            }
            Err(error) => {
                println!("[FLASH] failed: {error}");
                RUN_FAILED.store(true, Ordering::Relaxed);
                RUN_ACTIVE.store(false, Ordering::Relaxed);
                return;
            }
        }
    }
    println!(
        "[FLASH] done records={} wall_us={} avg_kbps={}",
        RECORDS_DONE.load(Ordering::Relaxed),
        RUN_WALL_MICROS.load(Ordering::Relaxed),
        AVERAGE_KBPS.load(Ordering::Relaxed)
    );
    RUN_ACTIVE.store(false, Ordering::Relaxed);
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn fill_payload(payload: &mut [u8], sequence: u32) {
    let mut value = sequence ^ 0xa5a5_5a5a;
    for byte in payload {
        value ^= value << 13;
        value ^= value >> 17;
        value ^= value << 5;
        *byte = value as u8;
    }
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

fn throughput_kib(micros: u128, records: usize) -> i32 {
    if micros == 0 {
        return 0;
    }
    ((records as u128 * SECTOR_SIZE as u128 * 1_000_000) / (micros * 1024)).min(i32::MAX as u128)
        as i32
}

thread_local! {
    /// Handle of the running writer thread. The UI thread holds it so the
    /// run-end / page-exit paths can join the thread.
    static WRITER_THREAD: RefCell<Option<std::thread::JoinHandle<()>>> =
        const { RefCell::new(None) };
}

/// Join the writer thread, releasing its stack. Joinable librs threads block
/// in pthread_exit at a barrier until someone joins them — a run left
/// unjoined leaks its whole 24 KiB thread storage and the next spawn panics
/// on the exhausted heap. join() waits at most one record's flash time
/// (~110 ms) while a run is active (stop signal honored at record
/// boundaries); after a natural finish it returns immediately.
fn join_writer_thread() {
    WRITER_THREAD.with(|slot| {
        if let Some(handle) = slot.borrow_mut().take() {
            let _ = handle.join();
        }
    });
}

/// Stop the writer thread (abort signal at the next record boundary) and
/// reclaim its stack.
fn abort_writer_thread() {
    RUN_ACTIVE.store(false, Ordering::Relaxed);
    join_writer_thread();
}

pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let ui_weak = ui.as_weak();
    ui.on_flash_start_requested(move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        // Refuse to start while a run is in flight: the writer thread owns
        // the flash device, and two writers would corrupt the journal walk.
        if RUN_ACTIVE.load(Ordering::Relaxed) {
            return;
        }
        println!("[FLASH] start");
        RUN_ACTIVE.store(true, Ordering::Relaxed);
        RUN_END_HANDLED.store(false, Ordering::Relaxed);
        ui.set_flash_bytes_written(0);
        ui.set_flash_total_bytes((RECORDS_PER_RUN * (SECTOR_SIZE / 1024)) as i32);
        ui.set_flash_speed_kbps(0);
        ui.set_flash_average_kbps(0);
        ui.set_flash_running(true);
        ui.set_flash_status_text("正在写入 Flash 槽位".into());
        match std::thread::Builder::new()
            .stack_size(FLASH_THREAD_STACK_SIZE)
            .spawn(run_write_thread)
        {
            Ok(handle) => {
                WRITER_THREAD.with(|slot| *slot.borrow_mut() = Some(handle));
            }
            Err(error) => {
                println!("[FLASH] thread spawn failed: {error}");
                RUN_ACTIVE.store(false, Ordering::Relaxed);
                ui.set_flash_running(false);
                ui.set_flash_status_text(format!("线程启动失败: {error}").into());
            }
        }
    });

    // Leaving the page recycles the writer thread: flip RUN_ACTIVE so the
    // thread stops at the next record boundary, then join it here (bounded
    // by one record's flash time). Re-entering shows the aborted state and
    // START becomes available again.
    let ui_weak2 = ui.as_weak();
    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_flash_page_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} flash", if active { "enter" } else { "exit" });
        if !active {
            abort_writer_thread();
            if let Some(ui) = ui_weak2.upgrade() {
                ui.set_flash_running(false);
                ui.set_flash_status_text("已中止".into());
            }
        }
    });

    // UI-side poller: mirrors the writer thread's atomics into the Slint
    // properties. MainWindow is !Send, so all UI mutations stay on this
    // (event-loop) thread. The 500ms cadence keeps the flash bus free for
    // the writer thread: a faster UI refresh steals flash bandwidth and
    // drags the measured write speed down (single core, XIP from flash).
    let timer = slint::Timer::default();
    let ui_weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(500),
        move || {
            let Some(ui) = ui_weak.upgrade() else {
                return;
            };
            let done = RECORDS_DONE.load(Ordering::Relaxed);
            ui.set_flash_bytes_written((done * 4) as i32);
            ui.set_flash_speed_kbps(SPEED_KBPS.load(Ordering::Relaxed));
            ui.set_flash_average_kbps(AVERAGE_KBPS.load(Ordering::Relaxed));
            ui.set_flash_slot(LAST_SLOT.load(Ordering::Relaxed) as i32);
            ui.set_flash_sequence(LAST_SEQUENCE.load(Ordering::Relaxed) as i32);
            if RUN_ACTIVE.load(Ordering::Relaxed) || RUN_END_HANDLED.swap(true, Ordering::Relaxed) {
                return;
            }
            // Run finished (first tick after completion only): reclaim the
            // writer thread's stack right away (a joinable librs thread
            // blocks in pthread_exit until joined; leaving it unjoined leaks
            // its 24 KiB storage and the next spawn panics on the exhausted
            // heap), then show the verdict. RUN_END_HANDLED is cleared by
            // the next start.
            ui.set_flash_running(false);
            join_writer_thread();
            if RUN_FAILED.load(Ordering::Relaxed) {
                ui.set_flash_status_text("Flash 错误，见串口日志".into());
            } else {
                ui.set_flash_status_text("写入并校验通过".into());
            }
        },
    );
    timer
}
