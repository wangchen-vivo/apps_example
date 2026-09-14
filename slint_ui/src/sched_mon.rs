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

// Scheduler and resource monitor backend.
// Polls /proc/stat, /proc/meminfo, and /proc/0/task/<tid>/status on a
// dedicated worker thread so the Slint UI thread never blocks on procfs
// reads. The worker publishes parsed results into shared atomics and a
// mutex-guarded task snapshot; the UI timer copies them into Slint models.

use crate::app_window::MainWindow;
use crate::syscall_error;
use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

const POLL_MS: u64 = 500; // 2 Hz CPU/mem refresh rate
const TASK_POLL_MS: u64 = 2000; // task list refresh rate (5x less churn)
const CORE_COUNT: usize = 1; // ESP32-C6 is single-core RISC-V
const MAX_TASK_LINES: usize = 8;
const WORKER_STACK_SIZE: usize = 8 * 1024;

/// Snapshot of CPU idle/system ticks for computing delta usage.
#[derive(Clone, Copy, Default)]
struct CpuTickSnapshot {
    idle: u64,
    system: u64,
}

/// Parse a single "cpuN  ..." line from /proc/stat.
/// Returns (cpu_id, idle_ticks, total_ticks) or None if not a cpu line.
fn parse_cpu_stat_line(line: &str) -> Option<(usize, u64, u64)> {
    let line = line.trim();
    if !line.starts_with("cpu") {
        return None;
    }
    let rest = line.strip_prefix("cpu")?;
    let (id_str, values) = rest.split_once(' ')?;
    let cpu_id = id_str.parse::<usize>().ok()?;
    let parts: Vec<u64> = values
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    if parts.len() < 5 {
        return None;
    }
    // user, nice, system, idle, iowait, irq, softirq, ...
    let user = parts[0];
    let nice = parts[1];
    let system = parts[2];
    let idle = parts[3];
    let total = user + nice + system + idle + parts.iter().skip(4).sum::<u64>();
    Some((cpu_id, idle, total))
}

/// Parse /proc/stat content and return per-core idle/total tick snapshots.
fn parse_proc_stat(content: &[u8]) -> Vec<CpuTickSnapshot> {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut snaps: Vec<CpuTickSnapshot> = Vec::with_capacity(CORE_COUNT);
    for line in text.lines() {
        if let Some((cpu_id, idle, total)) = parse_cpu_stat_line(line) {
            if cpu_id < CORE_COUNT {
                snaps.push(CpuTickSnapshot {
                    idle,
                    ..Default::default()
                });
                // Store total in the system field (reuse field)
                if let Some(entry) = snaps.last_mut() {
                    entry.system = total;
                }
            }
        }
    }
    snaps
}

/// Parse /proc/meminfo content. Returns (total_kb, used_kb, max_used_kb).
fn parse_proc_meminfo(content: &[u8]) -> (f32, f32, f32) {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut total = 0.0;
    let mut used = 0.0;
    let mut max_used = 0.0;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("MemTotal:") {
            total = parse_kb_value(line);
        } else if line.starts_with("MemUsed:") {
            used = parse_kb_value(line);
        } else if line.starts_with("MemMaxUsed:") {
            max_used = parse_kb_value(line);
        }
    }
    (total, used, max_used)
}

/// Extract the numeric kB value from a line like "MemTotal: 1234 kB".
fn parse_kb_value(line: &str) -> f32 {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse::<f32>().unwrap_or(0.0)
    } else {
        0.0
    }
}

/// Parse /proc/cpuinfo. Returns (uarch, isa, mhz_text) or defaults on failure.
fn parse_cpuinfo(content: &[u8]) -> (String, String, String) {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut uarch = String::from("esp32c6");
    let mut isa = String::from("rv32imac");
    let mut mhz: u32 = 160;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("uarch") {
            if let Some(v) = rest.trim_start().strip_prefix(':') {
                let v = v.trim();
                if !v.is_empty() {
                    uarch = String::from(v);
                }
            }
        } else if let Some(rest) = line.strip_prefix("isa") {
            if let Some(v) = rest.trim_start().strip_prefix(':') {
                let v = v.trim();
                if !v.is_empty() {
                    isa = String::from(v);
                }
            }
        } else if let Some(rest) = line.strip_prefix("cpu MHz") {
            if let Some(v) = rest.trim_start().strip_prefix(':') {
                if let Ok(f) = v.trim().parse::<f32>() {
                    mhz = f as u32;
                }
            }
        }
    }
    (uarch, isa, format!("{} MHz", mhz))
}

/// Parse a thread status file from /proc/<tid>/status.
fn parse_thread_status(content: &[u8], tid: usize) -> (String, String, String, String, String) {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut name = "";
    let mut kind = "normal";
    let mut state = "unknown";
    let mut priority = 0usize;

    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("Name:") {
            name = val.trim();
        } else if let Some(val) = line.strip_prefix("Kind:") {
            kind = val.trim();
        } else if let Some(val) = line.strip_prefix("State:") {
            state = val.trim();
        } else if let Some(val) = line.strip_prefix("Priority:") {
            priority = val.trim().parse::<usize>().unwrap_or(0);
        }
    }

    // State abbreviated to keep the per-frame glyph count low. The software
    // renderer grows its glyph texture array from 256 to 512 entries the
    // moment a frame exceeds 256 glyphs, and that 14336-byte allocation is
    // the OOM point on this page. Short state/name strings keep 8 rows of
    // 5 columns comfortably below 256 glyphs.
    let state_abbr = match state {
        "running" => "RUN",
        "ready" => "RDY",
        "suspended" => "SUSP",
        "idle" => "IDLE",
        "retired" => "RET",
        _ => "?",
    };

    // Type column derives from the thread kind (four categories).
    let type_abbr = match kind {
        "idle" => "idle",
        "normal" => "norm",
        "async_poller" => "poll",
        "soft_timer" => "timer",
        _ => "norm",
    };

    // Name column shows the custom name verbatim (empty falls back to kind),
    // truncated so the longest row still stays under the glyph budget.
    let name_src = if name.is_empty() { kind } else { name };
    let typed_name: String = name_src.chars().take(10).collect();

    // TID: show last 4 hex digits
    let tid_str = format!("{:04X}", tid & 0xFFFF);
    let prio_str = format!("{}", priority);

    (
        tid_str,
        type_abbr.to_string(),
        state_abbr.to_string(),
        prio_str,
        typed_name,
    )
}

/// Read the full content of a file (small, procfs-style).
fn read_proc_file(path: &[u8]) -> IoResult<Vec<u8>> {
    // Use from_bytes_until_nul to tolerate trailing zero bytes in the buffer.
    let c_path =
        CStr::from_bytes_until_nul(path).map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(c_path, libc::O_RDONLY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    let mut buf = [0u8; 1024];
    let n = match librs::syscall::sys::Sys::read(fd, &mut buf) {
        Ok(n) => n,
        Err(librs::errno::Errno(errno)) => {
            let _ = librs::syscall::sys::Sys::close(fd);
            return Err(Error::from_raw_os_error(errno));
        }
    };
    let _ = librs::syscall::sys::Sys::close(fd);
    Ok(buf[..n].to_vec())
}

/// Build a null-terminated /proc/0/task/<tid>/status path.
/// The "0" pid is a placeholder — BlueOS does not have a process concept yet.
/// Path aligns with Linux /proc/<pid>/task/<tid>/status layout.
/// The returned buffer always ends with \0 and fits in 64 bytes.
fn path_for_task_status(tid: usize) -> [u8; 64] {
    let mut buf = [0u8; 64];
    let prefix = b"/proc/0/task/";
    buf[..prefix.len()].copy_from_slice(prefix);
    let mut pos = prefix.len();
    if tid == 0 {
        buf[pos] = b'0';
        pos += 1;
    } else {
        let mut digits = [0u8; 12];
        let mut n = tid;
        let mut nd = 0;
        while n > 0 {
            digits[nd] = b'0' + (n % 10) as u8;
            n /= 10;
            nd += 1;
        }
        for i in 0..nd {
            buf[pos + i] = digits[nd - 1 - i];
        }
        pos += nd;
    }
    buf[pos..pos + 8].copy_from_slice(b"/status\0");
    buf
}

/// List directory entries in /proc/0/task/ (each is a TID directory).
/// The "0" pid is a placeholder — BlueOS does not have a process concept yet.
/// Uses the kernel's dirent layout (which matches libc::dirent64 on 32-bit musl).
fn list_task_entries() -> IoResult<Vec<usize>> {
    let path = CStr::from_bytes_with_nul(b"/proc/0/task\0")
        .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    // Read directory entries using getdents
    let mut buf = [0u8; 512];
    let mut tids = Vec::new();
    loop {
        let n = match librs::syscall::sys::Sys::getdents(fd, &mut buf) {
            Ok(n) => n,
            Err(librs::errno::Errno(errno)) => {
                let _ = librs::syscall::sys::Sys::close(fd);
                return Err(Error::from_raw_os_error(errno));
            }
        };
        if n == 0 {
            break;
        }

        // Kernel dirent layout (RISC-V 32-bit, ino_t=u16, off_t=i32, repr(C) with alignment):
        //   d_ino:     u16   @0   (2 bytes)
        //   [padding]        @2   (2 bytes, align d_off to 4)
        //   d_off:     i32   @4   (4 bytes)
        //   d_reclen:  u16   @8   (2 bytes)
        //   d_type:    u8    @10  (1 byte)
        //   [padding]        @11  (1 byte, align d_namlen to 2)
        //   d_namlen:  u16   @12  (2 bytes)
        //   d_name:    [i8]  @14  (NAME_OFFSET = 14)
        const NAME_OFFSET: usize = 14;

        let mut offset: usize = 0;
        while offset + NAME_OFFSET <= n {
            let reclen =
                u16::from_le_bytes(buf[offset + 8..offset + 10].try_into().unwrap()) as usize;
            if reclen < NAME_OFFSET + 1 || offset + reclen > n {
                break;
            }

            let d_type = buf[offset + 10];
            let namlen =
                u16::from_le_bytes(buf[offset + 12..offset + 14].try_into().unwrap()) as usize;

            let name_len = namlen.min(reclen.saturating_sub(NAME_OFFSET + 1));
            let name_bytes = &buf[offset + NAME_OFFSET..offset + NAME_OFFSET + name_len];

            // Skip "." and ".."
            if d_type == 4 && name_len > 0 && name_bytes[0] != b'.' {
                if let Ok(name) = core::str::from_utf8(name_bytes) {
                    if let Ok(tid) = name.parse::<usize>() {
                        tids.push(tid);
                    }
                }
            }

            offset += reclen;
        }
    }
    let _ = librs::syscall::sys::Sys::close(fd);

    // Sort by TID
    tids.sort_unstable();
    Ok(tids)
}

#[derive(Clone)]
struct TaskEntry {
    tid_disp: String,
    type_abbr: String,
    state_abbr: String,
    prio_str: String,
    typed_name: String,
}

// ---------------------------------------------------------------------------
// Shared worker output. The worker thread writes these; the UI timer reads.
// All values are plain data so the worker never touches Slint/MainWindow.
// ---------------------------------------------------------------------------

/// CPU usage percent for the single core, computed by the worker.
static CPU_PCT: AtomicU32 = AtomicU32::new(0);
/// Memory figures in kB (total / used / max-used), published by the worker.
static MEM_TOTAL_KB: AtomicI32 = AtomicI32::new(0);
static MEM_USED_KB: AtomicI32 = AtomicI32::new(0);
static MEM_MAX_USED_KB: AtomicI32 = AtomicI32::new(0);
/// Static CPU model/ISA and current clock text, published by the worker.
static CPU_MODEL: Mutex<String> = Mutex::new(String::new());
static CPU_ISA: Mutex<String> = Mutex::new(String::new());
static CPU_MHZ: Mutex<String> = Mutex::new(String::new());
/// Publish flags — set once when the worker has valid data.
static CPUINFO_READY: AtomicBool = AtomicBool::new(false);
/// Full task list, sorted by state (running/ready/other), published by the
/// worker. The UI thread locks it briefly to slice the visible window.
static TASK_ENTRIES: Mutex<Vec<TaskEntry>> = Mutex::new(Vec::new());
static TASK_TOTAL: AtomicUsize = AtomicUsize::new(0);
/// Worker state for CPU delta calculation.
static PREV_TICKS: Mutex<Vec<CpuTickSnapshot>> = Mutex::new(Vec::new());
static FIRST_STAT: AtomicBool = AtomicBool::new(true);
/// Set true while the worker thread should keep running. The worker is
/// spawned when the sched-mon page is entered and stopped when it is left,
/// so the 16 KiB thread stack is only held while the page is on screen.
static WORKER_RUNNING: AtomicBool = AtomicBool::new(false);
static WORKER_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Start the background collector (called when the page is entered). The
/// thread is spawned once for the whole process and parked on the gate
/// between visits; subsequent entries just re-open the gate.
fn start_worker() {
    if WORKER_RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    if !WORKER_SPAWNED.swap(true, Ordering::Relaxed) {
        match std::thread::Builder::new()
            .stack_size(WORKER_STACK_SIZE)
            .spawn(worker_loop)
        {
            Ok(_) => {}
            Err(error) => {
                WORKER_SPAWNED.store(false, Ordering::Relaxed);
                WORKER_RUNNING.store(false, Ordering::Relaxed);
                println!("[SCHED_MON] worker spawn failed: {error}");
            }
        }
    }
}

/// Stop the worker (page left): close the gate and let the thread park.
/// The thread and its stack stay alive and resume on the next visit, so no
/// per-visit spawn/teardown heap churn accumulates. Shared buffers keep
/// their capacity and are overwritten on the next entry.
fn stop_worker() {
    if !WORKER_RUNNING.swap(false, Ordering::Relaxed) {
        return;
    }
    TASK_TOTAL.store(0, Ordering::Relaxed);
    CPU_PCT.store(0, Ordering::Relaxed);
    MEM_TOTAL_KB.store(0, Ordering::Relaxed);
    MEM_USED_KB.store(0, Ordering::Relaxed);
    MEM_MAX_USED_KB.store(0, Ordering::Relaxed);
    CPUINFO_READY.store(false, Ordering::Relaxed);
}

/// One full worker pass: read + parse every proc file, publish results.
fn worker_pass() {
    // ---- CPU usage ----
    if let Ok(stat_content) = read_proc_file(b"/proc/stat\0") {
        let current_ticks = parse_proc_stat(&stat_content);
        let mut prev = match PREV_TICKS.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let first = FIRST_STAT.swap(false, Ordering::Relaxed);
        if !first && current_ticks.len() == prev.len() {
            let d_total = current_ticks[0].system.saturating_sub(prev[0].system);
            let d_idle = current_ticks[0].idle.saturating_sub(prev[0].idle);
            let pct = if d_total > 0 {
                ((d_total - d_idle) as f32 / d_total as f32 * 100.0).min(100.0)
            } else {
                0.0
            };
            CPU_PCT.store(pct as u32, Ordering::Relaxed);
        }
        *prev = current_ticks;
    }

    // ---- Memory usage ----
    if let Ok(mem_content) = read_proc_file(b"/proc/meminfo\0") {
        let (total, used, max_used) = parse_proc_meminfo(&mem_content);
        MEM_TOTAL_KB.store(total as i32, Ordering::Relaxed);
        MEM_USED_KB.store(used as i32, Ordering::Relaxed);
        MEM_MAX_USED_KB.store(max_used as i32, Ordering::Relaxed);
    }

    // ---- CPU info (static-ish; only the MHz line changes) ----
    if !CPUINFO_READY.load(Ordering::Relaxed) {
        if let Ok(c) = read_proc_file(b"/proc/cpuinfo\0") {
            let (uarch, isa, mhz_text) = parse_cpuinfo(&c);
            if let Ok(mut m) = CPU_MODEL.lock() {
                *m = uarch;
            }
            if let Ok(mut m) = CPU_ISA.lock() {
                *m = isa;
            }
            if let Ok(mut m) = CPU_MHZ.lock() {
                *m = mhz_text;
            }
            CPUINFO_READY.store(true, Ordering::Relaxed);
        }
    }
}

/// Collect + publish the task list snapshot. Kept separate from the CPU/mem
/// pass so it can run on a slower cadence and reduce heap churn.
fn worker_collect_tasks() {
    let tids = match list_task_entries() {
        Ok(tids) => tids,
        Err(_) => return,
    };
    // Build directly into the shared buffer (clear + push) so the ≈14 KiB
    // snapshot is never duplicated as a local Vec while the shared one is
    // alive. The UI thread slices rows under the same lock, so holding it
    // across the reads is safe and blocks only a shallow window copy.
    let mut entries = match TASK_ENTRIES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    entries.clear();
    for &tid in tids.iter() {
        let path = path_for_task_status(tid);
        let (tid_str, type_abbr, state_abbr, prio_str, typed_name) =
            if let Ok(content) = read_proc_file(&path) {
                parse_thread_status(&content, tid)
            } else {
                (
                    format!("{:04X}", tid & 0xFFFF),
                    "?".into(),
                    "?".into(),
                    "?".into(),
                    "?".into(),
                )
            };
        entries.push(TaskEntry {
            tid_disp: tid_str,
            type_abbr,
            state_abbr,
            prio_str,
            typed_name,
        });
    }

    // Sort: running first, then ready, then others; stable to preserve TID order for ties.
    entries.sort_by_key(|e| match e.state_abbr.as_str() {
        "RUN" => 0,
        "RDY" => 1,
        _ => 2,
    });

    // The shared buffer is already filled and sorted; just update the count.
    drop(entries);
    TASK_TOTAL.store(tids.len(), Ordering::Relaxed);
}

/// Worker entry: thread lives for the whole process lifetime and parks on
/// the WORKER_RUNNING gate between page visits, so no per-visit spawn/drop
/// heap churn accumulates. The inner loop runs one full pass and sleeps; on
/// page leave the gate drops and the thread parks until the next visit.
fn worker_loop() {
    loop {
        while !WORKER_RUNNING.load(Ordering::Relaxed) {
            librs::time::msleep(POLL_MS as libc::c_uint);
        }
        FIRST_STAT.store(true, Ordering::Relaxed);
        PREV_TICKS.lock().map(|mut g| g.clear());
        let mut passes_since_tasks = u32::MAX; // collect on the first pass
        while WORKER_RUNNING.load(Ordering::Relaxed) {
            worker_pass();
            if passes_since_tasks >= (TASK_POLL_MS / POLL_MS) as u32 {
                worker_collect_tasks();
                passes_since_tasks = 0;
            } else {
                passes_since_tasks += 1;
            }
            librs::time::msleep(POLL_MS as libc::c_uint);
        }
    }
}

/// UI-side scroll state. The worker owns all data; this only tracks the
/// window offset and copies the latest snapshot into the Slint models.
struct SchedMonitor {
    /// 0-based page index. Offset for slicing = page_index * MAX_TASK_LINES,
    /// so pages are always aligned: 9 tasks → page 0 shows 1-8, page 1 shows
    /// only the 9th.
    page_index: usize,
    total_tasks: usize,
}

impl SchedMonitor {
    fn new() -> Self {
        Self {
            page_index: 0,
            total_tasks: 0,
        }
    }

    /// 1-based page number shown to the user.
    fn page(&self) -> usize {
        self.page_index + 1
    }

    /// Total page count for the current task total.
    fn page_count(&self) -> usize {
        (self.total_tasks + MAX_TASK_LINES - 1) / MAX_TASK_LINES
    }

    fn scroll_down(&mut self) {
        // swipe-down → previous page
        self.page_index = self.page_index.saturating_sub(1);
    }

    fn scroll_up(&mut self) {
        // swipe-up → next page, clamped to the last page.
        if self.page_index + 1 < self.page_count() {
            self.page_index += 1;
        }
    }

    /// Copy the latest worker-published task snapshot window into the Slint
    /// models, in place. Only changed rows dirty the scene. Each of the 5
    /// columns is formatted as an 8-line string so the page uses 5 Text scene
    /// items total instead of 40 cells.
    fn copy_task_window(&mut self, ui: &MainWindow) {
        self.total_tasks = TASK_TOTAL.load(Ordering::Relaxed);

        let snapshot = match TASK_ENTRIES.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let offset = self.page_index * MAX_TASK_LINES;

        let mut tid_col = String::new();
        let mut type_col = String::new();
        let mut state_col = String::new();
        let mut prio_col = String::new();
        let mut name_col = String::new();
        let mut row = 0;
        for entry in snapshot.iter().skip(offset).take(MAX_TASK_LINES) {
            if row > 0 {
                tid_col.push('\n');
                type_col.push('\n');
                state_col.push('\n');
                prio_col.push('\n');
                name_col.push('\n');
            }
            tid_col.push_str(&entry.tid_disp);
            type_col.push_str(&entry.type_abbr);
            state_col.push_str(&entry.state_abbr);
            prio_col.push_str(&entry.prio_str);
            name_col.push_str(&entry.typed_name);
            row += 1;
        }
        drop(snapshot);

        while row < MAX_TASK_LINES {
            tid_col.push('\n');
            type_col.push('\n');
            state_col.push('\n');
            prio_col.push('\n');
            name_col.push('\n');
            row += 1;
        }

        ui.set_task_col_tid(tid_col.into());
        ui.set_task_col_type(type_col.into());
        ui.set_task_col_state(state_col.into());
        ui.set_task_col_prio(prio_col.into());
        ui.set_task_col_name(name_col.into());
        ui.set_task_hidden(
            self.total_tasks
                .saturating_sub(self.page_index * MAX_TASK_LINES + MAX_TASK_LINES) as i32,
        );
        ui.set_task_total(self.total_tasks as i32);
        ui.set_task_page(self.page() as i32);
        ui.set_task_page_count(self.page_count() as i32);
    }

    /// UI-side tick: copy the worker's latest data into Slint models.
    /// Does zero /proc I/O — the worker thread owns all reads.
    fn tick(&mut self, ui: &MainWindow) {
        // Only touch the models when the sched-mon page (app 6) is active.
        if ui.get_current_app() != 6 {
            return;
        }

        // ---- Task list ----
        self.copy_task_window(ui);

        // ---- CPU usage ----
        let pct = CPU_PCT.load(Ordering::Relaxed) as f32;
        let old_pct = ui.get_cpu_usage_percent().row_data(0).unwrap_or(0.0);
        if (pct - old_pct).abs() > 0.5 {
            let model = slint::ModelRc::new(slint::VecModel::from(vec![pct]));
            ui.set_cpu_usage_percent(model);
        }
        ui.set_cpu_cores(CORE_COUNT as i32);

        // ---- Memory usage ----
        let total = MEM_TOTAL_KB.load(Ordering::Relaxed) as f32;
        let used = MEM_USED_KB.load(Ordering::Relaxed) as f32;
        let max_used = MEM_MAX_USED_KB.load(Ordering::Relaxed) as f32;
        let eps = 0.5;
        if (total - ui.get_mem_total_kb()).abs() > eps {
            ui.set_mem_total_kb(total);
        }
        if (used - ui.get_mem_used_kb()).abs() > eps {
            ui.set_mem_used_kb(used);
        }
        if (max_used - ui.get_mem_max_used_kb()).abs() > eps {
            ui.set_mem_max_used_kb(max_used);
        }

        // ---- CPU info (published once by the worker) ----
        if CPUINFO_READY.load(Ordering::Relaxed) {
            if let Ok(m) = CPU_MODEL.lock() {
                let cur: slint::SharedString = ui.get_cpu_model();
                if cur.as_str() != m.as_str() {
                    ui.set_cpu_model(m.as_str().into());
                }
            }
            if let Ok(m) = CPU_ISA.lock() {
                let cur: slint::SharedString = ui.get_cpu_isa();
                if cur.as_str() != m.as_str() {
                    ui.set_cpu_isa(m.as_str().into());
                }
            }
            if let Ok(m) = CPU_MHZ.lock() {
                let cur: slint::SharedString = ui.get_cpu_mhz_text();
                if cur.as_str() != m.as_str() {
                    ui.set_cpu_mhz_text(m.as_str().into());
                }
            }
        }
    }
}

/// Connect the scheduler monitor to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let monitor = Rc::new(RefCell::new(SchedMonitor::new()));

    // Bind the refresh-tasks callback from the Slint UI. The worker already
    // keeps the snapshot fresh; this just asks the next timer tick to copy
    // the latest window into the models immediately.
    {
        let monitor = monitor.clone();
        let refresh_ui = ui.as_weak();
        ui.on_refresh_tasks(move || {
            println!("[SCHED_MON] refresh");
            if let Some(ui) = refresh_ui.upgrade() {
                let mut mon = monitor.borrow_mut();
                mon.copy_task_window(&ui);
            }
        });
    }

    // Task list scroll: swipe-up/down adjust the window offset; the next tick
    // (≤POLL_MS) re-slices the visible rows. No immediate /proc read — the
    // worker owns the data.
    {
        let monitor = monitor.clone();
        ui.on_task_scroll_up(move || {
            println!("[SCHED_MON] scroll up");
            monitor.borrow_mut().scroll_up();
        });
    }
    {
        let monitor = monitor.clone();
        ui.on_task_scroll_down(move || {
            println!("[SCHED_MON] scroll down");
            monitor.borrow_mut().scroll_down();
        });
    }

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(POLL_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                let mut mon = monitor.borrow_mut();
                mon.tick(&ui);
            }
        },
    );

    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_sched_mon_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        if active {
            start_worker();
        } else {
            stop_worker();
        }
        println!("[PAGE] {} sched-mon", if active { "enter" } else { "exit" });
    });
    timer
}
