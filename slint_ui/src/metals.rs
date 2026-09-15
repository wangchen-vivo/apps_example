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
// network I/O never stalls the UI; the event loop polls the result atomics on
// the per-second tick.

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
const TICK_MS: u64 = 1000; // per-second wall-clock tick
const REFRESH_MS: u128 = 60 * 1000; // price refresh cadence
const FIRST_FETCH_DELAY_MS: u128 = 1500; // let the network settle after boot
/// A normal fetch lands in ~3 s; after this interval the UI reports that it
/// is still waiting. FETCH_PENDING intentionally remains set so a stalled
/// request can never overlap a second worker and consume another stack.
const FETCH_TIMEOUT_MS: u128 = 20 * 1000;

/// The proxy response for two quotes is well below 2 KiB (including its
/// roughly 300-byte header). Keep the complete response on the worker stack
/// so an HTTP request does not allocate several overlapping heap buffers.
const MAX_RESPONSE: usize = 2 * 1024;
const HTTP_REQUEST: &[u8] = b"GET /q=hf_XAU,hf_XAG HTTP/1.1\r\nHost: qt.gtimg.cn\r\nUser-Agent: slint_ui/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n";

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
// generation with the result; the UI tick only applies matching results.
static FETCH_GENERATION: AtomicU32 = AtomicU32::new(0);
static RESULT_GENERATION: AtomicU32 = AtomicU32::new(0);
static CLOCK_SERVER_TS: AtomicU64 = AtomicU64::new(0);
static CLOCK_MONO_MS: AtomicU64 = AtomicU64::new(0);

/// Fetch in-flight flag shared by the UI tick (setter) and worker (clearer).
static FETCH_PENDING: AtomicBool = AtomicBool::new(false);
/// Spawn moment of the in-flight fetch (uptime millis); drives timeout UI.
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
        // Preserve the original, known-working blocking transport. Keeping
        // this timeout setup also avoids changing the socket contract while
        // the allocation-heavy response handling is optimized separately.
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
                Err(librs::errno::Errno(errno)) => {
                    return Err(Error::from_raw_os_error(errno));
                }
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

#[derive(Clone, Copy)]
struct HttpResult {
    status: u16,
    server_ts: Option<u64>,
    xau: Option<i32>,
    xag: Option<i32>,
}

fn http_get(ip: [u8; 4], port: u16) -> IoResult<HttpResult> {
    let mut sock = TcpSocket::connect(ip, port)?;
    sock.write_all(HTTP_REQUEST)?;
    println!("[HTTP:metals] request sent");

    let mut raw = [0u8; MAX_RESPONSE];
    let mut raw_len = 0usize;
    loop {
        if raw_len == raw.len() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "HTTP response exceeds 2 KiB",
            ));
        }
        let n = sock.read(&mut raw[raw_len..])?;
        if n == 0 {
            break;
        }
        raw_len += n;
        // Do not rely on the proxy closing its side of the connection. Some
        // proxy versions keep it alive even after a complete response.
        if response_is_complete(&raw[..raw_len]) {
            break;
        }
    }
    println!("[HTTP:metals] received bytes={raw_len}");

    let sep = find_subslice(&raw[..raw_len], b"\r\n\r\n")
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "no HTTP head"))?;
    let body_start = sep + 4;
    let (status, server_ts, chunked) = {
        let head_str = core::str::from_utf8(&raw[..sep])
            .map_err(|_| Error::new(ErrorKind::InvalidData, "HTTP head is not UTF-8"))?;
        let status = head_str
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "bad status line"))?;
        (
            status,
            parse_date_header(head_str),
            header_is_chunked(head_str),
        )
    };

    let body_len = if chunked {
        dechunk_in_place(&mut raw[body_start..raw_len])?
    } else {
        raw_len - body_start
    };
    let body = &raw[body_start..body_start + body_len];

    Ok(HttpResult {
        status,
        server_ts,
        xau: extract_tencent_price(body, b"v_hf_XAU=\"").and_then(parse_price),
        xag: extract_tencent_price(body, b"v_hf_XAG=\"").and_then(parse_price),
    })
}

fn header_is_chunked(head: &str) -> bool {
    head.lines().any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    })
}

/// Whether the bytes received so far contain a complete HTTP response.
/// This endpoint always returns both quote records in one response. Detecting
/// their terminators avoids waiting for a proxy that ignores Connection: close.
fn response_is_complete(raw: &[u8]) -> bool {
    let Some(sep) = find_subslice(raw, b"\r\n\r\n") else {
        return false;
    };
    let body = &raw[sep + 4..];
    quote_is_complete(body, b"v_hf_XAU=\"") && quote_is_complete(body, b"v_hf_XAG=\"")
}

fn quote_is_complete(body: &[u8], needle: &[u8]) -> bool {
    let Some(start) = find_subslice(body, needle) else {
        return false;
    };
    find_subslice(&body[start + needle.len()..], b"\";").is_some()
}

fn dechunk_in_place(data: &mut [u8]) -> IoResult<usize> {
    let mut pos = 0usize;
    let mut out_len = 0usize;
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
        let chunk_size = {
            let size_str = core::str::from_utf8(&data[pos..line_end])
                .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid chunk size"))?;
            let size_str = size_str.split(';').next().unwrap_or(size_str).trim();
            usize::from_str_radix(size_str, 16)
                .map_err(|_| Error::new(ErrorKind::InvalidData, "bad chunk size"))?
        };
        pos = line_end + 2;
        if chunk_size == 0 {
            break;
        }
        let payload_end = pos
            .checked_add(chunk_size)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "chunk payload overflow"))?;
        if payload_end > data.len() {
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "chunk payload truncated",
            ));
        }
        let next_len = out_len
            .checked_add(chunk_size)
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "chunked body overflow"))?;
        if next_len > data.len() {
            return Err(Error::new(ErrorKind::InvalidData, "HTTP body too large"));
        }
        data.copy_within(pos..payload_end, out_len);
        out_len = next_len;
        pos = payload_end;
        if pos + 1 < data.len() && data[pos] == b'\r' && data[pos + 1] == b'\n' {
            pos += 2;
        }
        if pos >= data.len() {
            break;
        }
    }
    Ok(out_len)
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
fn extract_tencent_price<'a>(body: &'a [u8], needle: &[u8]) -> Option<&'a str> {
    let pos = find_subslice(body, needle)?;
    let rest = &body[pos + needle.len()..];
    let end = find_subslice(rest, b",")?;
    if end == 0 {
        return None;
    }
    let raw = core::str::from_utf8(&rest[..end]).ok()?;
    if !raw.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    Some(raw)
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
    let line = head.lines().find(|line| {
        line.get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("date:"))
    })?;
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
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
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
    timeout_reported: bool,
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
            timeout_reported: false,
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
        } else if !active && self.active {
            // The control socket is only needed for WiFi configuration. Do
            // not retain its kernel-side socket buffers after leaving.
            if let Some(fd) = self.ctl_socket.take() {
                let _ = librs::syscall::sys::Sys::close(fd);
            }
            if self.wifi_state != WifiState::Connected {
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
                if let Err(_) = set_passphrase(fd, config_str(blueos_kconfig::CONFIG_WLAN_PASSWORD))
                {
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
                    if let Some(fd) = self.ctl_socket.take() {
                        let _ = librs::syscall::sys::Sys::close(fd);
                    }
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
    /// Runs on a short-lived worker thread: the blocking socket calls here
    /// must never stall the UI event loop. The thread exits after this one
    /// request, so its stack is returned to the system heap immediately.
    fn fetch_prices(&mut self, _ui: &MainWindow) {
        let generation = FETCH_GENERATION.load(Ordering::Relaxed);
        match std::thread::Builder::new()
            .stack_size(FETCH_THREAD_STACK_SIZE)
            .spawn(move || fetch_once(generation))
        {
            // Dropping JoinHandle detaches the finite worker. BlueOS runs the
            // pthread cleanup routine on exit and deallocates its stack.
            Ok(worker) => drop(worker),
            Err(error) => {
                FETCH_PENDING.store(false, Ordering::Release);
                println!("[METALS] fetch thread spawn failed: {error}");
            }
        }
    }

    fn tick(&mut self, ui: &MainWindow) {
        let now = uptime_millis();

        // Drain a completed fetch: apply prices + clock sync to the UI. All
        // mutations stay on this (event-loop) thread; the worker only wrote
        // the atomics. A stale orphan's result carries an old generation and
        // is dropped here instead of overwriting fresher data.
        if RESULT_READY.swap(false, Ordering::Acquire) {
            ui.set_metals_refreshing(false);
            self.refreshing = false;
            self.timeout_reported = false;
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

        // Never start a second worker while the first one still owns its
        // stack. A timed-out socket cannot currently be cancelled safely;
        // it will publish an error and release FETCH_PENDING when it returns.
        if FETCH_PENDING.load(Ordering::Relaxed) {
            let started = FETCH_STARTED_MS.load(Ordering::Relaxed);
            if !self.timeout_reported
                && started != 0
                && now.saturating_sub(started as u128) >= FETCH_TIMEOUT_MS
            {
                ui.set_metals_status("请求超时，等待连接回收".into());
                self.timeout_reported = true;
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

        // Do not create a WiFi control socket or an HTTP worker while this
        // page is hidden. set_active() also closes an existing control fd.
        if !self.active {
            return;
        }

        if !self.ensure_wifi(ui, now) {
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
            FETCH_PENDING.store(true, Ordering::Release);
            self.timeout_reported = false;
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

// The release ELF reports a 2304-byte direct frame for fetch_once, including
// its single fixed HTTP buffer. 6 KiB leaves about 3.75 KiB for the thread
// trampoline and libc/socket call chains.
const FETCH_THREAD_STACK_SIZE: usize = 6 * 1024;

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

/// Execute exactly one request and return. Keeping this worker finite is
/// essential on the 303-KiB heap: its 6-KiB stack must not remain resident.
fn fetch_once(generation: u32) {
    match http_get(HTTP_PROXY_IP, HTTP_PROXY_PORT) {
        Ok(result) if result.status == 200 => {
            println!("[HTTP:metals] response 200");
            RESULT_HTTP_CODE.store(200, Ordering::Relaxed);
            if let Some(ts) = result.server_ts {
                RESULT_SERVER_TS.store(ts, Ordering::Relaxed);
            }
            if let Some(value) = result.xau {
                RESULT_XAU.store(value, Ordering::Relaxed);
            }
            if let Some(value) = result.xag {
                RESULT_XAG.store(value, Ordering::Relaxed);
            }
            RESULT_OK.store(true, Ordering::Relaxed);
        }
        Ok(result) => {
            println!("[HTTP:metals] response {}", result.status);
            RESULT_HTTP_CODE.store(result.status as i32, Ordering::Relaxed);
            RESULT_OK.store(false, Ordering::Relaxed);
        }
        Err(err) => {
            println!(
                "[HTTP:metals] err kind={:?} raw={:?}",
                err.kind(),
                err.raw_os_error()
            );
            RESULT_HTTP_CODE.store(0, Ordering::Relaxed);
            RESULT_OK.store(false, Ordering::Relaxed);
        }
    }
    RESULT_GENERATION.store(generation, Ordering::Relaxed);
    FETCH_PENDING.store(false, Ordering::Release);
    RESULT_READY.store(true, Ordering::Release);
}
