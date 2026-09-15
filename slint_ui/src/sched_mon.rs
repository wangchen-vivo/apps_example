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

// Scheduler and resource monitor backend. Slint's UI timer polls /proc/stat,
// /proc/meminfo, and /proc/0/task/<tid>/status directly. This deliberately
// avoids a per-page pthread stack on the constrained device heap.

use crate::app_window::MainWindow;
use crate::syscall_error;
use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

const POLL_MS: u64 = 500; // 2 Hz CPU/mem refresh rate
const TASK_POLL_MS: u64 = 2000; // task list refresh rate (5x less churn)
const CORE_COUNT: usize = 1; // ESP32-C6 is single-core RISC-V
const MAX_TASK_LINES: usize = 8;
/// BlueOS stores 16 bytes including the trailing NUL, so `/proc` can expose
/// at most 15 name bytes. Keep that complete value; the UI adds no shorter cap.
const KERNEL_TASK_NAME_MAX: usize = 15;
const TID_COLUMN_BYTES: usize = MAX_TASK_LINES * (4 + 1) - 1;
const TYPE_COLUMN_BYTES: usize = MAX_TASK_LINES * (5 + 1) - 1;
const STATE_COLUMN_BYTES: usize = MAX_TASK_LINES * (4 + 1) - 1;
const PRIO_COLUMN_BYTES: usize = MAX_TASK_LINES * (3 + 1) - 1;
const NAME_COLUMN_BYTES: usize = MAX_TASK_LINES * (KERNEL_TASK_NAME_MAX + 1) - 1;
/// Upper bound on kernel threads surfaced by /proc/0/task. The board runs
/// ~10 threads; 32 leaves headroom while keeping the snapshot a single
/// static block instead of a growable heap Vec.
const MAX_TASKS: usize = 32;

/// Snapshot of CPU idle/system ticks for computing delta usage.
#[derive(Clone, Copy)]
struct CpuTickSnapshot {
    idle: u64,
    system: u64,
}

impl CpuTickSnapshot {
    const fn zeroed() -> Self {
        Self { idle: 0, system: 0 }
    }
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
    // Pull the tick values with an iterator instead of a temporary Vec.
    let mut vals = values
        .split_whitespace()
        .filter_map(|s| s.parse::<u64>().ok());
    let (user, nice, system, idle) = (vals.next()?, vals.next()?, vals.next()?, vals.next()?);
    let total = user + nice + system + idle + vals.sum::<u64>();
    Some((cpu_id, idle, total))
}

/// Parse /proc/stat content and return per-core idle/total tick snapshots.
fn parse_proc_stat(content: &[u8]) -> [CpuTickSnapshot; CORE_COUNT] {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut snaps = [CpuTickSnapshot::zeroed(); CORE_COUNT];
    for line in text.lines() {
        if let Some((cpu_id, idle, total)) = parse_cpu_stat_line(line) {
            if cpu_id < CORE_COUNT {
                snaps[cpu_id] = CpuTickSnapshot {
                    idle,
                    system: total,
                };
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
    line.split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.0)
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

/// Parse a thread status file from /proc/<tid>/status into a compact entry.
fn parse_thread_status(content: &[u8], tid: usize) -> TaskEntry {
    let text = core::str::from_utf8(content).unwrap_or("");
    let mut name = "";
    let mut kind = "normal";
    let mut state = "unknown";
    let mut priority = 0u32;

    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("Name:") {
            name = val.trim();
        } else if let Some(val) = line.strip_prefix("Kind:") {
            kind = val.trim();
        } else if let Some(val) = line.strip_prefix("State:") {
            state = val.trim();
        } else if let Some(val) = line.strip_prefix("Priority:") {
            priority = val.trim().parse::<u32>().unwrap_or(0);
        }
    }

    // State/kind map to enums; abbreviations are only materialized for the
    // visible rows.
    let task_state = match state {
        "running" => TaskState::Running,
        "ready" => TaskState::Ready,
        "suspended" => TaskState::Suspended,
        "idle" => TaskState::Idle,
        "retired" => TaskState::Retired,
        _ => TaskState::Unknown,
    };
    let task_kind = match kind {
        "idle" => TaskKind::Idle,
        "async_poller" => TaskKind::AsyncPoller,
        "soft_timer" => TaskKind::SoftTimer,
        _ => TaskKind::Normal,
    };

    // Name column shows the complete kernel-visible name (empty falls back
    // to kind). The kernel itself guarantees it is at most 15 bytes.
    let name_src = if name.is_empty() { kind } else { name };
    let mut name_buf = [0u8; KERNEL_TASK_NAME_MAX];
    let mut name_len = 0;
    for (dst, byte) in name_buf
        .iter_mut()
        .zip(name_src.bytes().take(KERNEL_TASK_NAME_MAX))
    {
        *dst = byte;
        name_len += 1;
    }

    TaskEntry {
        tid: (tid & 0xFFFF) as u16,
        priority: priority.min(u8::MAX as u32) as u8,
        kind: task_kind,
        state: task_state,
        name: name_buf,
        name_len,
    }
}

/// Read a small procfs-style file into a fixed stack buffer, returning the
/// buffer and its valid length. No heap allocation on the hot task-list
/// path (the old per-read `to_vec` made ~1 KiB transient allocations for
/// every one of the ~100 tasks every collection).
fn read_proc_file(path: &[u8]) -> IoResult<([u8; 1024], usize)> {
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
    Ok((buf, n))
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
/// Returns a fixed array plus its valid length; no heap allocation.
fn list_task_entries() -> IoResult<([usize; MAX_TASKS], usize)> {
    let path = CStr::from_bytes_with_nul(b"/proc/0/task\0")
        .map_err(|_| Error::from_raw_os_error(libc::EINVAL))?;
    let fd = librs::syscall::sys::Sys::open(path, libc::O_RDONLY | libc::O_DIRECTORY, 0);
    if fd < 0 {
        return Err(syscall_error(fd));
    }

    // Read directory entries using getdents
    let mut buf = [0u8; 512];
    let mut tids = [0usize; MAX_TASKS];
    let mut tids_len = 0;
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
                        if tids_len < MAX_TASKS {
                            tids[tids_len] = tid;
                            tids_len += 1;
                        }
                    }
                }
            }

            offset += reclen;
        }
    }
    let _ = librs::syscall::sys::Sys::close(fd);

    // Sort by TID
    tids[..tids_len].sort_unstable();
    Ok((tids, tids_len))
}

/// Compact per-task snapshot: fixed-size fields instead of five heap
/// Strings, so the ~100-entry task list is one ~2 KiB block instead of a
/// 7.5 KiB Vec plus hundreds of small allocations. Display strings are
/// formatted only for the visible rows in copy_task_window.
#[derive(Clone, Copy)]
struct TaskEntry {
    tid: u16,
    priority: u8,
    kind: TaskKind,
    state: TaskState,
    name: [u8; KERNEL_TASK_NAME_MAX],
    name_len: u8,
}

impl TaskEntry {
    /// Zeroed placeholder used to back the static snapshot array.
    const fn placeholder() -> Self {
        Self {
            tid: 0,
            priority: 0,
            kind: TaskKind::Normal,
            state: TaskState::Unknown,
            name: [0u8; KERNEL_TASK_NAME_MAX],
            name_len: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TaskKind {
    Idle,
    Normal,
    AsyncPoller,
    SoftTimer,
}

impl TaskKind {
    fn abbr(self) -> &'static str {
        match self {
            TaskKind::Idle => "idle",
            TaskKind::Normal => "norm",
            TaskKind::AsyncPoller => "poll",
            TaskKind::SoftTimer => "timer",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum TaskState {
    Running,
    Ready,
    Suspended,
    Idle,
    Retired,
    Unknown,
}

impl TaskState {
    fn abbr(self) -> &'static str {
        match self {
            TaskState::Running => "RUN",
            TaskState::Ready => "RDY",
            TaskState::Suspended => "SUSP",
            TaskState::Idle => "IDLE",
            TaskState::Retired => "RET",
            TaskState::Unknown => "?",
        }
    }
}

// ---------------------------------------------------------------------------
// Timer-published monitor data. All values are plain data; only the Slint
// event-loop thread reads procfs and updates MainWindow.
// ---------------------------------------------------------------------------

/// CPU usage percent for the single core, computed by the timer.
static CPU_PCT: AtomicU32 = AtomicU32::new(0);
/// Memory figures in kB (total / used / max-used), published by the timer.
static MEM_TOTAL_KB: AtomicI32 = AtomicI32::new(0);
static MEM_USED_KB: AtomicI32 = AtomicI32::new(0);
static MEM_MAX_USED_KB: AtomicI32 = AtomicI32::new(0);
/// Static CPU model/ISA and current clock text, published by the timer.
static CPU_MODEL: Mutex<String> = Mutex::new(String::new());
static CPU_ISA: Mutex<String> = Mutex::new(String::new());
static CPU_MHZ: Mutex<String> = Mutex::new(String::new());
/// Publish flags — set once when the timer has valid data.
static CPUINFO_READY: AtomicBool = AtomicBool::new(false);
/// Full task list, sorted by state (running/ready/other), published by the
/// timer. Fixed-size so the snapshot never needs a heap allocation; the
/// valid length lives in TASK_TOTAL. The UI thread locks it briefly to
/// slice the visible window.
static TASK_ENTRIES: Mutex<[TaskEntry; MAX_TASKS]> =
    Mutex::new([TaskEntry::placeholder(); MAX_TASKS]);
static TASK_TOTAL: AtomicUsize = AtomicUsize::new(0);
static TASK_VERSION: AtomicU32 = AtomicU32::new(0);
/// Timer state for CPU delta calculation.
static PREV_TICKS: Mutex<[CpuTickSnapshot; CORE_COUNT]> =
    Mutex::new([CpuTickSnapshot::zeroed(); CORE_COUNT]);
static FIRST_STAT: AtomicBool = AtomicBool::new(true);
fn reset_collector() {
    FIRST_STAT.store(true, Ordering::Relaxed);
    if let Ok(mut ticks) = PREV_TICKS.lock() {
        *ticks = [CpuTickSnapshot::zeroed(); CORE_COUNT];
    }
}

fn clear_collector() {
    TASK_TOTAL.store(0, Ordering::Relaxed);
    CPU_PCT.store(0, Ordering::Relaxed);
    MEM_TOTAL_KB.store(0, Ordering::Relaxed);
    MEM_USED_KB.store(0, Ordering::Relaxed);
    MEM_MAX_USED_KB.store(0, Ordering::Relaxed);
}

/// One timer pass: read + parse CPU, memory, and static CPU info.
fn collect_system_stats() {
    // ---- CPU usage ----
    if let Ok((stat_buf, stat_len)) = read_proc_file(b"/proc/stat\0") {
        let current_ticks = parse_proc_stat(&stat_buf[..stat_len]);
        let mut prev = match PREV_TICKS.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let first = FIRST_STAT.swap(false, Ordering::Relaxed);
        if !first {
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
    if let Ok((mem_buf, mem_len)) = read_proc_file(b"/proc/meminfo\0") {
        let (total, used, max_used) = parse_proc_meminfo(&mem_buf[..mem_len]);
        MEM_TOTAL_KB.store(total as i32, Ordering::Relaxed);
        MEM_USED_KB.store(used as i32, Ordering::Relaxed);
        MEM_MAX_USED_KB.store(max_used as i32, Ordering::Relaxed);
    }

    // ---- CPU info (static-ish; only the MHz line changes) ----
    if !CPUINFO_READY.load(Ordering::Relaxed) {
        if let Ok((cpu_buf, cpu_len)) = read_proc_file(b"/proc/cpuinfo\0") {
            let (uarch, isa, mhz_text) = parse_cpuinfo(&cpu_buf[..cpu_len]);
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
fn collect_tasks() {
    let (tids, tids_len) = match list_task_entries() {
        Ok(result) => result,
        Err(_) => return,
    };
    // Build directly into the shared fixed array so the snapshot never
    // allocates. The UI thread slices rows under the same lock, so holding
    // it across the reads is safe and blocks only a shallow window copy.
    let mut entries = match TASK_ENTRIES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let count = tids_len.min(MAX_TASKS);
    for i in 0..count {
        let tid = tids[i];
        let path = path_for_task_status(tid);
        let entry = if let Ok((status_buf, status_len)) = read_proc_file(&path) {
            parse_thread_status(&status_buf[..status_len], tid)
        } else {
            TaskEntry {
                tid: (tid & 0xFFFF) as u16,
                priority: 0,
                kind: TaskKind::Normal,
                state: TaskState::Unknown,
                name: [0u8; KERNEL_TASK_NAME_MAX],
                name_len: 0,
            }
        };
        entries[i] = entry;
    }

    // Include TID in the key for deterministic ordering while using the
    // allocation-free unstable sorter.
    entries[..count].sort_unstable_by_key(|e| {
        let state_order = match e.state {
            TaskState::Running => 0,
            TaskState::Ready => 1,
            _ => 2,
        };
        (state_order, e.tid)
    });

    // The shared buffer is already filled and sorted; just update the count.
    drop(entries);
    TASK_TOTAL.store(count, Ordering::Relaxed);
    TASK_VERSION.fetch_add(1, Ordering::Release);
}

/// Small stack-backed formatter used for the five visible task columns. It
/// avoids temporary heap Strings; converting to SharedString is the only
/// allocation, and that value is retained directly by Slint.
struct FixedText<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedText<N> {
    const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    fn push_byte(&mut self, byte: u8) {
        if self.len < N {
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn as_str(&self) -> &str {
        // All inputs are validated ASCII task fields or formatting output.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> core::fmt::Write for FixedText<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let remaining = N.saturating_sub(self.len);
        if text.len() > remaining {
            return Err(core::fmt::Error);
        }
        self.bytes[self.len..self.len + text.len()].copy_from_slice(text.as_bytes());
        self.len += text.len();
        Ok(())
    }
}

/// UI-side scroll state. The timer owns all collection and tracks the
/// window offset and copies the latest snapshot into the Slint models.
struct SchedMonitor {
    /// 0-based page index. Offset for slicing = page_index * MAX_TASK_LINES,
    /// so pages are always aligned: 9 tasks → page 0 shows 1-8, page 1 shows
    /// only the 9th.
    page_index: usize,
    total_tasks: usize,
    last_task_version: u32,
    task_dirty: bool,
    task_snapshot_diagnostic_pending: bool,
    polls_since_tasks: u32,
}

impl SchedMonitor {
    fn new() -> Self {
        Self {
            page_index: 0,
            total_tasks: 0,
            last_task_version: u32::MAX,
            task_dirty: true,
            task_snapshot_diagnostic_pending: true,
            polls_since_tasks: u32::MAX,
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
        self.task_dirty = true;
    }

    fn scroll_up(&mut self) {
        // swipe-up → next page, clamped to the last page.
        if self.page_index + 1 < self.page_count() {
            self.page_index += 1;
            self.task_dirty = true;
        }
    }

    /// Copy the latest timer-published task snapshot window into the Slint
    /// models, in place. Only changed rows dirty the scene. Each of the 5
    /// columns is formatted as an 8-line string so the page uses 5 Text scene
    /// items total instead of 40 cells.
    fn copy_task_window(&mut self, ui: &MainWindow) {
        let copied_version = TASK_VERSION.load(Ordering::Acquire);
        self.total_tasks = TASK_TOTAL.load(Ordering::Relaxed);

        let snapshot = match TASK_ENTRIES.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let offset = self.page_index * MAX_TASK_LINES;
        let total = self.total_tasks.min(MAX_TASKS);

        let mut tid_col = FixedText::<TID_COLUMN_BYTES>::new();
        let mut type_col = FixedText::<TYPE_COLUMN_BYTES>::new();
        let mut state_col = FixedText::<STATE_COLUMN_BYTES>::new();
        let mut prio_col = FixedText::<PRIO_COLUMN_BYTES>::new();
        let mut name_col = FixedText::<NAME_COLUMN_BYTES>::new();
        let mut row = 0;
        for entry in snapshot[..total].iter().skip(offset).take(MAX_TASK_LINES) {
            if row > 0 {
                tid_col.push_byte(b'\n');
                type_col.push_byte(b'\n');
                state_col.push_byte(b'\n');
                prio_col.push_byte(b'\n');
                name_col.push_byte(b'\n');
            }
            let _ = write!(tid_col, "{:04X}", entry.tid);
            let _ = type_col.write_str(entry.kind.abbr());
            let _ = state_col.write_str(entry.state.abbr());
            let _ = write!(prio_col, "{}", entry.priority);
            let _ = name_col.write_str(
                core::str::from_utf8(&entry.name[..entry.name_len as usize]).unwrap_or("?"),
            );
            row += 1;
        }
        drop(snapshot);

        while row < MAX_TASK_LINES {
            tid_col.push_byte(b'\n');
            type_col.push_byte(b'\n');
            state_col.push_byte(b'\n');
            prio_col.push_byte(b'\n');
            name_col.push_byte(b'\n');
            row += 1;
        }

        ui.set_task_col_tid(tid_col.as_str().into());
        ui.set_task_col_type(type_col.as_str().into());
        ui.set_task_col_state(state_col.as_str().into());
        ui.set_task_col_prio(prio_col.as_str().into());
        ui.set_task_col_name(name_col.as_str().into());
        ui.set_task_hidden(
            self.total_tasks
                .saturating_sub(self.page_index * MAX_TASK_LINES + MAX_TASK_LINES)
                as i32,
        );
        ui.set_task_total(self.total_tasks as i32);
        ui.set_task_page(self.page() as i32);
        ui.set_task_page_count(self.page_count() as i32);
        if self.task_snapshot_diagnostic_pending && self.total_tasks > 0 {
            self.task_snapshot_diagnostic_pending = false;
            crate::sched_mon_task_snapshot_published();
        }
        self.last_task_version = copied_version;
        self.task_dirty = false;
    }

    /// Timer tick: collect procfs data and copy it into Slint models.
    fn tick(&mut self, ui: &MainWindow) {
        // Only touch the models when the sched-mon page (app 6) is active.
        if ui.get_current_app() != 6 {
            return;
        }

        collect_system_stats();
        if self.polls_since_tasks >= (TASK_POLL_MS / POLL_MS) as u32 {
            collect_tasks();
            self.polls_since_tasks = 0;
        } else {
            self.polls_since_tasks += 1;
        }

        // ---- Task list ----
        let task_version = TASK_VERSION.load(Ordering::Acquire);
        if self.task_dirty || task_version != self.last_task_version {
            self.copy_task_window(ui);
        }

        // ---- CPU usage ----
        let pct = CPU_PCT.load(Ordering::Relaxed) as f32;
        let cpu_model = ui.get_cpu_usage_percent();
        let old_pct = cpu_model.row_data(0).unwrap_or(0.0);
        if (pct - old_pct).abs() > 0.5 {
            if let Some(model) = cpu_model.as_any().downcast_ref::<slint::VecModel<f32>>() {
                model.set_row_data(0, pct);
            } else {
                ui.set_cpu_usage_percent(slint::ModelRc::new(slint::VecModel::from(vec![pct])));
            }
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

        // ---- CPU info (published once by the timer) ----
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

    // Bind the refresh-tasks callback from the Slint UI.
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
    // (≤POLL_MS) re-slices the visible rows.
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
    let timer_monitor = monitor.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(POLL_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                let mut mon = timer_monitor.borrow_mut();
                mon.tick(&ui);
            }
        },
    );

    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    let active_monitor = monitor.clone();
    let active_ui = ui.as_weak();
    ui.on_sched_mon_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        active_monitor.borrow_mut().task_dirty = true;
        if active {
            active_monitor.borrow_mut().task_snapshot_diagnostic_pending = true;
            active_monitor.borrow_mut().polls_since_tasks = u32::MAX;
            reset_collector();
            crate::sched_mon_renderer_entered();
        } else {
            clear_collector();
            crate::sched_mon_renderer_exited();
            if let Some(ui) = active_ui.upgrade() {
                // Drop the five SharedString payloads retained by Slint while
                // this page is hidden. Empty SharedString has no text buffer.
                ui.set_task_col_tid("".into());
                ui.set_task_col_type("".into());
                ui.set_task_col_state("".into());
                ui.set_task_col_prio("".into());
                ui.set_task_col_name("".into());
            }
        }
        println!("[PAGE] {} sched-mon", if active { "enter" } else { "exit" });
    });
    timer
}
