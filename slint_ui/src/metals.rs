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

// Gold & silver price fetcher for the launcher MetalsPage.
// Mirrors apps/example/metals_clock: Tencent COMEX quote (1 request for both
// metals) + bilibili unix-timestamp time sync, displayed via a locally-derived
// wall clock so the per-second tick costs zero network requests.
//
// A slint::Timer drives the WiFi state machine on the UI thread. The HTTP
// fetch is blocking (bounded by SO_RCVTIMEO) and runs on a worker thread so
// a dead network never stalls the UI; the event loop polls the result
// atomics on the per-second tick.

use crate::app_window::MainWindow;
use crate::{syscall_error, uptime_millis};
use librs::syscall::Syscall;
use slint::ComponentHandle;
use std::{
    cell::RefCell,
    io::{Error, ErrorKind, Result as IoResult},
    rc::Rc,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering},
};

// Board reaches the public APIs through a plain-HTTP reverse proxy on the host
// (kernel has TCP but no DNS/TLS).
const HTTP_PROXY_IP: [u8; 4] = [10, 171, 198, 12];
const HTTP_PROXY_PORT: u16 = 18085;
const HTTP_HOST: &str = "qt.gtimg.cn";
const HTTP_PATH: &str = "/q=hf_XAU,hf_XAG";

const TICK_MS: u64 = 1000; // per-second wall-clock tick
const REFRESH_MS: u128 = 60 * 1000; // price refresh cadence
const FIRST_FETCH_DELAY_MS: u128 = 1500; // let the network settle after boot
/// Orphan-recycle window. A normal fetch lands in ~3 s; the SO_RCVTIMEO
/// bounds the read side at 10 s. Only a dead network (SYN retry loop)
/// can exceed this, and then the UI tick stops waiting on that worker,
/// releases FETCH_PENDING so a fresh fetch can start, and the stale
/// worker's late result is discarded via the generation check.
const FETCH_TIMEOUT_MS: u128 = 20 * 1000;

const MAX_BODY: usize = 4 * 1024;
// 8 KiB reserve was OOMing the kernel heap (8704-byte alloc) after Wi-Fi
// init; the proxy's header is ~300 bytes so 1 KiB is ample.
const MAX_HEAD: usize = 1 * 1024;
const READ_CHUNK: usize = 512;

// ---------------------------------------------------------------------------
// Fetch result channel: worker thread (producer) -> UI tick (consumer).
// MainWindow is !Send, so the blocking HTTP fetch runs off-thread and the
// event loop only ever reads these atomics. Prices are i32 in 0.01 units
// (e.g. 73456 = 734.56); FRESH marks fields that this fetch has updated.
// ---------------------------------------------------------------------------
static RESULT_READY: AtomicBool = AtomicBool::new(false);
static RESULT_OK: AtomicBool = AtomicBool::new(false);
static RESULT_HTTP_CODE: AtomicI32 = AtomicI32::new(0);
static RESULT_SERVER_TS: AtomicU64 = AtomicU64::new(0);
static RESULT_XAU: AtomicI32 = AtomicI32::new(0);
static RESULT_XAG: AtomicI32 = AtomicI32::new(0);
// Generation tag: bumped when a fetch is launched. A worker publishes its
// generation with the result; the UI tick only applies results whose
// generation matches the current one, so a stale orphan worker (past its
// recycle window) can never overwrite fresher data.
static FETCH_GENERATION: AtomicU32 = AtomicU32::new(0);
static RESULT_GENERATION: AtomicU32 = AtomicU32::new(0);
static CLOCK_SERVER_TS: AtomicU64 = AtomicU64::new(0);
static CLOCK_MONO_MS: AtomicU64 = AtomicU64::new(0);

/// Fetch in-flight flag shared by the UI tick (setter) and worker (clearer).
static FETCH_PENDING: AtomicBool = AtomicBool::new(false);
/// Spawn moment of the in-flight fetch (uptime millis); drives recycle.
static FETCH_STARTED_MS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Wall clock: network-synced unix seconds + local monotonic derivation.
// ---------------------------------------------------------------------------

struct Clock {
    server_ts: u64,
    mono_at_sync: u128,
}

impl Clock {
    const fn new() -> Self {
        Self {
            server_ts: 0,
            mono_at_sync: 0,
        }
    }
    fn sync(&mut self, ts: u64) {
        self.server_ts = ts;
        self.mono_at_sync = uptime_millis();
    }
    /// Current Beijing time as "HH:MM" (UTC+8, no DST — integer math is exact).
    fn now_hhmm(&self) -> Option<std::string::String> {
        if self.server_ts == 0 {
            return None;
        }
        let elapsed_ms = uptime_millis().saturating_sub(self.mono_at_sync);
        let now = self.server_ts + (elapsed_ms / 1000) as u64;
        let day_secs = (now + 8 * 3600) % 86_400;
        Some(format!(
            "{:02}:{:02}",
            day_secs / 3600,
            (day_secs % 3600) / 60
        ))
    }
}

// ---------------------------------------------------------------------------
// Minimal blocking HTTP client over librs sockets.
// ---------------------------------------------------------------------------

struct TcpSocket {
    fd: libc::c_int,
}

fn config_str(value: &[u8]) -> &str {
    let value = value.split(|&byte| byte == 0).next().unwrap_or(value);
    core::str::from_utf8(value).unwrap_or("")
}

impl TcpSocket {
    fn connect(ip: [u8; 4], port: u16) -> IoResult<Self> {
        println!(
            "[HTTP:metals] connect {}.{}.{}.{}:{}",
            ip[0], ip[1], ip[2], ip[3], port
        );
        let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(syscall_error(fd));
        }
        // Don't block the UI thread forever on a dead network.
        let tv = libc::timeval {
            tv_sec: 10,
            tv_usec: 0,
        };
        unsafe {
            let ret = librs::syscall::sys::Sys::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                core::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
            if let Err(librs::errno::Errno(errno)) = ret {
                let _ = librs::syscall::sys::Sys::close(fd);
                return Err(Error::from_raw_os_error(errno));
            }
        }
        let addr = libc::sockaddr_in {
            sin_len: core::mem::size_of::<libc::sockaddr_in>() as u8,
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(ip),
            },
            sin_vport: 0,
            sin_zero: [0; 6],
        };
        let ret = unsafe {
            librs::syscall::sys::Sys::connect(
                fd,
                &addr as *const libc::sockaddr_in as *const libc::sockaddr,
                core::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if let Err(librs::errno::Errno(errno)) = ret {
            println!("[HTTP:metals] connect err errno={}", errno);
            let _ = librs::syscall::sys::Sys::close(fd);
            return Err(Error::from_raw_os_error(errno));
        }
        println!("[HTTP:metals] connect ok port={}", port);
        Ok(Self { fd })
    }
    fn write_all(&mut self, mut buf: &[u8]) -> IoResult<()> {
        while !buf.is_empty() {
            match librs::syscall::sys::Sys::send(self.fd, buf, 0) {
                Ok(0) => return Err(Error::new(ErrorKind::WriteZero, "socket write zero")),
                Ok(n) => buf = &buf[n..],
                Err(librs::errno::Errno(errno)) => return Err(Error::from_raw_os_error(errno)),
            }
        }
        Ok(())
    }
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        match librs::syscall::sys::Sys::recv(self.fd, buf, 0) {
            Ok(n) => Ok(n),
            Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
        }
    }
}

impl Drop for TcpSocket {
    fn drop(&mut self) {
        let _ = librs::syscall::sys::Sys::close(self.fd);
    }
}

fn http_get(ip: [u8; 4], port: u16, path: &str, host: &str) -> IoResult<(u16, String, Vec<u8>)> {
    let mut sock = TcpSocket::connect(ip, port)?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: slint_ui/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        path, host
    );
    sock.write_all(request.as_bytes())?;

    let mut raw: Vec<u8> = Vec::with_capacity(MAX_HEAD + READ_CHUNK);
    let mut chunk = [0u8; READ_CHUNK];
    let raw_cap = MAX_HEAD + MAX_BODY;
    loop {
        if raw.len() >= raw_cap {
            break;
        }
        let n = sock.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
    }

    let sep = find_subslice(&raw, b"\r\n\r\n")
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no HTTP head"))?;
    let head = &raw[..sep];
    let mut body = raw[sep + 4..].to_vec();
    let head_str = std::string::String::from_utf8_lossy(head).into_owned();
    let status = head_str
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "bad status line"))?;
    let body_bytes = if head_str
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        dechunk(&body, MAX_BODY)?
    } else {
        body.truncate(MAX_BODY);
        body
    };
    Ok((status, head_str, body_bytes))
}

fn dechunk(data: &[u8], cap: usize) -> IoResult<Vec<u8>> {
    let mut out = Vec::with_capacity(READ_CHUNK);
    let mut pos = 0usize;
    loop {
        let mut line_end = None;
        let mut i = pos;
        while i + 1 < data.len() {
            if data[i] == b'\r' && data[i + 1] == b'\n' {
                line_end = Some(i);
                break;
            }
            i += 1;
        }
        let line_end = line_end
            .ok_or_else(|| Error::new(ErrorKind::UnexpectedEof, "chunk size line truncated"))?;
        let size_str = std::string::String::from_utf8_lossy(&data[pos..line_end]);
        let chunk_size = usize::from_str_radix(size_str.trim(), 16)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "bad chunk size"))?;
        pos = line_end + 2;
        if chunk_size == 0 {
            break;
        }
        if pos + chunk_size > data.len() {
            let have = data.len().saturating_sub(pos);
            out.extend_from_slice(&data[pos..pos + have]);
            break;
        }
        out.extend_from_slice(&data[pos..pos + chunk_size]);
        pos += chunk_size;
        if pos + 1 < data.len() && data[pos] == b'\r' && data[pos + 1] == b'\n' {
            pos += 2;
        }
        if out.len() >= cap || pos >= data.len() {
            break;
        }
    }
    out.truncate(cap);
    Ok(out)
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

// ---------------------------------------------------------------------------
// Data extraction — byte scans only; no serde, no regex.
// ---------------------------------------------------------------------------

/// Parse Tencent quote text for one instrument line. Field 0 is current price.
///   v_hf_GC="4466.56,-0.22,...";
fn extract_tencent_price(body: &[u8], key: &str) -> Option<std::string::String> {
    let mut needle = std::string::String::from("v_");
    needle.push_str(key);
    needle.push_str("=\"");
    let pos = find_subslice(body, needle.as_bytes())?;
    let rest = &body[pos + needle.len()..];
    let end = find_subslice(rest, b",")?;
    if end == 0 {
        return None;
    }
    let raw = core::str::from_utf8(&rest[..end]).ok()?;
    if !raw.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(raw.to_string())
}

/// Parse a decimal price string into hundredths as i32 ("734.56" -> 73456).
/// Cross-atomics cannot carry strings (no atomic String), so the worker
/// converts to a fixed-point integer before publishing.
fn parse_price(raw: &str) -> Option<i32> {
    let (whole, frac) = match raw.split_once('.') {
        Some((w, f)) => (w, f),
        None => (raw, ""),
    };
    let whole: i32 = whole.parse().ok()?;
    let cents = match frac {
        "" => 0,
        f if f.len() == 1 => f.parse::<i32>().ok()? * 10,
        f => f.get(..2)?.parse::<i32>().ok()?,
    };
    whole.checked_mul(100)?.checked_add(cents)
}

/// Parse an RFC 1123 `Date` header (e.g. "date: Thu, 10 Sep 2026 07:21:36 GMT")
/// into unix seconds. Used to derive Beijing time from the price response so
/// the demo needs only one HTTP request.
fn parse_date_header(head: &str) -> Option<u64> {
    let line = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("date:"))?;
    let colon = line.find(':')?;
    // "Thu, 10 Sep 2026 07:21:36 GMT"
    let rest = line[colon + 1..].trim();
    let mut parts = rest.split_whitespace();
    let _ = parts.next()?; // weekday "Thu," — ignored, date is authoritative
    let day: u64 = parts.next()?.trim_end_matches(',').parse().ok()?;
    let month = parts.next()?;
    let year: u64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut t = time.split(':');
    let hh: u64 = t.next()?.parse().ok()?;
    let mm: u64 = t.next()?.parse().ok()?;
    let ss: u64 = t.next()?.parse().ok()?;
    let m = match month {
        "Jan" => 1, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    // Days since 1970-01-01 (UTC). Valid for 2001-2099 (no leap-century edge).
    let y = if m <= 2 { year - 1 } else { year };
    let era = y / 100;
    let yoe = y - era * 100;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days_since_epoch = era as u64 * 146_097 + doe as u64 - 719_468;
    Some(days_since_epoch * 86_400 + hh * 3600 + mm * 60 + ss)
}

// ---------------------------------------------------------------------------
// WiFi station bring-up. The built-in (non-OTA) firmware does not connect on
// its own, so the page connects to the hotspot before sending TCP traffic.
// ---------------------------------------------------------------------------

const WIFI_CONNECT_GRACE_MS: u128 = 3_000; // association grace period after connect ioctl

#[derive(Clone, Copy, PartialEq)]
enum WifiState {
    Idle,
    Connecting { started_at: u128 },
    Connected,
    Failed,
}

fn wlan0_name() -> [libc::c_char; 16] {
    let mut name = [0 as libc::c_char; 16];
    for (dst, src) in name.iter_mut().zip(b"wlan0\0") {
        *dst = *src as libc::c_char;
    }
    name
}

fn open_ctl_socket() -> IoResult<libc::c_int> {
    let fd = librs::net::socket::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
    if fd < 0 {
        Err(syscall_error(fd))
    } else {
        Ok(fd)
    }
}

fn set_passphrase(fd: libc::c_int, passphrase: &str) -> IoResult<()> {
    let point = libc::iw_point {
        pointer: passphrase.as_ptr() as *mut libc::c_void,
        length: passphrase.len() as u16,
        flags: 0,
    };
    let mut req = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { encoding: point },
    };
    match unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWENCODE,
            &mut req as *mut libc::iwreq as *mut libc::c_void,
        )
    } {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

fn trigger_connect(fd: libc::c_int, ssid: &str) -> IoResult<()> {
    let point = libc::iw_point {
        pointer: ssid.as_ptr() as *mut libc::c_void,
        length: ssid.len() as u16,
        flags: 0,
    };
    let mut req = libc::iwreq {
        ifr_ifrn: libc::__c_anonymous_iwreq {
            ifrn_name: wlan0_name(),
        },
        u: libc::iwreq_data { essid: point },
    };
    match unsafe {
        librs::syscall::sys::Sys::ioctl(
            fd,
            libc::SIOCSIWESSID,
            &mut req as *mut libc::iwreq as *mut libc::c_void,
        )
    } {
        Ok(ret) if ret < 0 => Err(syscall_error(ret)),
        Ok(_) => Ok(()),
        Err(librs::errno::Errno(errno)) => Err(Error::from_raw_os_error(errno)),
    }
}

// Fetcher state machine, driven by a per-second slint::Timer.
// ---------------------------------------------------------------------------

struct MetalsFetcher {
    clock: Clock,
    started_at: u128,
    last_refresh_ms: u128,
    last_minute: Option<std::string::String>,
    refreshing: bool, // fetch trigger + UI "in progress" flag (one bool)
    active: bool, // only fetch while the Metals page is on screen
    wifi_state: WifiState,
    ctl_socket: Option<libc::c_int>,
}

impl MetalsFetcher {
    fn new() -> Self {
        let now = uptime_millis();
        Self {
            clock: Clock::new(),
            started_at: now,
            last_refresh_ms: 0,
            last_minute: None,
            refreshing: false,
            active: false,
            wifi_state: WifiState::Idle,
            ctl_socket: None,
        }
    }

    fn request_refresh(&mut self, ui: &MainWindow) {
        self.refreshing = true;
        ui.set_metals_refreshing(true);
    }

    fn set_active(&mut self, ui: &MainWindow, active: bool) {
        // Entering the page triggers an immediate refresh.
        if active && !self.active {
            self.refreshing = true;
            ui.set_metals_refreshing(true);
            if self.wifi_state == WifiState::Failed {
                self.wifi_state = WifiState::Idle;
            }
        }
        self.active = active;
    }

    /// Drive the WiFi state machine; returns true once the link is up.
    fn ensure_wifi(&mut self, ui: &MainWindow, now: u128) -> bool {
        match self.wifi_state {
            WifiState::Connected => true,
            WifiState::Failed => false,
            WifiState::Idle => {
                ui.set_metals_status("正在连接 WiFi...".into());
                match open_ctl_socket() {
                    Ok(fd) => self.ctl_socket = Some(fd),
                    Err(_) => {
                        println!("[WIFI:metals] socket err");
                        self.wifi_state = WifiState::Failed;
                        ui.set_metals_status("WiFi 套接字失败".into());
                        return false;
                    }
                }
                let fd = self.ctl_socket.unwrap();
                if let Err(_) = set_passphrase(fd, config_str(blueos_kconfig::CONFIG_WLAN_PASSWORD)) {
                    println!("[WIFI:metals] psk err");
                }
                match trigger_connect(fd, config_str(blueos_kconfig::CONFIG_WLAN_SSID)) {
                    Ok(()) => {
                        println!("[WIFI:metals] connecting");
                        self.wifi_state = WifiState::Connecting { started_at: now };
                    }
                    Err(_) => {
                        println!("[WIFI:metals] connect err");
                        self.wifi_state = WifiState::Failed;
                        ui.set_metals_status("WiFi 连接失败".into());
                    }
                }
                false
            }
            WifiState::Connecting { started_at } => {
                // The kernel's SIOCGIFFLAGS path does not write the flags back
                // to userspace, so we can't poll for a real link-up event.
                // Wait briefly for station association after the connect ioctl.
                if now.saturating_sub(started_at) >= WIFI_CONNECT_GRACE_MS {
                    println!("[WIFI:metals] up");
                    self.wifi_state = WifiState::Connected;
                    ui.set_metals_status("WiFi 已连接".into());
                    true
                } else {
                    false
                }
            }
        }
    }

    /// One Tencent request updates both prices and resyncs the wall clock
    /// from the response `Date` header, so the demo needs a single request.
    /// Runs on a worker thread: the blocking socket calls here must never
    /// stall the UI event loop. Results go through the atomics above.
    fn fetch_prices(&mut self, _ui: &MainWindow) {
        let generation = FETCH_GENERATION.load(Ordering::Relaxed);
        let _ = std::thread::Builder::new()
            .stack_size(FETCH_THREAD_STACK_SIZE)
            .spawn(move || {
                match http_get(HTTP_PROXY_IP, HTTP_PROXY_PORT, HTTP_PATH, HTTP_HOST) {
                    Ok((200, head, body)) => {
                        println!("[HTTP:metals] response 200");
                        RESULT_HTTP_CODE.store(200, Ordering::Relaxed);
                        if let Some(ts) = parse_date_header(&head) {
                            RESULT_SERVER_TS.store(ts, Ordering::Relaxed);
                        }
                        if let Some(gold) = extract_tencent_price(&body, "hf_XAU") {
                            if let Some(v) = parse_price(&gold) {
                                RESULT_XAU.store(v, Ordering::Relaxed);
                            }
                        }
                        if let Some(silver) = extract_tencent_price(&body, "hf_XAG") {
                            if let Some(v) = parse_price(&silver) {
                                RESULT_XAG.store(v, Ordering::Relaxed);
                            }
                        }
                        RESULT_OK.store(true, Ordering::Relaxed);
                    }
                    Ok((code, _, _)) => {
                        println!("[HTTP:metals] response {}", code);
                        RESULT_HTTP_CODE.store(code as i32, Ordering::Relaxed);
                        RESULT_OK.store(false, Ordering::Relaxed);
                    }
                    Err(err) => {
                        println!("[HTTP:metals] err kind={:?} raw={:?}", err.kind(), err.raw_os_error());
                        RESULT_HTTP_CODE.store(0, Ordering::Relaxed);
                        RESULT_OK.store(false, Ordering::Relaxed);
                    }
                }
                RESULT_GENERATION.store(generation, Ordering::Relaxed);
                RESULT_READY.store(true, Ordering::Relaxed);
                FETCH_PENDING.store(false, Ordering::Relaxed);
            });
    }

    fn tick(&mut self, ui: &MainWindow) {
        let now = uptime_millis();

        // Drain a completed fetch: apply prices + clock sync to the UI. All
        // mutations stay on this (event-loop) thread; the worker only wrote
        // the atomics. A stale orphan's result carries an old generation and
        // is dropped here instead of overwriting fresher data.
        if RESULT_READY.swap(false, Ordering::Relaxed) {
            FETCH_PENDING.store(false, Ordering::Relaxed);
            ui.set_metals_refreshing(false);
            self.refreshing = false;
            let generation = RESULT_GENERATION.load(Ordering::Relaxed);
            if generation != FETCH_GENERATION.load(Ordering::Relaxed) {
                println!("[METALS] dropped stale fetch result (gen {generation})");
            } else if RESULT_OK.load(Ordering::Relaxed) {
                let ts = RESULT_SERVER_TS.load(Ordering::Relaxed);
                if ts != 0 {
                    self.clock.sync(ts);
                    if let Some(hm) = self.clock.now_hhmm() {
                        ui.set_metals_time(hm.clone().into());
                        self.last_minute = Some(hm);
                    }
                }
                if let Some(gold) = format_fixed_price(RESULT_XAU.load(Ordering::Relaxed)) {
                    ui.set_metals_xau(gold.into());
                }
                if let Some(silver) = format_fixed_price(RESULT_XAG.load(Ordering::Relaxed)) {
                    ui.set_metals_xag(silver.into());
                }
                ui.set_metals_status("".into());
            } else {
                let code = RESULT_HTTP_CODE.load(Ordering::Relaxed);
                let text = if code == 0 {
                    "价格获取失败".to_string()
                } else {
                    format!("HTTP 状态 {}", code)
                };
                ui.set_metals_status(text.into());
            }
        }

        // Orphan recycle: a fetch stuck past FETCH_TIMEOUT_MS (dead network,
        // connect retry loop) must not hold FETCH_PENDING forever. Release
        // the slot so the cadence can start a fresh worker; the old thread,
        // if it ever returns, fails the generation check above. The thread
        // itself is detached and cannot be killed (kernel pthreads have no
        // kill) — it dies on its own when the TCP stack gives up, and its
        // stack + fd are reclaimed then.
        if FETCH_PENDING.load(Ordering::Relaxed) {
            let started = FETCH_STARTED_MS.load(Ordering::Relaxed);
            if started != 0 && now.saturating_sub(started as u128) >= FETCH_TIMEOUT_MS {
                println!("[METALS] fetch timeout, releasing slot");
                FETCH_PENDING.store(false, Ordering::Relaxed);
            }
        }

        // Per-second wall-clock tick, but only rewrite the property on a
        // minute change (the equality guard avoids per-frame allocation).
        if let Some(hm) = self.clock.now_hhmm() {
            if self.last_minute.as_deref() != Some(hm.as_str()) {
                ui.set_metals_time(hm.clone().into());
                self.last_minute = Some(hm);
            }
        }

        // Bring up WiFi from boot, independent of page visibility, so the
        // link is already up by the time the user opens the page.
        if !self.ensure_wifi(ui, now) {
            return;
        }

        // HTTP requests only run while the page is on screen.
        if !self.active {
            return;
        }

        let first_due = now.saturating_sub(self.started_at) >= FIRST_FETCH_DELAY_MS;
        // `refreshing` doubles as the fetch trigger and the UI "in progress"
        // flag — set when the page is entered, the user taps refresh, or the
        // periodic cadence elapses. The fetch itself is non-blocking: it
        // spawns a worker and the result lands on a later tick.
        let refresh_due = (self.refreshing && first_due)
            || (first_due && now.saturating_sub(self.last_refresh_ms) >= REFRESH_MS);
        if refresh_due && !FETCH_PENDING.load(Ordering::Relaxed) {
            self.last_refresh_ms = now;
            if !self.refreshing {
                self.refreshing = true;
                ui.set_metals_refreshing(true);
            }
            println!("[METALS] fetch start");
            FETCH_GENERATION.fetch_add(1, Ordering::Relaxed);
            FETCH_STARTED_MS.store(now as u64, Ordering::Relaxed);
            FETCH_PENDING.store(true, Ordering::Relaxed);
            self.fetch_prices(ui);
        }
    }
}

/// Format fixed-point hundredths back into the display string ("73456" ->
/// "734.56"). Returns None for the zero value so a fetch without this
/// field leaves the previous price on screen.
fn format_fixed_price(cents: i32) -> Option<std::string::String> {
    if cents == 0 {
        return None;
    }
    Some(format!("{}.{:02}", cents / 100, (cents % 100).abs()))
}

const FETCH_THREAD_STACK_SIZE: usize = 24 * 1024;

/// Connect the metals fetcher to the shared launcher window. The returned
/// timer must stay alive for as long as the Slint event loop runs.
pub(crate) fn install(ui: &MainWindow) -> slint::Timer {
    let fetcher = Rc::new(RefCell::new(MetalsFetcher::new()));

    let cb_fetcher = fetcher.clone();
    let cb_ui = ui.as_weak();
    ui.on_metals_refresh(move || {
        println!("[METALS] refresh");
        if let Some(ui) = cb_ui.upgrade() {
            cb_fetcher.borrow_mut().request_refresh(&ui);
        }
    });

    let active_fetcher = fetcher.clone();
    let active_ui = ui.as_weak();
    let page_active = std::rc::Rc::new(std::cell::Cell::new(false));
    let active_state = page_active.clone();
    ui.on_metals_active_changed(move |active| {
        if active_state.replace(active) == active {
            return;
        }
        println!("[PAGE] {} metals", if active { "enter" } else { "exit" });
        if let Some(ui) = active_ui.upgrade() {
            active_fetcher.borrow_mut().set_active(&ui, active);
        }
    });

    let timer = slint::Timer::default();
    let timer_ui = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(TICK_MS),
        move || {
            if let Some(ui) = timer_ui.upgrade() {
                fetcher.borrow_mut().tick(&ui);
            }
        },
    );
    timer
}
