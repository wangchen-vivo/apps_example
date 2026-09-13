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
// Polls /proc/stat, /proc/meminfo, and /proc/0/task/<tid>/status on the UI thread
// via a repeating slint::Timer, following the imu.rs pattern.

use crate::app_window::MainWindow;
use crate::syscall_error;
use librs::c_str::CStr;
use librs::syscall::Syscall;
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const POLL_MS: u64 = 500; // 2 Hz refresh rate
const CORE_COUNT: usize = 1; // ESP32-C6 is single-core RISC-V
const MAX_TASK_LINES: usize = 8;

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

    // State full names for display.
    let state_abbr = match state {
        "running" => "RUNNING",
        "ready" => "READY",
        "suspended" => "SUSPENDED",
        "idle" => "IDLE",
        "retired" => "RETIRED",
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

    // Name column shows the custom name verbatim (empty falls back to kind).
    let typed_name = if name.is_empty() { kind } else { name };

    // TID: show last 4 hex digits
    let tid_str = format!("{:04X}", tid & 0xFFFF);
    let prio_str = format!("{}", priority);

    (
        tid_str,
        type_abbr.to_string(),
        state_abbr.to_string(),
        prio_str,
        typed_name.to_string(),
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

struct TaskEntry {
    tid_disp: String,
    type_abbr: String,
    state_abbr: String,
    prio_str: String,
    typed_name: String,
}

struct SchedMonitor {
    prev_ticks: Vec<CpuTickSnapshot>,
    first_stat: bool,
    first_cpuinfo: bool,
    scroll_offset: usize,
    total_tasks: usize,
}

impl SchedMonitor {
    fn new() -> Self {
        Self {
            prev_ticks: vec![CpuTickSnapshot::default(); CORE_COUNT],
            first_stat: true,
            first_cpuinfo: true,
            scroll_offset: 0,
            total_tasks: 0,
        }
    }

    /// Max valid scroll offset so the last window still fills all rows when
    /// there are enough tasks; when fewer than MAX_TASK_LINES, offset is 0.
    fn max_offset(&self) -> usize {
        self.total_tasks.saturating_sub(MAX_TASK_LINES)
    }

    fn scroll_down(&mut self) {
        // swipe-down → look at tasks above (offset toward 0)
        self.scroll_offset = self.scroll_offset.saturating_sub(1);
    }

    fn scroll_up(&mut self) {
        // swipe-up → look at tasks below (offset toward max)
        if self.scroll_offset < self.max_offset() {
            self.scroll_offset += 1;
        }
    }

    fn refresh_task_list(&mut self, ui: &MainWindow) {
        // List ALL thread TIDs
        let tids = match list_task_entries() {
            Ok(tids) => tids,
            Err(e) => {
                return;
            }
        };

        // Collect status for all threads. Start with zero capacity so the Vec
        // grows incrementally; pre-allocating capacity = tids.len() (≈120
        // threads) reserved ~14 KiB in one shot and tipped the 272 KiB system
        // heap into OOM on this page.
        let mut entries: Vec<TaskEntry> = Vec::new();

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
            "RUNNING" => 0,
            "READY" => 1,
            _ => 2,
        });

        // Take top MAX_TASK_LINES (no dedup — threads with custom names are
        // all distinct anyway, and same-type threads are still worth showing).
        let mut tid_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut type_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut state_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut prio_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);
        let mut name_col: Vec<slint::SharedString> = Vec::with_capacity(MAX_TASK_LINES);

        for entry in entries.iter().skip(self.scroll_offset).take(MAX_TASK_LINES) {
            tid_col.push(entry.tid_disp.clone().into());
            type_col.push(entry.type_abbr.clone().into());
            state_col.push(entry.state_abbr.clone().into());
            prio_col.push(entry.prio_str.clone().into());
            name_col.push(entry.typed_name.clone().into());
        }

        // Fill remaining rows if fewer than MAX_TASK_LINES
        while tid_col.len() < MAX_TASK_LINES {
            tid_col.push("".into());
            type_col.push("".into());
            state_col.push("".into());
            prio_col.push("".into());
            name_col.push("".into());
        }

        // Update rows in-place via existing VecModels. Only changed rows
        // dirty the scene; replacing the whole model every tick forced all
        // 4×5 Text cells to recompute (248 dirty lines, 330ms per frame).
        {
            update_task_model(&ui.get_task_tids(), &tid_col);
            update_task_model(&ui.get_task_types(), &type_col);
            update_task_model(&ui.get_task_states(), &state_col);
            update_task_model(&ui.get_task_prios(), &prio_col);
            update_task_model(&ui.get_task_names(), &name_col);
            ui.set_task_hidden(
                tids.len()
                    .saturating_sub(self.scroll_offset + MAX_TASK_LINES) as i32,
            );
            ui.set_task_total(tids.len() as i32);
            self.total_tasks = tids.len();
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        // Only poll when the sched-mon page (app 6) is active
        if ui.get_current_app() != 6 {
            return;
        }

        // ---- Task list ----
        self.refresh_task_list(ui);

        // ---- CPU usage ----
        if let Ok(stat_content) = read_proc_file(b"/proc/stat\0") {
            let current_ticks = parse_proc_stat(&stat_content);
            if !self.first_stat && current_ticks.len() == self.prev_ticks.len() {
                // Calculate deltas
                let mut cpu_pcts: Vec<f32> = Vec::with_capacity(CORE_COUNT);
                for i in 0..current_ticks.len() {
                    let d_total = current_ticks[i]
                        .system
                        .saturating_sub(self.prev_ticks[i].system);
                    let d_idle = current_ticks[i]
                        .idle
                        .saturating_sub(self.prev_ticks[i].idle);
                    if d_total > 0 {
                        let pct = (d_total - d_idle) as f32 / d_total as f32 * 100.0;
                        cpu_pcts.push(pct.min(100.0));
                    } else {
                        cpu_pcts.push(0.0);
                    }
                }
                // Pad to CORE_COUNT
                while cpu_pcts.len() < CORE_COUNT {
                    cpu_pcts.push(0.0);
                }
                // Only push a new model when the rounded percentage changed;
                // an identical value still dirties the CPU bar every tick.
                let new_pct = cpu_pcts[0];
                let old_pct = ui.get_cpu_usage_percent().row_data(0).unwrap_or(0.0);
                if (new_pct - old_pct).abs() > 0.5 {
                    let model = slint::ModelRc::new(slint::VecModel::from(cpu_pcts));
                    ui.set_cpu_usage_percent(model);
                }
                ui.set_cpu_cores(CORE_COUNT as i32);
            }
            self.prev_ticks = current_ticks;
            self.first_stat = false;
        }

        // ---- Memory usage ----
        if let Ok(mem_content) = read_proc_file(b"/proc/meminfo\0") {
            let (total, used, max_used) = parse_proc_meminfo(&mem_content);
            // Only update when the value changes (to avoid unnecessary redraws)
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
        }

        // ---- CPU info ----
        // Model and ISA are static; the clock frequency comes from the current
        // hardware clock-tree configuration and may change at runtime.
        if let Ok(c) = read_proc_file(b"/proc/cpuinfo\0") {
            let (uarch, isa, mhz_text) = parse_cpuinfo(&c);
            if self.first_cpuinfo {
                ui.set_cpu_model(uarch.into());
                ui.set_cpu_isa(isa.into());
                self.first_cpuinfo = false;
            }
            let cur: slint::SharedString = ui.get_cpu_mhz_text();
            if cur.as_str() != mhz_text {
                ui.set_cpu_mhz_text(mhz_text.into());
            }
        }
    }
}

/// Update a shared task-list column model in place, only touching rows whose
/// value actually changed. Replacing the whole model every tick dirtied all
/// 4×5 Text cells and forced a 330ms partial redraw even when the task list
/// was identical to the previous tick. The caller must seed each column with
/// an empty VecModel at install time so the downcast always succeeds.
fn update_task_model(
    existing: &slint::ModelRc<slint::SharedString>,
    rows: &[slint::SharedString],
) {
    if let Some(model) = existing.as_any().downcast_ref::<slint::VecModel<slint::SharedString>>()
    {
        while model.row_count() < rows.len() {
            model.push(rows[model.row_count()].clone());
        }
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        for (i, row) in rows.iter().enumerate() {
            if model.row_data(i).as_ref() != Some(row) {
                model.set_row_data(i, row.clone());
            }
        }
    }
}

/// Connect the scheduler monitor to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let monitor = Rc::new(RefCell::new(SchedMonitor::new()));

    // Seed empty VecModels so refresh_task_list can update rows in place
    // (see update_task_model) instead of replacing the whole model each tick.
    // Each column needs its own VecModel instance — ModelRc::clone() shares
    // the underlying Rc, so all columns would alias one model and mix data.
    ui.set_task_tids(slint::ModelRc::new(slint::VecModel::<slint::SharedString>::default()));
    ui.set_task_types(slint::ModelRc::new(slint::VecModel::<slint::SharedString>::default()));
    ui.set_task_states(slint::ModelRc::new(slint::VecModel::<slint::SharedString>::default()));
    ui.set_task_prios(slint::ModelRc::new(slint::VecModel::<slint::SharedString>::default()));
    ui.set_task_names(slint::ModelRc::new(slint::VecModel::<slint::SharedString>::default()));

    // Bind the refresh-tasks callback from the Slint UI.
    {
        let monitor = monitor.clone();
        let refresh_ui = ui.as_weak();
        ui.on_refresh_tasks(move || {
            if let Some(ui) = refresh_ui.upgrade() {
                let mut mon = monitor.borrow_mut();
                mon.refresh_task_list(&ui);
            }
        });
    }

    // Task list scroll: swipe-up/down adjust the window offset; the next tick
    // (≤POLL_MS) re-slices the visible rows. No immediate refresh — matches
    // the 2 Hz cadence and avoids a /proc read per swipe.
    {
        let monitor = monitor.clone();
        ui.on_task_scroll_up(move || {
            monitor.borrow_mut().scroll_up();
        });
    }
    {
        let monitor = monitor.clone();
        ui.on_task_scroll_down(move || {
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
    timer
}
