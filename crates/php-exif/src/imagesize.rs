// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `getimagesize()` / `getimagesizefromstring()` (ref: `ext/standard/image.c`).

use std::io::{Cursor, Read, Seek};

use php_runtime::{ArrayKey, HashTable, Value, ZArray};

use crate::avif;
use crate::budget;
use crate::diag::Diagnostic;
use crate::imagetype::{self, getimagetype, image_type_to_mime_type};
use crate::inflate::{self, Outcome};
use crate::stream::PhpStream;

/// `struct php_gfxinfo` (units are always pixels for the built-in formats;
/// registered handlers may report others).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GfxInfo {
    /// Width (in `width_unit`).
    pub width: u32,
    /// Height (in `height_unit`).
    pub height: u32,
    /// The width's unit; `None` means pixels.
    pub width_unit: Option<String>,
    /// The height's unit; `None` means pixels.
    pub height_unit: Option<String>,
    /// Bits per channel (0 = unknown, omitted from the result array).
    pub bits: u32,
    /// Number of channels (0 = unknown, omitted from the result array).
    pub channels: u32,
}

/// What `getimagesize()` produces.
#[derive(Debug, Clone)]
pub struct ImageSizeOutcome {
    /// The image type found by `php_getimagetype()`.
    pub image_type: i64,
    /// The decoded size information, `None` when PHP returns `false`.
    pub info: Option<GfxInfo>,
    /// The `$image_info` array (`APPn` markers of JPEG files), present when
    /// the second argument was passed.  PHP resets the by-reference argument
    /// to an empty array before reading.
    pub app_markers: Option<HashTable>,
    /// Warnings and notices, in order.
    pub diagnostics: Vec<Diagnostic>,
}

impl ImageSizeOutcome {
    /// The PHP return value: the 7-element array (`0`, `1`, `2`, `3`,
    /// `bits`?, `channels`?, `mime`, `width_unit`, `height_unit`) or `false`.
    pub fn to_value(&self) -> Value {
        let Some(info) = &self.info else {
            return Value::Bool(false);
        };
        let is_px = |u: &Option<String>| u.as_deref().is_none_or(|u| u == "px");
        let mut ht = HashTable::new();
        ht.insert(0i64, Value::Int(i64::from(info.width)));
        ht.insert(1i64, Value::Int(i64::from(info.height)));
        ht.insert(2i64, Value::Int(self.image_type));
        if is_px(&info.width_unit) && is_px(&info.height_unit) {
            // "%d" of the unsigned fields.
            #[allow(clippy::cast_possible_wrap)]
            let html = format!(
                "width=\"{}\" height=\"{}\"",
                info.width as i32, info.height as i32
            );
            ht.insert(3i64, Value::string(html));
        }
        if info.bits != 0 {
            ht.insert(key("bits"), Value::Int(i64::from(info.bits)));
        }
        if info.channels != 0 {
            ht.insert(key("channels"), Value::Int(i64::from(info.channels)));
        }
        ht.insert(
            key("mime"),
            Value::string(image_type_to_mime_type(self.image_type)),
        );
        ht.insert(
            key("width_unit"),
            Value::string(info.width_unit.as_deref().unwrap_or("px")),
        );
        ht.insert(
            key("height_unit"),
            Value::string(info.height_unit.as_deref().unwrap_or("px")),
        );
        Value::Array(ZArray::from_table(ht))
    }
}

fn key(s: &str) -> ArrayKey {
    ArrayKey::from_bytes(s.as_bytes())
}

/// `getimagesize($filename, &$image_info)` on an already opened stream.
/// `input` is the file name used in "Error reading from %s!" notices.
/// The SWC decompression buffers and the XBM lines are checked against
/// `memory_limit`; past it `info` is `None` and the VM raises the fatal.
pub fn getimagesize<R: Read + Seek>(reader: R, input: &[u8], want_info: bool) -> ImageSizeOutcome {
    let mut stream = PhpStream::new(reader);
    let mut diags = Vec::new();
    let mut app = want_info.then(HashTable::new);
    let image_type = getimagetype(&mut stream, input, &mut diags);
    let mut info = None;
    if !budget::exhausted() {
        info = handle(&mut stream, image_type, app.as_mut(), &mut diags);
    }
    if budget::exhausted() {
        info = None;
    }
    ImageSizeOutcome {
        image_type,
        info,
        app_markers: app,
        diagnostics: diags,
    }
}

/// `getimagesizefromstring($string, &$image_info)`.
pub fn getimagesizefromstring(data: &[u8], want_info: bool) -> ImageSizeOutcome {
    getimagesize(Cursor::new(data), data, want_info)
}

fn handle<R: Read + Seek>(
    s: &mut PhpStream<R>,
    itype: i64,
    app: Option<&mut HashTable>,
    diags: &mut Vec<Diagnostic>,
) -> Option<GfxInfo> {
    match itype {
        imagetype::IMAGETYPE_GIF => handle_gif(s),
        imagetype::IMAGETYPE_JPEG => handle_jpeg(s, app, diags),
        imagetype::IMAGETYPE_PNG => handle_png(s),
        imagetype::IMAGETYPE_SWF => handle_swf(s),
        imagetype::IMAGETYPE_SWC => handle_swc(s),
        imagetype::IMAGETYPE_PSD => handle_psd(s),
        imagetype::IMAGETYPE_BMP => handle_bmp(s),
        imagetype::IMAGETYPE_TIFF_II => handle_tiff(s, false),
        imagetype::IMAGETYPE_TIFF_MM => handle_tiff(s, true),
        imagetype::IMAGETYPE_JPC => handle_jpc(s, diags),
        imagetype::IMAGETYPE_JP2 => handle_jp2(s, diags),
        imagetype::IMAGETYPE_IFF => handle_iff(s),
        imagetype::IMAGETYPE_WBMP => get_wbmp(s, false).map(|(width, height)| GfxInfo {
            width,
            height,
            ..GfxInfo::default()
        }),
        imagetype::IMAGETYPE_XBM => get_xbm(s).map(|(width, height)| GfxInfo {
            width,
            height,
            ..GfxInfo::default()
        }),
        imagetype::IMAGETYPE_ICO => handle_ico(s),
        imagetype::IMAGETYPE_WEBP => handle_webp(s),
        imagetype::IMAGETYPE_AVIF => handle_avif(s),
        imagetype::IMAGETYPE_HEIF => {
            if s.rewind() {
                handle_avif(s)
            } else {
                None
            }
        }
        _ => crate::handlers::handler(itype).and_then(|h| (h.get_info)(s)),
    }
}

fn read_n<R: Read + Seek, const N: usize>(s: &mut PhpStream<R>) -> Option<[u8; N]> {
    let mut b = [0u8; N];
    (s.read(&mut b) == N).then_some(b)
}

fn le16(b: &[u8]) -> u32 {
    u32::from(b[0]) | u32::from(b[1]) << 8
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// `php_stream_getc()` as an `int` (`EOF` = -1).
fn getc_int<R: Read + Seek>(s: &mut PhpStream<R>) -> i32 {
    s.getc().map_or(-1, i32::from)
}

fn handle_gif<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(3) {
        return None;
    }
    let d = read_n::<R, 5>(s)?;
    Some(GfxInfo {
        width: le16(&d[0..]),
        height: le16(&d[2..]),
        bits: if d[4] & 0x80 != 0 {
            u32::from(d[4] & 0x07) + 1
        } else {
            0
        },
        channels: 3,
        ..GfxInfo::default()
    })
}

fn handle_psd<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(11) {
        return None;
    }
    let d = read_n::<R, 8>(s)?;
    Some(GfxInfo {
        height: be32(&d[0..]),
        width: be32(&d[4..]),
        ..GfxInfo::default()
    })
}

#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn handle_bmp<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(11) {
        return None;
    }
    let d = read_n::<R, 16>(s)?;
    let size = le32(&d[0..]) as i32;
    if size == 12 {
        Some(GfxInfo {
            width: le16(&d[4..]),
            height: le16(&d[6..]),
            bits: u32::from(d[11]),
            channels: 0,
            ..GfxInfo::default()
        })
    } else if size > 12 && (size <= 64 || size == 108 || size == 124) {
        Some(GfxInfo {
            width: le32(&d[4..]),
            height: (le32(&d[8..]) as i32).wrapping_abs() as u32,
            bits: le16(&d[14..]),
            channels: 0,
            ..GfxInfo::default()
        })
    } else {
        None
    }
}

/// `php_swf_get_bits()`.
fn swf_bits(buf: &[u8], pos: u32, count: u32) -> u64 {
    let mut result: u64 = 0;
    for lp in pos..pos + count {
        let byte = buf.get((lp / 8) as usize).copied().unwrap_or(0);
        let bit = u64::from((byte >> (7 - (lp % 8))) & 1);
        result = result.wrapping_add(bit << (count - (lp - pos) - 1));
    }
    result
}

#[allow(clippy::cast_possible_truncation)]
fn swf_dims(b: &[u8]) -> (u32, u32) {
    let bits = swf_bits(b, 0, 5) as u32;
    let width = swf_bits(b, 5 + bits, bits).wrapping_sub(swf_bits(b, 5, bits)) / 20;
    let height = swf_bits(b, 5 + 3 * bits, bits).wrapping_sub(swf_bits(b, 5 + 2 * bits, bits)) / 20;
    (width as u32, height as u32)
}

fn handle_swf<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(5) {
        return None;
    }
    let a = read_n::<R, 32>(s)?;
    let (width, height) = swf_dims(&a);
    Some(GfxInfo {
        width,
        height,
        ..GfxInfo::default()
    })
}

fn handle_swc<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(5) {
        return None;
    }
    let a = read_n::<R, 64>(s)?;
    let mut b = [0u8; 65];
    let first = inflate::uncompress(&a, 64);
    let ok = match first {
        Outcome::Ok(out) => {
            b[..out.len()].copy_from_slice(&out);
            true
        }
        Outcome::BufError(out) | Outcome::DataError(out) => {
            // zlib already wrote the prefix it could decode into `b`; the
            // whole file is then decompressed with growing buffers (2x ..
            // 2^15x the input size) and only its status matters.
            b[..out.len()].copy_from_slice(&out);
            if !s.seek_set(8) {
                return None;
            }
            let rest = s.read_to_end();
            if budget::exhausted() {
                return None;
            }
            // Retry with buffers of 2x .. 2^15x the input size while the
            // output does not fit (Z_BUF_ERROR).
            let mut factor: usize = 1;
            let mut ok = false;
            while factor < 1 << 15 {
                factor <<= 1;
                let size = rest.len().saturating_mul(factor);
                if !budget::reserve(size as u64) {
                    return None;
                }
                match inflate::uncompress(&rest, size) {
                    Outcome::Ok(_) => {
                        ok = true;
                        break;
                    }
                    Outcome::BufError(_) => {}
                    Outcome::DataError(_) => break,
                }
            }
            ok
        }
    };
    if !ok {
        return None;
    }
    let (width, height) = swf_dims(&b);
    Some(GfxInfo {
        width,
        height,
        ..GfxInfo::default()
    })
}

fn handle_png<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    if !s.seek_cur(8) {
        return None;
    }
    let d = read_n::<R, 9>(s)?;
    Some(GfxInfo {
        width: be32(&d[0..]),
        height: be32(&d[4..]),
        bits: u32::from(d[8]),
        channels: 0,
        ..GfxInfo::default()
    })
}

/// `php_read2()`: big-endian 16 bits, 0 on a short read.
fn read2<R: Read + Seek>(s: &mut PhpStream<R>) -> u16 {
    read_n::<R, 2>(s).map_or(0, u16::from_be_bytes)
}

/// `php_read4()`: big-endian 32 bits, 0 on a short read.
fn read4<R: Read + Seek>(s: &mut PhpStream<R>) -> u32 {
    read_n::<R, 4>(s).map_or(0, u32::from_be_bytes)
}

const M_EOI: u32 = 0xD9;
const M_SOS: u32 = 0xDA;

/// `php_next_marker()`.
fn next_marker<R: Read + Seek>(
    s: &mut PhpStream<R>,
    ff_read: bool,
    diags: &mut Vec<Diagnostic>,
) -> u32 {
    if !ff_read {
        let mut extraneous: usize = 0;
        loop {
            match s.getc() {
                None => return M_EOI,
                Some(0xFF) => break,
                Some(_) => extraneous += 1,
            }
        }
        if extraneous > 0 {
            diags.push(Diagnostic::warning(format!(
                "Corrupt JPEG data: {extraneous} extraneous bytes before marker"
            )));
        }
    }
    loop {
        match s.getc() {
            None => return M_EOI,
            Some(0xFF) => {}
            Some(m) => return u32::from(m),
        }
    }
}

/// `php_skip_variable()`.
fn skip_variable<R: Read + Seek>(s: &mut PhpStream<R>) -> bool {
    let length = i128::from(read2(s));
    if length < 2 {
        return false;
    }
    s.seek_cur(length - 2);
    true
}

/// `php_read_APP()`.
fn read_app<R: Read + Seek>(s: &mut PhpStream<R>, marker: u32, info: &mut HashTable) -> bool {
    let length = usize::from(read2(s));
    if length < 2 {
        return false;
    }
    let length = length - 2;
    let buf = s.read_vec(length);
    if buf.len() != length {
        return false;
    }
    let name = format!("APP{}", marker - 0xE0);
    let k = key(&name);
    if !info.contains_key(&k) {
        info.insert(k, Value::string(buf.as_slice()));
    }
    true
}

#[allow(clippy::cast_sign_loss)]
fn handle_jpeg<R: Read + Seek>(
    s: &mut PhpStream<R>,
    mut info: Option<&mut HashTable>,
    diags: &mut Vec<Diagnostic>,
) -> Option<GfxInfo> {
    let mut result: Option<GfxInfo> = None;
    let mut ff_read = true;
    loop {
        let marker = next_marker(s, ff_read, diags);
        ff_read = false;
        match marker {
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                if result.is_none() {
                    let length = read2(s);
                    let bits = getc_int(s) as u32;
                    let height = u32::from(read2(s));
                    let width = u32::from(read2(s));
                    let channels = getc_int(s) as u32;
                    result = Some(GfxInfo {
                        width,
                        height,
                        bits,
                        channels,
                        ..GfxInfo::default()
                    });
                    if info.is_none() || length < 8 {
                        return result;
                    }
                    if !s.seek_cur(i128::from(length) - 8) {
                        return result;
                    }
                } else if !skip_variable(s) {
                    return result;
                }
            }
            0xE0..=0xEF => {
                if let Some(app) = info.as_deref_mut() {
                    if !read_app(s, marker, app) {
                        return result;
                    }
                } else if !skip_variable(s) {
                    return result;
                }
            }
            M_SOS | M_EOI => return result,
            _ => {
                if !skip_variable(s) {
                    return result;
                }
            }
        }
    }
}

#[allow(clippy::cast_sign_loss)]
fn handle_jpc<R: Read + Seek>(
    s: &mut PhpStream<R>,
    diags: &mut Vec<Diagnostic>,
) -> Option<GfxInfo> {
    // (unsigned char) of php_stream_getc(): EOF becomes 0xFF.
    let first = s.getc().unwrap_or(0xFF);
    if first != 0x51 {
        diags.push(Diagnostic::warning(
            "JPEG2000 codestream corrupt(Expected SIZ marker not found after SOC)",
        ));
        return None;
    }
    read2(s);
    read2(s);
    let width = read4(s);
    let height = read4(s);
    if !s.seek_cur(24) {
        return None;
    }
    let channels = u32::from(read2(s));
    if (channels == 0 && s.eof()) || channels > 256 {
        return None;
    }
    let mut highest = 0i32;
    for _ in 0..channels {
        let depth = getc_int(s) + 1;
        highest = highest.max(depth);
        getc_int(s);
        getc_int(s);
    }
    Some(GfxInfo {
        width,
        height,
        bits: highest as u32,
        channels,
        ..GfxInfo::default()
    })
}

#[allow(clippy::cast_possible_wrap)]
fn handle_jp2<R: Read + Seek>(
    s: &mut PhpStream<R>,
    diags: &mut Vec<Diagnostic>,
) -> Option<GfxInfo> {
    let mut result = None;
    loop {
        let box_length = read4(s);
        let Some(box_type) = read_n::<R, 4>(s) else {
            break;
        };
        if box_length == 1 {
            return None;
        }
        if &box_type == b"jp2c" {
            s.seek_cur(3);
            result = handle_jpc(s, diags);
            break;
        }
        if box_length as i32 <= 0 {
            break;
        }
        if !s.seek_cur(i128::from(box_length.wrapping_sub(8))) {
            break;
        }
    }
    if result.is_none() {
        diags.push(Diagnostic::warning(
            "JP2 file has no codestreams at root level",
        ));
    }
    result
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
fn handle_tiff<R: Read + Seek>(s: &mut PhpStream<R>, mi: bool) -> Option<GfxInfo> {
    let get16 = |b: &[u8]| -> u32 {
        if mi {
            u32::from(b[0]) << 8 | u32::from(b[1])
        } else {
            u32::from(b[1]) << 8 | u32::from(b[0])
        }
    };
    let get32 = |b: &[u8]| -> u32 {
        if mi {
            be32(b)
        } else {
            le32(b)
        }
    };
    let ptr = read_n::<R, 4>(s)?;
    let ifd_addr = u64::from(get32(&ptr));
    // `ifd_addr - 8` is computed in size_t and passed as a zend_off_t.
    if !s.seek_cur(i128::from(ifd_addr.wrapping_sub(8) as i64)) {
        return None;
    }
    let head = read_n::<R, 2>(s)?;
    let num_entries = get16(&head) as usize;
    let dir_size = 2 + 12 * num_entries + 4;
    let data = s.read_vec(dir_size - 2);
    if data.len() != dir_size - 2 {
        return None;
    }
    let (mut width, mut height) = (0u64, 0u64);
    for i in 0..num_entries {
        let e = &data[i * 12..i * 12 + 12];
        let tag = get16(e);
        let value: u64 = match get16(&e[2..]) {
            1 | 6 => u64::from(e[8]),
            3 => u64::from(get16(&e[8..])),
            8 => i64::from(get16(&e[8..]) as u16 as i16) as u64,
            4 => u64::from(get32(&e[8..])),
            9 => i64::from(get32(&e[8..]) as i32) as u64,
            _ => continue,
        };
        match tag {
            0x0100 | 0xA002 => width = value,
            0x0101 | 0xA003 => height = value,
            _ => {}
        }
    }
    if width != 0 && height != 0 {
        Some(GfxInfo {
            width: width as u32,
            height: height as u32,
            ..GfxInfo::default()
        })
    } else {
        None
    }
}

fn handle_iff<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    let a = read_n::<R, 8>(s)?;
    if &a[4..8] != b"ILBM" && &a[4..8] != b"PBM " {
        return None;
    }
    loop {
        let a = read_n::<R, 8>(s)?;
        let chunk_id = be32(&a[0..]);
        let mut size = i32::from_be_bytes([a[4], a[5], a[6], a[7]]);
        if size < 0 {
            return None;
        }
        if size & 1 == 1 {
            if size == i32::MAX {
                return None;
            }
            size += 1;
        }
        if chunk_id == 0x424d_4844 {
            if size < 9 {
                return None;
            }
            let a = read_n::<R, 9>(s)?;
            let width = i16::from_be_bytes([a[0], a[1]]);
            let height = i16::from_be_bytes([a[2], a[3]]);
            let bits = i16::from(a[8]);
            if width > 0 && height > 0 && bits > 0 && bits < 33 {
                return Some(GfxInfo {
                    width: width.unsigned_abs().into(),
                    height: height.unsigned_abs().into(),
                    bits: bits.unsigned_abs().into(),
                    channels: 0,
                    ..GfxInfo::default()
                });
            }
        } else if !s.seek_cur(i128::from(size)) {
            return None;
        }
    }
}

/// `php_get_wbmp()`: `Some((width, height))` for a plausible WBMP header.
pub(crate) fn get_wbmp<R: Read + Seek>(s: &mut PhpStream<R>, _check: bool) -> Option<(u32, u32)> {
    if !s.rewind() {
        return None;
    }
    if s.getc()? != 0 {
        return None;
    }
    loop {
        let i = s.getc()?;
        if i & 0x80 == 0 {
            break;
        }
    }
    let mut dims = [0u32; 2];
    for d in &mut dims {
        loop {
            let i = s.getc()?;
            *d = (*d << 7) | u32::from(i & 0x7F);
            if *d > 2048 {
                return None;
            }
            if i & 0x80 == 0 {
                break;
            }
        }
    }
    if dims[0] == 0 || dims[1] == 0 {
        return None;
    }
    Some((dims[0], dims[1]))
}

/// `sscanf(line, "#define %s %d", name, &value) == 2`.
fn scan_define(line: &[u8]) -> Option<(&[u8], i32)> {
    let ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C);
    let rest = line.strip_prefix(b"#define")?;
    let mut i = 0;
    while i < rest.len() && ws(rest[i]) {
        i += 1;
    }
    let start = i;
    while i < rest.len() && !ws(rest[i]) {
        i += 1;
    }
    if i == start {
        return None;
    }
    let name = &rest[start..i];
    while i < rest.len() && ws(rest[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < rest.len() && (rest[i] == b'+' || rest[i] == b'-') {
        neg = rest[i] == b'-';
        i += 1;
    }
    let dstart = i;
    let mut v: i64 = 0;
    let mut overflow = false;
    while i < rest.len() && rest[i].is_ascii_digit() {
        if let Some(x) = v
            .checked_mul(10)
            .and_then(|x| x.checked_add(i64::from(rest[i] - b'0')))
        {
            v = x;
        } else {
            overflow = true;
            v = i64::MAX;
        }
        i += 1;
    }
    if i == dstart {
        return None;
    }
    let v = if overflow {
        if neg {
            i64::MIN
        } else {
            i64::MAX
        }
    } else if neg {
        -v
    } else {
        v
    };
    // strtol's long result stored into an int.
    #[allow(clippy::cast_possible_truncation)]
    Some((name, v as i32))
}

/// `php_get_xbm()`: `Some((width, height))` when both `#define`s are found.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn get_xbm<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<(u32, u32)> {
    if !s.rewind() {
        return None;
    }
    let (mut width, mut height) = (0u32, 0u32);
    while let Some(line) = s.gets() {
        if budget::exhausted() {
            return None;
        }
        // The line is a C string: it ends at the first NUL.
        let line = match line.iter().position(|&b| b == 0) {
            Some(p) => &line[..p],
            None => &line[..],
        };
        if let Some((name, value)) = scan_define(line) {
            let ty = match name.iter().rposition(|&b| b == b'_') {
                Some(p) => &name[p + 1..],
                None => name,
            };
            if ty == b"width" {
                width = value as u32;
                if height != 0 {
                    break;
                }
            }
            if ty == b"height" {
                height = value as u32;
                if width != 0 {
                    break;
                }
            }
        }
    }
    (width != 0 && height != 0).then_some((width, height))
}

fn handle_ico<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    let d = read_n::<R, 2>(s)?;
    let num = le16(&d);
    if !(1..=255).contains(&num) {
        return None;
    }
    let mut r = GfxInfo::default();
    for _ in 0..num {
        let Some(d) = read_n::<R, 16>(s) else {
            break;
        };
        let bits = le16(&d[6..]);
        if bits >= r.bits {
            r.width = u32::from(d[0]);
            r.height = u32::from(d[1]);
            r.bits = bits;
        }
    }
    if r.width == 0 {
        r.width = 256;
    }
    if r.height == 0 {
        r.height = 256;
    }
    Some(r)
}

fn handle_webp<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    let b = read_n::<R, 18>(s)?;
    if &b[..3] != b"VP8" {
        return None;
    }
    let b = b.map(u32::from);
    let (width, height) = match b[3] {
        0x20 => (b[14] + ((b[15] & 0x3F) << 8), b[16] + ((b[17] & 0x3F) << 8)),
        0x4C => (
            b[9] + ((b[10] & 0x3F) << 8) + 1,
            (b[10] >> 6) + (b[11] << 2) + ((b[12] & 0xF) << 10) + 1,
        ),
        0x58 => (
            b[12] + (b[13] << 8) + (b[14] << 16) + 1,
            b[15] + (b[16] << 8) + (b[17] << 16) + 1,
        ),
        _ => return None,
    };
    Some(GfxInfo {
        width,
        height,
        bits: 8,
        channels: 0,
        ..GfxInfo::default()
    })
}

fn handle_avif<R: Read + Seek>(s: &mut PhpStream<R>) -> Option<GfxInfo> {
    avif::get_features(s).map(|f| GfxInfo {
        width: f.width,
        height: f.height,
        bits: f.bit_depth,
        channels: f.num_channels,
        ..GfxInfo::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_define_matches_sscanf() {
        assert_eq!(
            scan_define(b"#define foo_width 75\n"),
            Some((&b"foo_width"[..], 75))
        );
        assert_eq!(
            scan_define(b"#define  x_height\t-3"),
            Some((&b"x_height"[..], -3))
        );
        assert_eq!(scan_define(b"#define x"), None);
        assert_eq!(scan_define(b" #define x 1"), None);
        assert_eq!(scan_define(b"#define x y"), None);
    }

    #[test]
    fn gif_png_from_string() {
        let gif = b"GIF89a\x02\x00\x03\x00\x80\x00\x00";
        let o = getimagesizefromstring(gif, false);
        assert_eq!(o.image_type, imagetype::IMAGETYPE_GIF);
        assert_eq!(
            o.info,
            Some(GfxInfo {
                width: 2,
                height: 3,
                bits: 1,
                channels: 3,
                ..GfxInfo::default()
            })
        );
        let o = getimagesizefromstring(b"", true);
        assert!(o.info.is_none());
        assert_eq!(o.diagnostics[0].message, b"Error reading from !");
        assert!(o.app_markers.is_some_and(|h| h.is_empty()));
    }
}
