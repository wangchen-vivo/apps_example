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

use crate::app_window::{MainWindow, SdFileEntry};
use crate::png_view::{self, SharedPngRenderState};
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::io::{Read, Result as IoResult};
use std::path::Path;
use std::rc::Rc;

#[path = "text_pages.rs"]
mod text_pages;

const SD_ROOT: &str = "/data";
const MAX_VISIBLE_ENTRIES: usize = 5;
const TEXT_FILE_LIMIT: usize = 8 * 1024;

struct FileEntryInfo {
    name: String,
    is_dir: bool,
    size: u64,
}

fn read_directory(path: &str) -> IoResult<Vec<FileEntryInfo>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let metadata = entry.metadata()?;
        entries.push(FileEntryInfo {
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
        });
    }
    entries.sort_by(|left, right| {
        right
            .is_dir
            .cmp(&left.is_dir)
            .then_with(|| {
                left.name
                    .bytes()
                    .map(|byte| byte.to_ascii_lowercase())
                    .cmp(right.name.bytes().map(|byte| byte.to_ascii_lowercase()))
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(entries)
}

const AUDIT_MAX_DEPTH: usize = 32;
const AUDIT_MAX_ENTRIES: usize = 512;

#[derive(Default)]
struct AuditSummary {
    directories: usize,
    files: usize,
    txt: usize,
    png: usize,
    other: usize,
    entries: usize,
    truncated: bool,
}

fn audit_file_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some(extension) if extension.eq_ignore_ascii_case("txt") => "TXT",
        Some(extension) if extension.eq_ignore_ascii_case("png") => "PNG",
        _ => "OTHER",
    }
}

fn audit_tree(path: &Path, depth: usize, summary: &mut AuditSummary) -> IoResult<()> {
    if depth > AUDIT_MAX_DEPTH {
        summary.truncated = true;
        return Ok(());
    }
    if summary.entries >= AUDIT_MAX_ENTRIES {
        summary.truncated = true;
        return Ok(());
    }

    for entry in std::fs::read_dir(path)? {
        if summary.entries >= AUDIT_MAX_ENTRIES {
            summary.truncated = true;
            break;
        }

        let entry = entry?;
        let child = entry.path();
        let metadata = entry.metadata()?;
        summary.entries += 1;

        if metadata.is_dir() {
            summary.directories += 1;
            audit_tree(&child, depth + 1, summary)?;
            continue;
        }

        if metadata.is_file() {
            match audit_file_type(&child) {
                "TXT" => summary.txt += 1,
                "PNG" => summary.png += 1,
                _ => summary.other += 1,
            }
            summary.files += 1;
        } else {
            summary.other += 1;
        }
    }
    Ok(())
}

/// Walk a bounded, read-only inventory of the mounted SD card. No per-file
/// logging: the walk happens on every page open and a full listing would
/// flood the serial console.
fn audit_sd_card() -> IoResult<()> {
    let root = Path::new(SD_ROOT);
    let mut summary = AuditSummary::default();
    audit_tree(root, 0, &mut summary)?;
    println!(
        "[SDCARD] scan: dirs={} files={} entries={} truncated={}",
        summary.directories,
        summary.files,
        summary.entries,
        summary.truncated
    );
    Ok(())
}

fn replace_entry_rows(ui: &MainWindow, rows: Vec<SdFileEntry>) {
    let model = ui.get_sd_entries();
    if let Some(model) = model
        .as_any()
        .downcast_ref::<slint::VecModel<SdFileEntry>>()
    {
        let old_count = model.row_count();
        let new_count = rows.len();
        for (index, row) in rows.into_iter().enumerate() {
            if index < old_count {
                if model.row_data(index).as_ref() != Some(&row) {
                    model.set_row_data(index, row);
                }
            } else {
                model.push(row);
            }
        }
        for index in (new_count..old_count).rev() {
            model.remove(index);
        }
    } else {
        ui.set_sd_entries(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
}

fn format_size(size: u64) -> String {
    if size >= 1024 * 1024 {
        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{} B", size)
    }
}

fn extension_is(name: &str, extension: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, value)| value.eq_ignore_ascii_case(extension))
}

fn join_path(directory: &str, name: &str) -> String {
    if directory.ends_with('/') {
        format!("{directory}{name}")
    } else {
        format!("{directory}/{name}")
    }
}

fn read_text_pages(
    path: &str,
    file_size: u64,
) -> IoResult<(text_pages::OnDemandTextPages, bool)> {
    let file = std::fs::File::open(path)?;
    // Allocate capacity up to the file size (capped at TEXT_FILE_LIMIT)
    // without filling it, then read via take.  This avoids a large
    // zero-initialized allocation that fragments the TLSF heap.
    let mut bytes = Vec::with_capacity(
        file_size.min(TEXT_FILE_LIMIT as u64) as usize,
    );
    file.take(TEXT_FILE_LIMIT as u64).read_to_end(&mut bytes)?;
    let truncated = bytes.len() >= TEXT_FILE_LIMIT;
    let contents = match String::from_utf8(bytes) {
        Ok(contents) => contents,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    };
    Ok((text_pages::OnDemandTextPages::new(contents), truncated))
}

struct SdBrowser {
    current_path: String,
    entries: Vec<FileEntryInfo>,
    scroll_offset: usize,
    text_pages: text_pages::OnDemandTextPages,
    text_page: usize,
    text_truncated: bool,
}

impl SdBrowser {
    fn new() -> Self {
        Self {
            current_path: SD_ROOT.to_string(),
            entries: Vec::new(),
            scroll_offset: 0,
            text_pages: text_pages::OnDemandTextPages::default(),
            text_page: 0,
            text_truncated: false,
        }
    }

    fn update_visible_entries(&self, ui: &MainWindow) {
        let total = self.entries.len();
        let visible_end = (self.scroll_offset + MAX_VISIBLE_ENTRIES).min(total);
        println!(
            "[SDCARD] build rows begin offset={} visible={} total={total}",
            self.scroll_offset,
            visible_end - self.scroll_offset
        );
        let rows: Vec<SdFileEntry> = self.entries[self.scroll_offset..visible_end]
            .iter()
            .map(|entry| {
            let is_text = !entry.is_dir && extension_is(&entry.name, "txt");
            let is_image = !entry.is_dir && extension_is(&entry.name, "png");
            let is_audio = !entry.is_dir && extension_is(&entry.name, "wav");
            SdFileEntry {
                name: entry.name.as_str().into(),
                is_dir: entry.is_dir,
                can_open: entry.is_dir || is_text || is_image || is_audio,
                is_image,
                is_audio,
                size_text: format_size(entry.size).into(),
            }
            })
            .collect();
        replace_entry_rows(ui, rows);
        ui.set_sd_total_count(total as i32);
        let per_page = MAX_VISIBLE_ENTRIES;
        let total_pages = (total + per_page - 1) / per_page;
        ui.set_sd_total_pages(total_pages as i32);
        let current_page = self.scroll_offset / per_page + 1;
        ui.set_sd_current_page(current_page as i32);
        ui.set_sd_visible_count((visible_end - self.scroll_offset) as i32);
        ui.set_sd_scroll_offset(self.scroll_offset as i32);
        ui.set_sd_status_text(if total == 0 {
            "空目录".into()
        } else {
            format!("{} 项", total).into()
        });
    }

    fn refresh(&mut self, ui: &MainWindow) {
        ui.set_sd_path_text(self.current_path.as_str().into());
        match read_directory(&self.current_path) {
            Ok(entries) => {
                println!(
                    "[SDCARD] loaded {} entries from {}",
                    entries.len(),
                    self.current_path
                );
                self.entries = entries;
                let total = self.entries.len();
                let max_offset = if total <= MAX_VISIBLE_ENTRIES {
                    0
                } else {
                    (total - 1) / MAX_VISIBLE_ENTRIES * MAX_VISIBLE_ENTRIES
                };
                self.scroll_offset = self.scroll_offset.min(max_offset);
                ui.set_sd_read_error(false);
                self.update_visible_entries(ui);
            }
            Err(error) => {
                println!("[SDCARD] failed to read {}: {error}", self.current_path);
                self.entries.clear();
                self.scroll_offset = 0;
                replace_entry_rows(ui, Vec::new());
                ui.set_sd_total_count(0);
                ui.set_sd_visible_count(0);
                ui.set_sd_scroll_offset(0);
                ui.set_sd_read_error(true);
                ui.set_sd_status_text(format!("读取失败: {error}").into());
            }
        }
    }

    fn select(&mut self, ui: &MainWindow, visible_index: usize) {
        let Some(entry) = self.entries.get(self.scroll_offset + visible_index) else {
            return;
        };
        let name = entry.name.clone();
        let is_dir = entry.is_dir;
        let size = entry.size;
        let path = join_path(&self.current_path, &name);
        if is_dir {
            self.current_path = path;
            self.scroll_offset = 0;
            self.refresh(ui);
        } else if extension_is(&name, "txt") {
            self.open_text_file(ui, &path, &name, size);
        } else if extension_is(&name, "png") {
            self.open_image_file(ui, &path, &name, size);
        } else if extension_is(&name, "wav") {
            self.open_audio_file(ui, &path, &name, size);
        } else {
            ui.set_sd_status_text("只能打开 TXT/PNG/WAV 文件".into());
        }
    }

    /// Open a WAV from the SD card: validate the header, then reuse the
    /// audio page's player (audio.rs) via its play_file entry point. The
    /// browser switches to the audio viewer panel while playing.
    fn open_audio_file(&self, ui: &MainWindow, path: &str, name: &str, size: u64) {
        match crate::audio::play_file(ui, path) {
            Ok(()) => {
                ui.set_sd_audio_title(name.into());
                ui.set_sd_audio_status(format_size(size).into());
                ui.set_sd_audio_open(true);
                println!("[SDCARD] opened WAV {path}");
            }
            Err(error) => {
                println!("[SDCARD] failed to open WAV {path}: {error}");
                ui.set_sd_status_text(format!("WAV 打开失败: {error}").into());
            }
        }
    }

    fn close_audio(&self, ui: &MainWindow) {
        // Stop playback and restore the directory rows. The audio module's
        // stop flow is driven through the shared audio-stop callback so the
        // TX ring drains through the same path as the audio page.
        ui.invoke_audio_stop();
        ui.set_sd_audio_open(false);
        self.update_visible_entries(ui);
    }

    fn open_text_file(&mut self, ui: &MainWindow, path: &str, name: &str, size: u64) {
        // The text viewer is opaque and completely covers the directory list.
        // Release the row model before pagination and first-frame glyph-cache
        // allocations, then rebuild it when the viewer closes.
        replace_entry_rows(ui, Vec::new());
        match read_text_pages(path, size) {
            Ok((pages, truncated)) => {
                self.text_pages = pages;
                self.text_page = 0;
                self.text_truncated = truncated;
                ui.set_sd_text_title(name.into());
                ui.set_sd_text_open(true);
                self.update_text_viewer(ui);
                println!("[SDCARD] opened text file {path}");
            }
            Err(error) => {
                println!("[SDCARD] failed to read {path}: {error}");
                ui.set_sd_status_text(format!("Open failed: {error}").into());
                self.update_visible_entries(ui);
            }
        }
    }

    fn update_text_viewer(&self, ui: &MainWindow) {
        let Some(page) = self.text_pages.get(self.text_page) else {
            return;
        };
        ui.set_sd_text_content(page.as_str().into());
        ui.set_sd_text_page(
            format!(
                "{} / {}{}",
                self.text_page + 1,
                self.text_pages.len(),
                if self.text_truncated { "  LIMIT" } else { "" }
            )
            .into(),
        );
        ui.set_sd_text_has_previous(self.text_page > 0);
        ui.set_sd_text_has_next(self.text_page + 1 < self.text_pages.len());
    }

    fn close_text(&mut self, ui: &MainWindow) {
        ui.set_sd_text_open(false);
        ui.set_sd_text_content("".into());
        self.text_pages.clear();
        self.text_page = 0;
        self.text_truncated = false;
        self.update_visible_entries(ui);
    }

    fn turn_text_page(&mut self, ui: &MainWindow, delta: isize) {
        if self.text_pages.is_empty() {
            return;
        }
        let last = self.text_pages.len() - 1;
        let next = if delta < 0 {
            self.text_page.saturating_sub(delta.unsigned_abs())
        } else {
            (self.text_page + delta as usize).min(last)
        };
        if next != self.text_page {
            self.text_page = next;
            self.update_text_viewer(ui);
        }
    }

    fn open_image_file(&self, ui: &MainWindow, path: &str, name: &str, _size: u64) {
        match png_view::inspect_png(path) {
            Ok((width, height)) => {
                let (display_w, display_h) = png_view::fit_png_dimensions(width, height);
                ui.set_sd_image_title(name.into());
                ui.set_sd_image_status(
                    format!("{width}x{height}").into(),
                );
                // The opaque PNG viewer covers the directory rows. Remove their
                // Slint repeater instances before decoding so fdeflate can use
                // the released heap for its Huffman tables.
                replace_entry_rows(ui, Vec::new());
                ui.set_sd_image_open(true);
                // Store the render request for the background loop to pick up
                crate::PNG_RENDER_STATE.with(|state| {
                    let mut s = state.borrow_mut();
                    s.pending = Some(png_view::PngRenderRequest {
                        path: path.to_string(),
                        source_width: width,
                        source_height: height,
                        display_width: display_w,
                        display_height: display_h,
                    });
                });
                println!("[SDCARD] opened PNG image {path} ({width}x{height})");
            }
            Err(error) => {
                println!("[SDCARD] failed to inspect PNG {path}: {error}");
                ui.set_sd_status_text(format!("PNG 打开失败: {error}").into());
            }
        }
    }

    fn close_image(&self, ui: &MainWindow) {
        ui.set_sd_image_open(false);
        crate::PNG_RENDER_STATE.with(|state| {
            let mut s = state.borrow_mut();
            s.pending = None;
            s.active = false;
            s.log_after_close_frame = true;
        });
        self.update_visible_entries(ui);
    }

    fn go_up(&mut self, ui: &MainWindow) {
        if self.current_path == SD_ROOT {
            return;
        }
        match self.current_path.rfind('/') {
            Some(0) => self.current_path = SD_ROOT.to_string(),
            Some(index) => self.current_path.truncate(index),
            None => self.current_path = SD_ROOT.to_string(),
        }
        self.scroll_offset = 0;
        self.refresh(ui);
    }

    fn scroll(&mut self, ui: &MainWindow, delta: isize) {
        // Page-aligned max offset: the last page may hold fewer than
        // MAX_VISIBLE_ENTRIES, but its start must still be reachable so
        // the page counter can reach the final page.
        let total = self.entries.len();
        let max_offset = if total <= MAX_VISIBLE_ENTRIES {
            0
        } else {
            (total - 1) / MAX_VISIBLE_ENTRIES * MAX_VISIBLE_ENTRIES
        };
        let next = if delta < 0 {
            self.scroll_offset.saturating_sub(delta.unsigned_abs())
        } else {
            (self.scroll_offset + delta as usize).min(max_offset)
        };
        if next != self.scroll_offset {
            self.scroll_offset = next;
            self.update_visible_entries(ui);
        }
    }
}

pub(crate) fn install(ui: &MainWindow, png_state: SharedPngRenderState) {
    ui.set_sd_entries(slint::ModelRc::new(slint::VecModel::default()));
    let browser = Rc::new(RefCell::new(SdBrowser::new()));
    let audit_done = Rc::new(RefCell::new(false));

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    let callback_audit_done = audit_done.clone();
    ui.on_sd_page_active_changed(move |active| {
        if !active {
            // Reset PNG viewer state when leaving the SD card page
            let mut s = png_state.borrow_mut();
            s.pending = None;
            s.active = false;
        }
        if active {
            if let Some(ui) = ui_weak.upgrade() {
                if !*callback_audit_done.borrow() {
                    if let Err(error) = audit_sd_card() {
                        println!("[SDCARD] scan failed: {error}");
                    } else {
                        *callback_audit_done.borrow_mut() = true;
                    }
                }
                callback_browser.borrow_mut().refresh(&ui);
            }
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_refresh_requested(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().refresh(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_entry_selected(move |index| {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().select(&ui, index as usize);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_up_requested(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().go_up(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_scroll_up(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().scroll(&ui, -(MAX_VISIBLE_ENTRIES as isize));
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_scroll_down(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().scroll(&ui, MAX_VISIBLE_ENTRIES as isize);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_close_text(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().close_text(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_previous_text_page(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().turn_text_page(&ui, -1);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_next_text_page(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().turn_text_page(&ui, 1);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_close_image(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().close_image(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let callback_browser = browser.clone();
    ui.on_sd_close_audio(move || {
        if let Some(ui) = ui_weak.upgrade() {
            callback_browser.borrow_mut().close_audio(&ui);
        }
    });
}
