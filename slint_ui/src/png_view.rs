// PNG streaming viewer for the SD card page (slint_ui).
// Streams a PNG file from the SD card, decodes it line-by-line with the
// png crate's StreamingDecoder, and writes RGB565 pixels directly to the
// framebuffer. Compatible with the provider-managed background renderer:
// while a PNG is displayed, the Slint software renderer's draw_if_needed is
// suspended so the direct pixels are not overwritten.
//
// Ported from shihange's SD-card example (commit 66e8973), adapted to the
// slint_ui FbFile API and the existing SD card page layout.

use crate::FbFile;
use librs::syscall::Syscall;
// PNG viewer uses its own Rgb565Pixel for direct framebuffer writes.
// The byte layout matches the panel's big-endian RGB565 format.
#[derive(Clone, Copy)]
struct Rgb565Pixel(pub u16);
use slint::ComponentHandle;
use std::cell::RefCell;
use std::io::{Error, ErrorKind, Read, Result as IoResult};
use std::rc::Rc;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PNG_MAX_DIMENSION: u32 = 480;
// Panel fills the viewer's visible area: below the 60 px title bar and above
// the 56 px bottom nav (480 x 364), centered horizontally.
const PNG_PANEL_X: usize = 0;
const PNG_PANEL_Y: usize = 60;
const PNG_DISPLAY_X: usize = PNG_PANEL_X;
const PNG_DISPLAY_Y: usize = PNG_PANEL_Y;
const PNG_DISPLAY_MAX_WIDTH: u32 = PNG_PANEL_W as u32;
const PNG_DISPLAY_MAX_HEIGHT: u32 = PNG_PANEL_H as u32;
const PNG_PANEL_W: usize = 480;
const PNG_PANEL_H: usize = 364;
const PNG_FRAMEBUFFER_BATCH_LINES: usize = 16;
const PNG_OVERLAY_BG: u16 = to_rgb565(0x0b, 0x11, 0x15).0;
const PNG_PANEL_BG: u16 = to_rgb565(0x05, 0x09, 0x0c).0;
const PNG_PANEL_BORDER: u16 = to_rgb565(0x29, 0x33, 0x4d).0;

// DEFLATE requires a 32 KiB history window. Another 8 KiB lets the decoder
// make progress before the processed prefix is compacted.
const PNG_STREAM_BUFFER_SIZE: usize = 40 * 1024;
const PNG_INPUT_BUFFER_SIZE: usize = 2 * 1024;

// Maximum row bytes for worst-case PNG (480x RGB8 = 1440 bytes).
const MAX_ROW_BYTES: usize = 1440;
// Maximum display width in pixels (the panel width).
const MAX_DISPLAY_WIDTH: usize = PNG_PANEL_W;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub(crate) struct PngRenderRequest {
    pub(crate) path: String,
    pub(crate) source_width: u32,
    pub(crate) source_height: u32,
    pub(crate) display_width: u32,
    pub(crate) display_height: u32,
}

#[derive(Default)]
pub(crate) struct PngRenderState {
    pub(crate) pending: Option<PngRenderRequest>,
    pub(crate) active: bool,
    pub(crate) log_after_close_frame: bool,
}

pub(crate) type SharedPngRenderState = Rc<RefCell<PngRenderState>>;

// ---------------------------------------------------------------------------
// PNG header inspection (magic, dimensions, constraints)
// ---------------------------------------------------------------------------

pub(crate) fn inspect_png(path: &str) -> IoResult<(u32, u32)> {
    let mut file = std::fs::File::open(path)?;
    let mut header = [0u8; 33];
    file.read_exact(&mut header)?;
    if header[..8] != [137, 80, 78, 71, 13, 10, 26, 10] {
        return Err(Error::new(ErrorKind::InvalidData, "invalid PNG header"));
    }
    let width = u32::from_be_bytes([header[16], header[17], header[18], header[19]]);
    let height = u32::from_be_bytes([header[20], header[21], header[22], header[23]]);
    if width == 0 || height == 0 || width > PNG_MAX_DIMENSION || height > PNG_MAX_DIMENSION {
        return Err(Error::new(
            ErrorKind::InvalidData,
            format!(
                "PNG dimensions {width}x{height} exceed {PNG_MAX_DIMENSION}x{PNG_MAX_DIMENSION}"
            ),
        ));
    }
    if header[28] != 0 {
        return Err(Error::new(ErrorKind::InvalidData, "interlaced PNG is not supported"));
    }
    Ok((width, height))
}

pub(crate) fn fit_png_dimensions(width: u32, height: u32) -> (u32, u32) {
    // Scale to fit inside the panel while preserving aspect ratio (contain).
    // scale = min(max_w / w, max_h / h); pick the smaller via cross products.
    if width == 0 || height == 0 {
        return (PNG_DISPLAY_MAX_WIDTH, PNG_DISPLAY_MAX_HEIGHT);
    }
    let max_w = PNG_DISPLAY_MAX_WIDTH as u64;
    let max_h = PNG_DISPLAY_MAX_HEIGHT as u64;
    let w = width as u64;
    let h = height as u64;
    // width-bound when max_w/w < max_h/h, i.e. max_w*h < max_h*w.
    if max_w * h < max_h * w {
        ((max_w) as u32, (h * max_w / w) as u32)
    } else {
        ((w * max_h / h) as u32, (max_h) as u32)
    }
}

// ---------------------------------------------------------------------------
// Internal: pixel helpers
// ---------------------------------------------------------------------------

fn png_error(error: impl ToString) -> Error {
    Error::new(ErrorKind::Other, error.to_string())
}

const fn to_rgb565(red: u8, green: u8, blue: u8) -> Rgb565Pixel {
    Rgb565Pixel(((red as u16 & 0xf8) << 8) | ((green as u16 & 0xfc) << 3) | ((blue as u16) >> 3))
}

fn blend_png_channel(channel: u8, background: u8, alpha: u8) -> u8 {
    if alpha == 255 {
        channel
    } else if alpha == 0 {
        background
    } else {
        ((channel as u16 * alpha as u16 + background as u16 * (255 - alpha as u16)) / 255) as u8
    }
}

fn fill_rgb565(bytes: &mut [u8], color: Rgb565Pixel) {
    let be = color.0.to_be_bytes();
    for chunk in bytes.chunks_exact_mut(2) {
        chunk.copy_from_slice(&be);
    }
}

// ---------------------------------------------------------------------------
// Internal: streaming decoder wrapper types
// ---------------------------------------------------------------------------

struct PngStreamInfo {
    width: u32,
    height: u32,
    row_length: usize,
    filter_bytes_per_pixel: usize,
    color_type: png::ColorType,
    bit_depth: png::BitDepth,
    transparency: Vec<u8>,
}

impl PngStreamInfo {
    fn from_decoder(info: &png::Info<'_>) -> IoResult<Self> {
        if info.width == 0
            || info.height == 0
            || info.width > PNG_MAX_DIMENSION
            || info.height > PNG_MAX_DIMENSION
        {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "PNG dimensions changed while decoding",
            ));
        }
        if info.interlaced {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "interlaced PNG is not supported",
            ));
        }
        if info.animation_control.is_some() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "animated PNG is not supported",
            ));
        }
        let row_raw = info.raw_row_length();
        if !(2..=(PNG_MAX_DIMENSION as usize * 8 + 1)).contains(&row_raw) {
            return Err(Error::new(ErrorKind::InvalidData, "PNG row is too large"));
        }
        if info.color_type == png::ColorType::Indexed && (info.palette.is_none() || info.palette.as_ref().unwrap().is_empty()) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "indexed PNG has no valid palette",
            ));
        }
        // PNG filters operate on bytes, not packed pixels. Sub-byte grayscale
        // and indexed formats have a filter byte + raw row sharing the same
        // "row_length" as the packed pixel data.
        let row_length = row_raw;
        let transparency = info
            .trns
            .as_ref()
            .map(|trns| trns.to_vec())
            .unwrap_or_default();
        Ok(Self {
            width: info.width,
            height: info.height,
            row_length,
            filter_bytes_per_pixel: info.bytes_per_pixel().max(1),
            color_type: info.color_type,
            bit_depth: info.bit_depth,
            transparency,
        })
    }
}

fn read_png_sample(row: &[u8], sample_index: usize, depth: png::BitDepth) -> Option<u16> {
    match depth {
        png::BitDepth::One | png::BitDepth::Two | png::BitDepth::Four => {
            let byte = row.get(sample_index / 8 / 2)?;
            let shift = (8 - depth as u8) - (sample_index % (8 / depth as usize)) as u8 * depth as u8;
            Some(((*byte >> shift) as u16) & ((1 << depth as u8) - 1))
        }
        png::BitDepth::Eight => row.get(sample_index).copied().map(u16::from),
        png::BitDepth::Sixteen => {
            let offset = sample_index * 2;
            if offset + 1 >= row.len() {
                return None;
            }
            Some(u16::from_be_bytes([row[offset], row[offset + 1]]))
        }
    }
}

fn png_sample_to_u8(sample: u16, depth: png::BitDepth) -> u8 {
    match depth {
        png::BitDepth::One => (sample * 255) as u8,
        png::BitDepth::Two => (sample * 85) as u8,
        png::BitDepth::Four => (sample * 17) as u8,
        png::BitDepth::Eight => sample as u8,
        png::BitDepth::Sixteen => ((sample as u32 + 128) / 257) as u8,
    }
}

fn png_transparent_sample(bytes: &[u8], offset: usize) -> Option<u16> {
    if offset < bytes.len() {
        Some(u16::from(bytes[offset]))
    } else {
        None
    }
}

fn png_pixel(info: &PngStreamInfo, row: &[u8], x: usize) -> IoResult<(u8, u8, u8, u8)> {
    let invalid = || Error::new(ErrorKind::InvalidData, "invalid PNG pixel data");
    let channels = info.color_type.samples();
    let sample = x * channels;
    match info.color_type {
        png::ColorType::Grayscale => {
            let gray = read_png_sample(row, x, info.bit_depth).ok_or_else(invalid)?;
            let alpha = if png_transparent_sample(&info.transparency, 0) == Some(gray) {
                0
            } else {
                255
            };
            let gray = png_sample_to_u8(gray, info.bit_depth);
            Ok((gray, gray, gray, alpha))
        }
        png::ColorType::Indexed => {
            let index = read_png_sample(row, x, info.bit_depth).ok_or_else(invalid)? as usize;
            let palette = &info.transparency;
            let alpha = if index < palette.len() { palette[index] } else { 255 };
            let entry = info
                .transparency
                .get(index * 3..index * 3 + 3)
                .or_else(|| info.transparency.get(..3))
                .ok_or_else(invalid)?;
            Ok((entry[0], entry[1], entry[2], alpha))
        }
        png::ColorType::Rgb => {
            let red = read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?;
            let green = read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?;
            let blue = read_png_sample(row, sample + 2, info.bit_depth).ok_or_else(invalid)?;
            let transparent = png_transparent_sample(&info.transparency, 0) == Some(red)
                && png_transparent_sample(&info.transparency, 2) == Some(green)
                && png_transparent_sample(&info.transparency, 4) == Some(blue);
            let alpha = if transparent { 0 } else { 255 };
            Ok((
                png_sample_to_u8(red, info.bit_depth),
                png_sample_to_u8(green, info.bit_depth),
                png_sample_to_u8(blue, info.bit_depth),
                alpha,
            ))
        }
        png::ColorType::GrayscaleAlpha => {
            let gray = read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?;
            let alpha = read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?;
            let gray = png_sample_to_u8(gray, info.bit_depth);
            Ok((gray, gray, gray, png_sample_to_u8(alpha, info.bit_depth)))
        }
        png::ColorType::Rgba => {
            let red = read_png_sample(row, sample, info.bit_depth).ok_or_else(invalid)?;
            let green = read_png_sample(row, sample + 1, info.bit_depth).ok_or_else(invalid)?;
            let blue = read_png_sample(row, sample + 2, info.bit_depth).ok_or_else(invalid)?;
            let alpha = read_png_sample(row, sample + 3, info.bit_depth).ok_or_else(invalid)?;
            Ok((
                png_sample_to_u8(red, info.bit_depth),
                png_sample_to_u8(green, info.bit_depth),
                png_sample_to_u8(blue, info.bit_depth),
                png_sample_to_u8(alpha, info.bit_depth),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Unfilter (undo PNG row filter)
// ---------------------------------------------------------------------------

fn unfilter_png_row(
    filter: u8,
    bytes_per_pixel: usize,
    current: &mut [u8],
    previous: &[u8],
    row_length: usize,
) -> IoResult<()> {
    match filter {
        0 => {} // None
        1 => {
            // Sub
            for i in bytes_per_pixel..row_length {
                current[i] = current[i].wrapping_add(current[i - bytes_per_pixel]);
            }
        }
        2 => {
            // Up
            for i in 0..row_length {
                current[i] = current[i].wrapping_add(previous[i]);
            }
        }
        3 => {
            // Average
            for i in 0..row_length {
                let left = if i >= bytes_per_pixel {
                    current[i - bytes_per_pixel]
                } else {
                    0
                };
                let up = previous[i];
                current[i] = current[i].wrapping_add(((left as u16 + up as u16) / 2) as u8);
            }
        }
        4 => {
            // Paeth
            for i in 0..row_length {
                let left = if i >= bytes_per_pixel {
                    current[i - bytes_per_pixel]
                } else {
                    0
                };
                let up = previous[i];
                let up_left = if i >= bytes_per_pixel {
                    previous[i - bytes_per_pixel]
                } else {
                    0
                };
                let p = left as i16 + up as i16 - up_left as i16;
                let pa = (p - left as i16).unsigned_abs();
                let pb = (p - up as i16).unsigned_abs();
                let pc = (p - up_left as i16).unsigned_abs();
                let pr = if pa <= pb && pa <= pc {
                    left
                } else if pb <= pc {
                    up
                } else {
                    up_left
                };
                current[i] = current[i].wrapping_add(pr);
            }
        }
        _ => return Err(Error::new(ErrorKind::InvalidData, "unknown PNG row filter")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Convert one decoded & unfiltered row to RGB565 output pixels
// ---------------------------------------------------------------------------

fn png_pixel_to_rgb565(red: u8, green: u8, blue: u8, alpha: u8) -> Rgb565Pixel {
    let panel_r = (PNG_PANEL_BG >> 8) as u8 & 0xf8;
    let panel_g = (PNG_PANEL_BG >> 3) as u8 & 0xfc;
    let panel_b = (PNG_PANEL_BG << 3) as u8;
    let r = blend_png_channel(red, panel_r, alpha);
    let g = blend_png_channel(green, panel_g, alpha);
    let b = blend_png_channel(blue, panel_b, alpha);
    Rgb565Pixel(((r as u16 & 0xf8) << 8) | ((g as u16 & 0xfc) << 3) | ((b as u16) >> 3))
}

fn convert_png_row(
    info: &PngStreamInfo,
    row: &[u8],
    source_x_map: &[u16],
    output: &mut [Rgb565Pixel],
) -> IoResult<()> {
    if output.len() < source_x_map.len() {
        return Err(Error::new(ErrorKind::InvalidData, "invalid PNG output row"));
    }
    match (info.color_type, info.bit_depth) {
        (png::ColorType::Rgb, png::BitDepth::Eight) => {
            for (dst, &src_x) in output[..source_x_map.len()].iter_mut().zip(source_x_map) {
                let offset = src_x as usize * 3;
                if offset + 2 >= row.len() {
                    return Err(Error::new(ErrorKind::InvalidData, "short PNG RGB row"));
                }
                let red = row[offset];
                let green = row[offset + 1];
                let blue = row[offset + 2];
                let transparent = png_transparent_sample(&info.transparency, 0).unwrap_or(u16::MAX)
                    == red as u16
                    && png_transparent_sample(&info.transparency, 2).unwrap_or(u16::MAX)
                        == green as u16
                    && png_transparent_sample(&info.transparency, 4).unwrap_or(u16::MAX)
                        == blue as u16;
                let alpha = if transparent { 0 } else { 255 };
                *dst = png_pixel_to_rgb565(red, green, blue, alpha);
            }
        }
        (png::ColorType::Rgba, png::BitDepth::Eight) => {
            for (dst, &src_x) in output[..source_x_map.len()].iter_mut().zip(source_x_map) {
                let offset = src_x as usize * 4;
                if offset + 3 >= row.len() {
                    return Err(Error::new(ErrorKind::InvalidData, "short PNG RGBA row"));
                }
                *dst = png_pixel_to_rgb565(row[offset], row[offset + 1], row[offset + 2], row[offset + 3]);
            }
        }
        _ => {
            for (dst, &src_x) in output[..source_x_map.len()].iter_mut().zip(source_x_map) {
                let (r, g, b, a) = png_pixel(info, row, src_x as usize)?;
                *dst = png_pixel_to_rgb565(r, g, b, a);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Framebuffer writer: batch 16 complete rows, pad odd height
// ---------------------------------------------------------------------------

struct PngFramebufferWriter<'a> {
    fb: &'a mut FbFile,
    image_x: usize,
    image_y: usize,
    overlay_line: [u8; 960], // 480px * 2 bytes
}

impl<'a> PngFramebufferWriter<'a> {
    fn new(fb: &'a mut FbFile, request: &PngRenderRequest) -> Self {
        let mut overlay_line = [0u8; 960];
        // Fill the full 480px line with overlay background
        fill_rgb565(&mut overlay_line, Rgb565Pixel(PNG_OVERLAY_BG));
        // Panel area: 1px border + panel background
        let panel_start = PNG_PANEL_X * 2;
        let panel_end = (PNG_PANEL_X + PNG_PANEL_W) * 2;
        fill_rgb565(
            &mut overlay_line[panel_start..panel_end],
            Rgb565Pixel(PNG_PANEL_BG),
        );
        overlay_line[panel_start..panel_start + 2]
            .copy_from_slice(&Rgb565Pixel(PNG_PANEL_BORDER).0.to_be_bytes());
        overlay_line[panel_end - 2..panel_end]
            .copy_from_slice(&Rgb565Pixel(PNG_PANEL_BORDER).0.to_be_bytes());

        Self {
            fb,
            image_x: PNG_DISPLAY_X
                + (PNG_DISPLAY_MAX_WIDTH as usize - request.display_width as usize) / 2,
            image_y: PNG_DISPLAY_Y
                + (PNG_DISPLAY_MAX_HEIGHT as usize - request.display_height as usize) / 2,
            overlay_line,
        }
    }

    /// Write one composed scanline to the framebuffer. A 960-byte scratch row
    /// is filled from the overlay background, blended with the decoded image
    /// pixels, then sent as a single-row draw_area. This replaces a 15 KiB
    /// batch buffer that OOM'd on the heap (Vec) or overflowed the UI-thread
    /// stack (inline array) — PNG rendering now costs ~1 KiB stack only.
    fn append_full_row(&mut self, line: usize, pixels: Option<&[Rgb565Pixel]>) -> IoResult<()> {
        if line >= 480 {
            return Ok(());
        }
        let mut row = [0u8; 960];
        row.copy_from_slice(&self.overlay_line);
        if let Some(pixels) = pixels {
            let image_start = self.image_x * 2;
            for (dst, pixel) in row[image_start..].chunks_exact_mut(2).zip(pixels.iter()) {
                dst.copy_from_slice(&pixel.0.to_be_bytes());
            }
        }
        // Single-row draw_area: width covers the full 480px panel line so the
        // overlay background and border are written together with the image.
        self.fb.draw_area(&row, 0, line, 480, 1, 960).map(|_| ())
    }

    fn draw_row(&mut self, display_y: usize, pixels: &[Rgb565Pixel]) -> IoResult<()> {
        self.append_full_row(self.image_y + display_y, Some(pixels))
    }

    fn finish(self) -> IoResult<()> {
        // Nothing to flush: every row is written immediately.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Process decoded rows from the streaming buffer
// ---------------------------------------------------------------------------

fn process_png_rows(
    writer: &mut PngFramebufferWriter<'_>,
    request: &PngRenderRequest,
    info: &PngStreamInfo,
    decode_buffer: &[u8],
    region: &png::UnfilterRegion,
    processed: &mut usize,
    source_y: &mut u32,
    display_y: &mut u32,
    previous_row: &mut [u8],
    current_row: &mut [u8],
    source_x_map: &[u16],
    output: &mut [Rgb565Pixel],
) -> IoResult<()> {
    while *source_y < info.height && region.filled.saturating_sub(*processed) >= info.row_length {
        let filter = decode_buffer[*processed];
        let row_data_start = *processed + 1;
        let row_end = *processed + info.row_length;
        let row_bytes = info.row_length - 1;
        current_row.copy_from_slice(&decode_buffer[row_data_start..row_end]);
        unfilter_png_row(
            filter,
            info.filter_bytes_per_pixel,
            current_row,
            previous_row,
            row_bytes,
        )?;

        // Map source rows to display rows.  Each source row contributes to at
        // most one display row; a display row may be composed from multiple
        // source rows when the image is downscaled.
        while *display_y < request.display_height
            && *display_y as u64 * info.height as u64 / request.display_height as u64 == *source_y as u64
        {
            if output.len() < request.display_width as usize {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "PNG output buffer too small",
                ));
            }
            convert_png_row(info, current_row, source_x_map, output)?;
            writer.draw_row(*display_y as usize, &output[..request.display_width as usize])?;
            *display_y += 1;
        }

        // Swap row buffers using a fixed-size temp buffer.
        // Can't use mem::swap on unsized slices.
        let n = previous_row.len().min(current_row.len());
        let mut tmp = [0u8; 1440];
        tmp[..n].copy_from_slice(&previous_row[..n]);
        previous_row[..n].copy_from_slice(&current_row[..n]);
        current_row[..n].copy_from_slice(&tmp[..n]);
        *processed = row_end;
        *source_y += 1;
    }
    Ok(())
}

fn compact_png_decode_buffer(
    decode_buffer: &mut [u8],
    region: &mut png::UnfilterRegion,
    processed: &mut usize,
) -> bool {
    if decode_buffer.len().saturating_sub(region.filled) >= 8 * 1024 {
        return false;
    }
    let discard = region.available.min(*processed);
    if discard == 0 {
        return false;
    }
    decode_buffer.copy_within(discard..region.filled, 0);
    region.available -= discard;
    region.filled -= discard;
    *processed -= discard;
    true
}

// ---------------------------------------------------------------------------
// Main entry: render a PNG file to the framebuffer
// ---------------------------------------------------------------------------

pub(crate) fn render_png_to_framebuffer(
    fb: &mut FbFile,
    request: &PngRenderRequest,
) -> IoResult<()> {
    let mut file = std::fs::File::open(&request.path)?;
    let mut decoder = png::StreamingDecoder::new();
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let _ = decoder.set_ignore_adler32(false);

    let mut decode_buffer = [0u8; PNG_STREAM_BUFFER_SIZE];
    let mut region = png::UnfilterRegion::default();
    let mut input = [0u8; PNG_INPUT_BUFFER_SIZE];
    let mut input_length = 0usize;
    let mut input_offset = 0usize;
    let mut processed = 0usize;
    let mut source_y = 0u32;
    let mut display_y = 0u32;
    let mut stream_info: Option<PngStreamInfo> = None;
    // Static buffers to avoid heap fragmentation from repeated PNG opens.
    // Clear on every entry: ROW_A/ROW_B carry stale pixel data from the
    // previous call, and unfilter_png_row expects a zero-filled previous row
    // for the first scanline of each image (PNG filter "None" / "Sub" /
    // "Average" / "Paeth" all reference it).
    static mut ROW_A: [u8; MAX_ROW_BYTES] = [0u8; MAX_ROW_BYTES];
    static mut ROW_B: [u8; MAX_ROW_BYTES] = [0u8; MAX_ROW_BYTES];
    static mut X_MAP: [u16; MAX_DISPLAY_WIDTH] = [0u16; MAX_DISPLAY_WIDTH];
    static mut OUT_BUF: [Rgb565Pixel; MAX_DISPLAY_WIDTH] = [Rgb565Pixel(0); MAX_DISPLAY_WIDTH];
    // Reset both rows so the first scanline has a clean previous row
    unsafe {
        ROW_A.fill(0);
        ROW_B.fill(0);
    }
    let mut previous_row: &mut [u8] = &mut [];
    let mut current_row: &mut [u8] = &mut [];
    let mut source_x_map: &[u16] = &[];
    let mut output: &mut [Rgb565Pixel] = &mut [];

    let mut writer = PngFramebufferWriter::new(fb, request);

    loop {
        if input_offset == input_length {
            input_length = file.read(&mut input)?;
            input_offset = 0;
            if input_length == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "incomplete PNG file"));
            }
        }

        let previous_filled = region.filled;
        let previous_source_y = source_y;
        let (consumed, decoded) = decoder
            .update(
                &input[input_offset..input_length],
                Some(&mut region.as_buf(&mut decode_buffer)),
            )
            .map_err(png_error)?;
        input_offset += consumed;
        if let png::Decoded::ChunkBegin(_, chunk) = &decoded {
            if *chunk == png::chunk::IDAT && stream_info.is_none() {
                let info = decoder
                    .info()
                    .ok_or_else(|| Error::new(ErrorKind::InvalidData, "PNG has no header"))?;
                let parsed = PngStreamInfo::from_decoder(info)?;
                if parsed.width != request.source_width || parsed.height != request.source_height {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "PNG changed after it was selected",
                    ));
                }
                let row_bytes = parsed.row_length - 1;
                if row_bytes > MAX_ROW_BYTES {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("PNG row too large: {row_bytes} > {MAX_ROW_BYTES}"),
                    ));
                }
                // Safety: the 4 static buffers are accessed from this single
                // function (render_png_to_framebuffer) and never re-entered.
                unsafe {
                    previous_row = &mut ROW_A[..row_bytes];
                    current_row = &mut ROW_B[..row_bytes];
                    for (dst, src_x) in X_MAP.iter_mut().enumerate() {
                        *src_x =
                            (dst as u64 * parsed.width as u64 / request.display_width as u64)
                                as u16;
                    }
                    let n = request.display_width as usize;
                    source_x_map = &X_MAP[..n];
                    output = &mut OUT_BUF[..n];
                }
                stream_info = Some(parsed);
            }
        }

        if let Some(info) = stream_info.as_ref() {
            process_png_rows(
                &mut writer,
                request,
                info,
                &decode_buffer,
                &region,
                &mut processed,
                &mut source_y,
                &mut display_y,
                &mut previous_row,
                &mut current_row,
                &source_x_map,
                &mut output,
            )?;
        }

        let compacted =
            compact_png_decode_buffer(&mut decode_buffer, &mut region, &mut processed);
        if matches!(&decoded, png::Decoded::ImageDataFlushed) {
            let info = stream_info
                .as_ref()
                .ok_or_else(|| Error::new(ErrorKind::InvalidData, "PNG has no image data"))?;
            if source_y != info.height || display_y != request.display_height {
                return Err(Error::new(ErrorKind::UnexpectedEof, "incomplete PNG image"));
            }
            return writer.finish();
        }
        if matches!(&decoded, png::Decoded::ChunkComplete(chunk) if *chunk == png::chunk::IEND) {
            return Err(Error::new(ErrorKind::InvalidData, "PNG has no image data"));
        }
        if consumed == 0
            && region.filled == previous_filled
            && source_y == previous_source_y
            && !compacted
        {
            return Err(Error::new(
                ErrorKind::OutOfMemory,
                "PNG streaming buffer could not make progress",
            ));
        }
    }
}
