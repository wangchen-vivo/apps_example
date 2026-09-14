// Copyright (c) 2025 vivo Mobile Communication Co., Ltd.
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

use crate::app_window::{MainWindow, WifiNetwork};
use crate::{syscall_error, uptime_millis};
use librs::syscall::Syscall;
use slint::{ComponentHandle, Model};
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Result as IoResult};
use std::rc::Rc;

const SCAN_POLL_ATTEMPTS: usize = 25;
const SCAN_POLL_INTERVAL_MS: u128 = 200;
const INITIAL_SCAN_DELAY_MS: u128 = 400;
const SCAN_BUFFER_SIZE: usize = 2048;
const MAX_VISIBLE_NETWORKS: usize = 6;

#[derive(Debug)]
struct WifiNetworkInfo {
    ssid: String,
    signal_dbm: i8,
    channel: u16,
    security: u8,
}

struct WifiScanResults {
    total_count: usize,
    networks: Vec<WifiNetworkInfo>,
}

struct SocketFd(libc::c_int);

#[derive(Clone, Copy)]
enum WifiScanState {
    Idle,
    Waiting {
        poll_count: usize,
        next_poll_at: u128,
    },
}

struct WifiScanner {
    socket: Option<SocketFd>,
    scan_buffer: Vec<u8>,
    state: WifiScanState,
    scan_requested: bool,
    scan_not_before: u128,
    scan_started_at: Option<u128>,
    page_active: bool,
    results: Vec<WifiNetworkInfo>,
    scroll_offset: usize,
    total_count: usize,
}

impl SocketFd {
    fn open_for_wifi_scan() -> IoResult<Self> {
        let fd =
            librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 {
            Err(syscall_error(fd))
        } else {
            Ok(Self(fd))
        }
    }
}

impl Drop for SocketFd {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.0);
    }
}

fn security_name(security: u8) -> &'static str {
    match security {
        0 => "Open",
        1 => "WEP",
        2 => "WPA",
        3 => "WPA2",
        4 => "WPA3",
        _ => "Unknown",
    }
}

fn signal_strength(signal_dbm: i8) -> i32 {
    match signal_dbm {
        -50..=i8::MAX => 4,
        -65..=-51 => 3,
        -75..=-66 => 2,
        _ => 1,
    }
}

fn wlan0_name() -> [libc::c_char; 16] {
    let mut name = [0 as libc::c_char; 16];
    for (dst, src) in name.iter_mut().zip(b"wlan0\0") {
        *dst = *src as libc::c_char;
    }
    name
}

fn trigger_wifi_scan(fd: libc::c_int) -> IoResult<()> {
    let scan_req = libc::iw_scan_req {
        scan_type: libc::IW_SCAN_TYPE_ACTIVE as u8,
        essid_len: 0,
        num_channels: 0,
        flags: 0,
        bssid: unsafe { core::mem::zeroed() },
        essid: [0u8; libc::IW_ESSID_MAX_SIZE],
        min_channel_time: 0,
        max_channel_time: 0,
        channel_list: [libc::iw_freq {
            m: 0,
            e: 0,
            i: 0,
            flags: 0,
        }; libc::IW_MAX_FREQUENCIES],
    };
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data {
            essid: libc::iw_point {
                pointer: &scan_req as *const libc::iw_scan_req as *mut libc::c_void,
                length: core::mem::size_of::<libc::iw_scan_req>() as u16,
                flags: 0,
            },
        },
    };

    unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWSCAN,
            &mut iwreq as *mut _ as *mut libc::c_void,
        )
        .map(|_| ())
        .map_err(|librs::errno::Errno(errno)| Error::from_raw_os_error(errno))
    }
}

fn poll_wifi_scan(fd: libc::c_int, buffer: &mut Vec<u8>) -> IoResult<Option<usize>> {
    buffer.resize(SCAN_BUFFER_SIZE, 0);
    let data = libc::iw_point {
        pointer: buffer.as_mut_ptr() as *mut libc::c_void,
        length: buffer.len() as u16,
        flags: 0,
    };
    let mut iwreq = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { data },
    };

    let result = unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCGIWSCAN,
            &mut iwreq as *mut _ as *mut libc::c_void,
        )
    };
    match result {
        Ok(size) if size >= 0 => {
            let size = size as usize;
            if size > buffer.len() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "wireless scan result exceeds buffer",
                ));
            }
            Ok(Some(size))
        }
        Err(librs::errno::Errno(errno)) if errno == libc::EAGAIN => Ok(None),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
        _ => Err(Error::new(
            ErrorKind::InvalidData,
            "wireless scan returned an invalid size",
        )),
    }
}

fn take_bytes<'a>(buffer: &'a [u8], offset: &mut usize, len: usize) -> IoResult<&'a [u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "wireless scan result overflow"))?;
    let bytes = buffer
        .get(*offset..end)
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "truncated wireless scan result"))?;
    *offset = end;
    Ok(bytes)
}

fn decode_wifi_scan(buffer: &[u8]) -> IoResult<WifiScanResults> {
    let mut offset = 0;
    let count = take_bytes(buffer, &mut offset, 4)?;
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    let mut networks: Vec<WifiNetworkInfo> = Vec::with_capacity(count);

    for _ in 0..count {
        let ssid_len = take_bytes(buffer, &mut offset, 4)?;
        let ssid_len = u32::from_le_bytes(ssid_len.try_into().unwrap()) as usize;
        if ssid_len > libc::IW_ESSID_MAX_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "wireless scan returned an invalid SSID length",
            ));
        }
        let ssid = take_bytes(buffer, &mut offset, ssid_len)?;
        let _bssid = take_bytes(buffer, &mut offset, 6)?;
        let signal_dbm = take_bytes(buffer, &mut offset, 1)?[0] as i8;
        let channel = take_bytes(buffer, &mut offset, 2)?;
        let channel = u16::from_le_bytes(channel.try_into().unwrap());
        let security = take_bytes(buffer, &mut offset, 1)?[0];

        let network = WifiNetworkInfo {
            ssid: if ssid.is_empty() {
                String::from("<hidden network>")
            } else {
                String::from_utf8_lossy(ssid).into_owned()
            },
            signal_dbm,
            channel,
            security,
        };
        networks.push(network);
    }

    networks.sort_by(|left, right| right.signal_dbm.cmp(&left.signal_dbm));
    Ok(WifiScanResults {
        total_count: count,
        networks,
    })
}

fn to_slint_network(network: &WifiNetworkInfo) -> WifiNetwork {
    WifiNetwork {
        ssid: network.ssid.as_str().into(),
        detail: format!(
            "{}  /  CHANNEL {}",
            security_name(network.security),
            network.channel
        )
        .into(),
        signal_text: format!("{} dBm", network.signal_dbm).into(),
        strength: signal_strength(network.signal_dbm),
        secure: network.security != 0,
    }
}
fn replace_network_rows(ui: &MainWindow, rows: Vec<WifiNetwork>) {
    let model = ui.get_networks();
    if let Some(model) = model
        .as_any()
        .downcast_ref::<slint::VecModel<WifiNetwork>>()
    {
        // Update rows individually so Slint keeps the existing repeater instances
        // and their rendering state. A model reset destroys and recreates all row
        // item trees, which needs a large contiguous allocation on every scan.
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
        ui.set_networks(slint::ModelRc::new(slint::VecModel::from(rows)));
    }
}

fn show_scan_result(ui: &MainWindow, result: IoResult<WifiScanResults>) {
    ui.set_scanning(false);
    match result {
        Ok(results) => {
            let visible: Vec<WifiNetwork> = results.networks.iter().map(to_slint_network).collect();
            replace_network_rows(ui, visible);
            ui.set_result_count(results.total_count as i32);
            if results.total_count == 0 {
                ui.set_status_text("扫描完成，未发现接入点".into());
            } else if results.total_count > MAX_VISIBLE_NETWORKS {
                ui.set_status_text(
                    format!(
                        "发现 {} 个网络，显示最强的 {} 个",
                        results.total_count, MAX_VISIBLE_NETWORKS
                    )
                    .into(),
                );
            } else {
                ui.set_status_text(format!("发现 {} 个附近网络", results.total_count).into());
            }
        }
        Err(error) => {
            replace_network_rows(ui, Vec::new());
            ui.set_result_count(0);
            ui.set_status_text(format!("扫描失败: {error}").into());
        }
    }
}

impl WifiScanner {
    fn new() -> Self {
        Self {
            socket: None,
            scan_buffer: vec![0u8; SCAN_BUFFER_SIZE],
            state: WifiScanState::Idle,
            scan_requested: false,
            scan_not_before: 0,
            scan_started_at: None,
            page_active: false,
            results: Vec::new(),
            scroll_offset: 0,
            total_count: 0,
        }
    }

    fn show_page(&self, ui: &MainWindow) {
        let page: Vec<WifiNetwork> = self.results[self.scroll_offset..]
            .iter()
            .take(MAX_VISIBLE_NETWORKS)
            .map(to_slint_network)
            .collect();
        replace_network_rows(ui, page);
        ui.set_result_count(self.total_count as i32);
        let per_page = MAX_VISIBLE_NETWORKS;
        let total_pages = (self.results.len() + per_page - 1) / per_page;
        ui.set_wifi_total_pages(total_pages as i32);
        let current_page = self.scroll_offset / per_page + 1;
        ui.set_wifi_current_page(current_page as i32);
    }

    fn can_scroll_down(&self) -> bool {
        self.scroll_offset > 0
    }

    fn can_scroll_up(&self) -> bool {
        self.scroll_offset + MAX_VISIBLE_NETWORKS < self.results.len()
    }

    fn scroll_up(&mut self, ui: &MainWindow) {
        if self.results.is_empty() {
            return;
        }
        let new_offset = self.scroll_offset + MAX_VISIBLE_NETWORKS;
        if new_offset < self.results.len() {
            self.scroll_offset = new_offset;
            self.show_page(ui);
        }
    }

    fn scroll_down(&mut self, ui: &MainWindow) {
        if self.results.is_empty() {
            return;
        }
        let new_offset = self.scroll_offset.saturating_sub(MAX_VISIBLE_NETWORKS);
        if new_offset != self.scroll_offset {
            self.scroll_offset = new_offset;
            self.show_page(ui);
        }
    }

    fn set_page_active(&mut self, ui: &MainWindow, active: bool) {
        self.page_active = active;
        if active {
            self.scroll_offset = 0;
            if !matches!(self.state, WifiScanState::Waiting { .. }) && !self.scan_requested {
                self.scan_requested = true;
                self.scan_not_before = uptime_millis().saturating_add(INITIAL_SCAN_DELAY_MS);
                ui.set_scanning(true);
                ui.set_status_text("正在准备无线扫描".into());
            }
        } else if matches!(self.state, WifiScanState::Idle) {
            self.scan_requested = false;
            ui.set_scanning(false);
        }
    }

    fn request_scan(&mut self, ui: &MainWindow) {
        if matches!(self.state, WifiScanState::Waiting { .. }) || self.scan_requested {
            return;
        }

        self.scan_requested = true;
        self.scan_not_before = uptime_millis();
        ui.set_scanning(true);
        ui.set_status_text("请求扫描".into());
    }

    fn finish_scan(&mut self, ui: &MainWindow, result: IoResult<WifiScanResults>) {
        self.state = WifiScanState::Idle;
        if let Some(started_at) = self.scan_started_at.take() {
            println!(
                "[WIFI_SCAN] completed elapsed_ms={}",
                uptime_millis().saturating_sub(started_at)
            );
        }
        match result {
            Ok(results) => {
                self.results = results.networks;
                self.total_count = results.total_count;
                self.scroll_offset = 0;
                self.show_page(ui);
                if results.total_count == 0 {
                    ui.set_status_text("扫描完成，未发现接入点".into());
                } else {
                    ui.set_status_text(format!("发现 {} 个附近网络", results.total_count).into());
                }
            }
            Err(error) => {
                self.results.clear();
                self.total_count = 0;
                replace_network_rows(ui, Vec::new());
                ui.set_result_count(0);
                ui.set_status_text(format!("扫描失败: {error}").into());
            }
        }
        ui.set_scanning(false);
    }

    fn start_scan(&mut self, ui: &MainWindow, now: u128) {
        self.scan_requested = false;
        self.scan_started_at = Some(now);
        self.scroll_offset = 0;
        ui.set_scanning(true);
        ui.set_status_text("扫描信道 1 至 13".into());
        println!("[WIFI_SCAN] started");

        if self.socket.is_none() {
            match SocketFd::open_for_wifi_scan() {
                Ok(socket) => self.socket = Some(socket),
                Err(error) => {
                    self.finish_scan(ui, Err(error));
                    return;
                }
            }
        }

        match trigger_wifi_scan(self.socket.as_ref().unwrap().0) {
            Ok(()) => {
                self.state = WifiScanState::Waiting {
                    poll_count: 0,
                    next_poll_at: now.saturating_add(SCAN_POLL_INTERVAL_MS),
                };
            }
            Err(error) => self.finish_scan(ui, Err(error)),
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        let now = uptime_millis();
        match self.state {
            WifiScanState::Idle => {
                if !self.page_active || !self.scan_requested || now < self.scan_not_before {
                    return;
                }
                self.start_scan(ui, now);
            }
            WifiScanState::Waiting {
                poll_count,
                next_poll_at,
            } => {
                if now < next_poll_at {
                    return;
                }

                let fd = self.socket.as_ref().unwrap().0;
                match poll_wifi_scan(fd, &mut self.scan_buffer) {
                    Ok(Some(result_size)) => {
                        let result = decode_wifi_scan(&self.scan_buffer[..result_size]);
                        self.finish_scan(ui, result);
                    }
                    Ok(None) if poll_count + 1 < SCAN_POLL_ATTEMPTS => {
                        self.state = WifiScanState::Waiting {
                            poll_count: poll_count + 1,
                            next_poll_at: now.saturating_add(SCAN_POLL_INTERVAL_MS),
                        };
                    }
                    Ok(None) => self.finish_scan(
                        ui,
                        Err(Error::new(ErrorKind::TimedOut, "wireless scan timed out")),
                    ),
                    Err(error) => self.finish_scan(ui, Err(error)),
                }
            }
        }
    }
}

/// Connect the Wi-Fi scanner to the shared launcher window. The returned
/// timer must remain alive for as long as the Slint event loop is running.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    ui.set_networks(slint::ModelRc::new(slint::VecModel::default()));

    let scanner = Rc::new(RefCell::new(WifiScanner::new()));
    let ui_weak = ui.as_weak();
    let callback_scanner = scanner.clone();
    ui.on_scan_requested(move || {
        println!("[WIFI_SCAN] scan requested");
        if let Some(ui) = ui_weak.upgrade() {
            callback_scanner.borrow_mut().request_scan(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let active_scanner = scanner.clone();
    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_wifi_page_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} wifi", if active { "enter" } else { "exit" });
        if let Some(ui) = ui_weak.upgrade() {
            active_scanner.borrow_mut().set_page_active(&ui, active);
        }
    });

    let ui_weak = ui.as_weak();
    let scroll_up_scanner = scanner.clone();
    ui.on_wifi_scroll_up(move || {
        println!("[WIFI_SCAN] scroll up");
        if let Some(ui) = ui_weak.upgrade() {
            scroll_up_scanner.borrow_mut().scroll_up(&ui);
        }
    });

    let ui_weak = ui.as_weak();
    let scroll_down_scanner = scanner.clone();
    ui.on_wifi_scroll_down(move || {
        println!("[WIFI_SCAN] scroll down");
        if let Some(ui) = ui_weak.upgrade() {
            scroll_down_scanner.borrow_mut().scroll_down(&ui);
        }
    });

    let scan_timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    scan_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(SCAN_POLL_INTERVAL_MS as u64),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                scanner.borrow_mut().tick(&ui);
            }
        },
    );
    scan_timer
}
