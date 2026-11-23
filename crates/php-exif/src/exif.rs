// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `exif_read_data()` / `exif_thumbnail()` engine: a port of
//! `ext/exif/exif.c` (php-src master), including its quirks.
//!
//! The parser works on a [`PhpStream`]; all offsets that C computes with
//! pointers are computed here with `i128` indices into the buffer they refer
//! to, so every bounds check of the hardened C code is kept and nothing can
//! read out of bounds.

// The C code relies on implicit integer conversions (int <-> unsigned <->
// size_t truncation and sign extension) which are reproduced on purpose.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_lossless,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::struct_excessive_bools,
    // Kept close to the C structure (one arm per C case, C variable names).
    clippy::match_same_arms,
    clippy::many_single_char_names
)]

use std::borrow::Cow;
use std::io::{Read, Seek};

use php_runtime::{ArrayKey, HashTable, Value, ZArray};

use crate::budget;
use crate::diag::{Diagnostic, Level};
use crate::encoding::EncodingConverter;
use crate::fmt::{hex_u32, hex_u64, pad_left, php_f};
use crate::imagetype::{
    image_type_to_mime_type, IMAGETYPE_HEIF, IMAGETYPE_JPEG, IMAGETYPE_TIFF_II, IMAGETYPE_TIFF_MM,
    IMAGETYPE_UNKNOWN, IMAGETYPE_WEBP,
};
use crate::stream::PhpStream;
use crate::tags::{
    TagTable, TAG_TABLE_GPS, TAG_TABLE_IFD, TAG_TABLE_IOP, TAG_TABLE_VND_CANON,
    TAG_TABLE_VND_CASIO, TAG_TABLE_VND_DJI, TAG_TABLE_VND_FUJI, TAG_TABLE_VND_KYOCERA,
    TAG_TABLE_VND_MINOLTA, TAG_TABLE_VND_NIKON, TAG_TABLE_VND_NIKON_990, TAG_TABLE_VND_OLYMPUS,
    TAG_TABLE_VND_PANASONIC, TAG_TABLE_VND_PENTAX, TAG_TABLE_VND_RICOH, TAG_TABLE_VND_SAMSUNG,
    TAG_TABLE_VND_SIGMA, TAG_TABLE_VND_SONY,
};

const MAX_IFD_NESTING_LEVEL: i32 = 10;
const MAX_IFD_TAGS: i32 = 1000;
const EXIF_MAX_ERRORS: i32 = 10;

const ERROR_FILEEOF: &str = "Unexpected end of file reached";
const ERROR_CORRUPT: &str = "File structure corrupted";
const ERROR_THUMBEOF: &str = "Thumbnail goes IFD boundary or end of file reached";

/// The warning `exif_read_data()`/`exif_thumbnail()` raise when the file
/// cannot be opened (after the stream layer's own warning).
pub const UNABLE_TO_OPEN: &str = "Unable to open file";

const BYTES_PER_FORMAT: [u64; 14] = [0, 1, 1, 2, 4, 8, 1, 1, 2, 4, 8, 4, 8, 1];
const NUM_FORMATS: u32 = 13;

const FMT_BYTE: u16 = 1;
const FMT_STRING: u16 = 2;
const FMT_USHORT: u16 = 3;
const FMT_ULONG: u16 = 4;
const FMT_URATIONAL: u16 = 5;
const FMT_SBYTE: u16 = 6;
const FMT_UNDEFINED: u16 = 7;
const FMT_SSHORT: u16 = 8;
const FMT_SLONG: u16 = 9;
const FMT_SRATIONAL: u16 = 10;
const FMT_SINGLE: u16 = 11;
const FMT_DOUBLE: u16 = 12;
const FMT_IFD: u16 = 13;

/// `TAG_NONE` (-1) stored in an `unsigned short`.
const TAG_NONE: u16 = 0xFFFF;
/// `TAG_COMPUTED_VALUE` (-2) stored in an `unsigned short`.
const TAG_COMPUTED_VALUE: u16 = 0xFFFE;

const TAG_IMAGEWIDTH: u32 = 0x0100;
const TAG_IMAGEHEIGHT: u32 = 0x0101;
const TAG_PHOTOMETRIC_INTERPRETATION: u32 = 0x0106;
const TAG_MAKE: u32 = 0x010F;
const TAG_MODEL: u32 = 0x0110;
const TAG_STRIP_OFFSETS: u32 = 0x0111;
const TAG_STRIP_BYTE_COUNTS: u32 = 0x0117;
const TAG_SUB_IFD: u32 = 0x014A;
const TAG_JPEG_INTERCHANGE_FORMAT: u32 = 0x0201;
const TAG_JPEG_INTERCHANGE_FORMAT_LEN: u32 = 0x0202;
const TAG_COPYRIGHT: u32 = 0x8298;
const TAG_EXPOSURETIME: u32 = 0x829A;
const TAG_FNUMBER: u32 = 0x829D;
const TAG_EXIF_IFD_POINTER: u32 = 0x8769;
const TAG_GPS_IFD_POINTER: u32 = 0x8825;
const TAG_SHUTTERSPEED: u32 = 0x9201;
const TAG_APERTURE: u32 = 0x9202;
const TAG_MAX_APERTURE: u32 = 0x9205;
const TAG_SUBJECT_DISTANCE: u32 = 0x9206;
const TAG_MAKER_NOTE: u32 = 0x927C;
const TAG_USERCOMMENT: u32 = 0x9286;
const TAG_XP_TITLE: u32 = 0x9C9B;
const TAG_XP_SUBJECT: u32 = 0x9C9F;
const TAG_COMP_IMAGE_WIDTH: u32 = 0xA002;
const TAG_COMP_IMAGE_HEIGHT: u32 = 0xA003;
const TAG_INTEROP_IFD_POINTER: u32 = 0xA005;
const TAG_FOCALPLANE_X_RES: u32 = 0xA20E;
const TAG_FOCALPLANE_RESOLUTION_UNIT: u32 = 0xA210;

/// Section indices (`SECTION_*`).
pub mod section {
    /// `FILE`.
    pub const FILE: usize = 0;
    /// `COMPUTED`.
    pub const COMPUTED: usize = 1;
    /// `ANY_TAG`.
    pub const ANY_TAG: usize = 2;
    /// `IFD0`.
    pub const IFD0: usize = 3;
    /// `THUMBNAIL`.
    pub const THUMBNAIL: usize = 4;
    /// `COMMENT`.
    pub const COMMENT: usize = 5;
    /// `APP0`.
    pub const APP0: usize = 6;
    /// `EXIF`.
    pub const EXIF: usize = 7;
    /// `FPIX`.
    pub const FPIX: usize = 8;
    /// `GPS`.
    pub const GPS: usize = 9;
    /// `INTEROP`.
    pub const INTEROP: usize = 10;
    /// `APP12`.
    pub const APP12: usize = 11;
    /// `WINXP`.
    pub const WINXP: usize = 12;
    /// `MAKERNOTE`.
    pub const MAKERNOTE: usize = 13;
    /// Number of sections.
    pub const COUNT: usize = 14;
}
use section as sec;

const FOUND_COMPUTED: u32 = 1 << sec::COMPUTED;
const FOUND_FILE: u32 = 1 << sec::FILE;
const FOUND_ANY_TAG: u32 = 1 << sec::ANY_TAG;
const FOUND_IFD0: u32 = 1 << sec::IFD0;
const FOUND_THUMBNAIL: u32 = 1 << sec::THUMBNAIL;
const FOUND_EXIF: u32 = 1 << sec::EXIF;
const FOUND_GPS: u32 = 1 << sec::GPS;
const FOUND_INTEROP: u32 = 1 << sec::INTEROP;
const FOUND_WINXP: u32 = 1 << sec::WINXP;
const FOUND_MAKERNOTE: u32 = 1 << sec::MAKERNOTE;

/// `exif_get_sectionname()`.
pub fn section_name(section: usize) -> &'static str {
    const NAMES: [&str; sec::COUNT] = [
        "FILE",
        "COMPUTED",
        "ANY_TAG",
        "IFD0",
        "THUMBNAIL",
        "COMMENT",
        "APP0",
        "EXIF",
        "FPIX",
        "GPS",
        "INTEROP",
        "APP12",
        "WINXP",
        "MAKERNOTE",
    ];
    NAMES.get(section).copied().unwrap_or("")
}

fn tag_table(section: usize) -> TagTable {
    match section {
        sec::GPS => TAG_TABLE_GPS,
        sec::INTEROP => TAG_TABLE_IOP,
        _ => TAG_TABLE_IFD,
    }
}

fn lookup_tag(tag: u32, table: TagTable) -> Option<&'static str> {
    table
        .iter()
        .find(|(t, _)| u32::from(*t) == tag)
        .map(|(_, n)| *n)
}

fn tagname_debug(tag: u32, table: TagTable) -> &'static str {
    lookup_tag(tag, table).unwrap_or("UndefinedTag")
}

fn tagname_key(tag: u32, table: TagTable) -> Vec<u8> {
    match lookup_tag(tag, table) {
        Some(n) => n.as_bytes().to_vec(),
        None => format!("UndefinedTag:0x{tag:04X}").into_bytes(),
    }
}

/// `exif_tagname()`: the IFD tag name, `None` (PHP `false`) when unknown or
/// negative.
pub fn exif_tagname(index: i64) -> Option<&'static str> {
    // The C function takes an `int`; the index lookup sign-extends it.
    let tag = index as i32;
    if index < 0 || tag < 0 {
        return None;
    }
    lookup_tag(tag as u32, TAG_TABLE_IFD)
}

// ---------------------------------------------------------------------------
// Maker notes.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MnOrder {
    Intel,
    Motorola,
    Normal,
}

struct MakerNote {
    table: Option<TagTable>,
    make: &'static [u8],
    id: Option<&'static [u8]>,
    id_len: usize,
    offset: usize,
    order: MnOrder,
    offset_maker: bool,
}

macro_rules! mn {
    ($t:expr, $make:expr, $id:expr, $idl:expr, $off:expr, $ord:ident, $mode:expr) => {
        MakerNote {
            table: $t,
            make: $make,
            id: $id,
            id_len: $idl,
            offset: $off,
            order: MnOrder::$ord,
            offset_maker: $mode,
        }
    };
}

static MAKER_NOTES: &[MakerNote] = &[
    mn!(
        Some(TAG_TABLE_VND_CANON),
        b"Canon",
        None,
        0,
        0,
        Intel,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_CASIO),
        b"CASIO",
        None,
        0,
        0,
        Motorola,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_FUJI),
        b"FUJIFILM",
        Some(b"FUJIFILM\x0C\x00\x00\x00\x00"),
        12,
        12,
        Intel,
        true
    ),
    mn!(
        Some(TAG_TABLE_VND_NIKON),
        b"NIKON",
        Some(b"Nikon\x00\x01\x00\x00"),
        8,
        8,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_NIKON_990),
        b"NIKON",
        None,
        0,
        0,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_OLYMPUS),
        b"OLYMPUS OPTICAL CO.,LTD",
        Some(b"OLYMP\x00\x01\x00\x00"),
        8,
        8,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_SAMSUNG),
        b"SAMSUNG",
        None,
        0,
        0,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_PANASONIC),
        b"Panasonic",
        Some(b"Panasonic\x00\x00\x00\x00"),
        12,
        12,
        Normal,
        false
    ),
    mn!(
        None,
        b"DJI",
        Some(b"[ae_dbg_info:\x00"),
        13,
        13,
        Motorola,
        false
    ),
    mn!(Some(TAG_TABLE_VND_DJI), b"DJI", None, 0, 0, Normal, false),
    mn!(
        Some(TAG_TABLE_VND_SONY),
        b"SONY",
        Some(b"SONY DSC \x00\x00\x00\x00"),
        12,
        12,
        Normal,
        false
    ),
    mn!(Some(TAG_TABLE_VND_SONY), b"SONY", None, 0, 0, Normal, false),
    mn!(
        Some(TAG_TABLE_VND_PENTAX),
        b"PENTAX",
        Some(b"AOC\x00\x00"),
        6,
        6,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_MINOLTA),
        b"Minolta, KONICA MINOLTA",
        None,
        0,
        0,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_SIGMA),
        b"SIGMA, FOVEON",
        Some(b"SIGMA\x00\x00\x00\x00"),
        10,
        10,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_SIGMA),
        b"SIGMA, FOVEON",
        Some(b"FOVEON\x00\x00\x00\x00"),
        10,
        10,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_KYOCERA),
        b"KYOCERA, CONTAX",
        Some(b"KYOCERA            \x00\x00\x00\x00"),
        22,
        22,
        Normal,
        true
    ),
    mn!(
        Some(TAG_TABLE_VND_RICOH),
        b"RICOH",
        Some(b"Ricoh\x00"),
        5,
        5,
        Motorola,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_RICOH),
        b"RICOH",
        Some(b"RICOH\x00"),
        5,
        5,
        Motorola,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_OLYMPUS),
        b"AGFA",
        Some(b"AGFA \x00\x01\x00"),
        8,
        8,
        Normal,
        false
    ),
    mn!(
        Some(TAG_TABLE_VND_OLYMPUS),
        b"EPSON",
        Some(b"EPSON\x00\x01\x00\x00"),
        8,
        8,
        Normal,
        false
    ),
];

/// C `strncmp(a, b, n) != 0` over NUL-terminated data (`a` always contains
/// a NUL within `n` bytes or is `n` long).
fn strncmp_differs(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return true;
        }
        if x == 0 {
            return false;
        }
    }
    false
}

fn cstr(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(p) => &b[..p],
        None => b,
    }
}

// ---------------------------------------------------------------------------
// Byte-order helpers (`php_ifd_get*`).

fn get16u(b: &[u8], mi: bool) -> u32 {
    if mi {
        u32::from(b[0]) << 8 | u32::from(b[1])
    } else {
        u32::from(b[1]) << 8 | u32::from(b[0])
    }
}

fn get16s(b: &[u8], mi: bool) -> i16 {
    get16u(b, mi) as u16 as i16
}

fn get32u(b: &[u8], mi: bool) -> u32 {
    let a = [b[0], b[1], b[2], b[3]];
    if mi {
        u32::from_be_bytes(a)
    } else {
        u32::from_le_bytes(a)
    }
}

fn get64u(b: &[u8], mi: bool) -> u64 {
    let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
    if mi {
        u64::from_be_bytes(a)
    } else {
        u64::from_le_bytes(a)
    }
}

/// `php_ifd_get_float()`: always little endian (sic).
fn get_float(b: &[u8]) -> f32 {
    f32::from_bits(get32u(b, false))
}

/// `php_ifd_get_double()`: always little endian (sic).
fn get_double(b: &[u8]) -> f64 {
    f64::from_bits(get64u(b, false))
}

fn set16u(d: &mut [u8], v: u32, mi: bool) {
    let b = (v as u16).to_be_bytes();
    if mi {
        d[..2].copy_from_slice(&b);
    } else {
        d[0] = b[1];
        d[1] = b[0];
    }
}

fn set32u(d: &mut [u8], v: u64, mi: bool) {
    let v = v as u32;
    if mi {
        d[..4].copy_from_slice(&v.to_be_bytes());
    } else {
        d[..4].copy_from_slice(&v.to_le_bytes());
    }
}

fn bpf(format: u16) -> u64 {
    BYTES_PER_FORMAT
        .get(usize::from(format))
        .copied()
        .unwrap_or(0)
}

/// `exif_convert_any_format()`.
fn convert_any_format(v: &[u8], format: u16, mi: bool) -> f64 {
    match format {
        FMT_SBYTE => f64::from(v[0] as i8),
        FMT_BYTE => f64::from(v[0]),
        FMT_USHORT => f64::from(get16u(v, mi)),
        FMT_ULONG => f64::from(get32u(v, mi)),
        FMT_URATIONAL => {
            let den = get32u(&v[4..], mi);
            if den == 0 {
                0.0
            } else {
                f64::from(get32u(v, mi)) / f64::from(den)
            }
        }
        FMT_SRATIONAL => {
            let den = get32u(&v[4..], mi) as i32;
            if den == 0 {
                0.0
            } else {
                // php_ifd_get32s() returns `unsigned`.
                f64::from(get32u(v, mi)) / f64::from(den)
            }
        }
        FMT_SSHORT => f64::from(get16s(v, mi)),
        FMT_SLONG => f64::from(get32u(v, mi)),
        FMT_SINGLE => f64::from(get_float(v)),
        FMT_DOUBLE => get_double(v),
        _ => 0.0,
    }
}

/// `exif_rewrite_tag_format_to_unsigned()`.
fn to_unsigned_format(format: u16) -> u16 {
    match format {
        FMT_SBYTE => FMT_BYTE,
        FMT_SRATIONAL => FMT_URATIONAL,
        FMT_SSHORT => FMT_USHORT,
        FMT_SLONG => FMT_ULONG,
        f => f,
    }
}

/// `exif_convert_any_to_int()` (returns a `size_t`).
fn convert_any_to_int(v: &[u8], format: u16, mi: bool) -> u64 {
    match format {
        FMT_SBYTE => i64::from(v[0] as i8) as u64,
        FMT_BYTE => u64::from(v[0]),
        FMT_USHORT | FMT_SSHORT => u64::from(get16u(v, mi)),
        FMT_ULONG | FMT_SLONG => u64::from(get32u(v, mi)),
        FMT_URATIONAL => u64::from(get32u(v, mi).checked_div(get32u(&v[4..], mi)).unwrap_or(0)),
        FMT_SRATIONAL => {
            let num = get32u(v, mi) as i32;
            let den = get32u(&v[4..], mi) as i32;
            if den == 0 {
                0
            } else if num == i32::MIN && den == -1 {
                i32::MAX as u64
            } else {
                i64::from(num / den) as u64
            }
        }
        // Saturating conversions (NaN and negatives become 0).
        FMT_SINGLE => get_float(v) as u64,
        FMT_DOUBLE => get_double(v) as u64,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Configuration.

/// The `exif.*` INI settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExifConfig {
    /// `exif.encode_unicode` (default `ISO-8859-15`).
    pub encode_unicode: String,
    /// `exif.decode_unicode_motorola` (default `UCS-2BE`).
    pub decode_unicode_motorola: String,
    /// `exif.decode_unicode_intel` (default `UCS-2LE`).
    pub decode_unicode_intel: String,
    /// `exif.encode_jis` (default empty: JIS comments are left untouched).
    pub encode_jis: String,
    /// `exif.decode_jis_motorola` (default `JIS`).
    pub decode_jis_motorola: String,
    /// `exif.decode_jis_intel` (default `JIS`).
    pub decode_jis_intel: String,
}

/// The INI entries with their defaults, in registration order.
pub const INI_ENTRIES: &[(&str, &str)] = &[
    ("exif.encode_unicode", "ISO-8859-15"),
    ("exif.decode_unicode_motorola", "UCS-2BE"),
    ("exif.decode_unicode_intel", "UCS-2LE"),
    ("exif.encode_jis", ""),
    ("exif.decode_jis_motorola", "JIS"),
    ("exif.decode_jis_intel", "JIS"),
];

impl Default for ExifConfig {
    fn default() -> Self {
        Self {
            encode_unicode: "ISO-8859-15".into(),
            decode_unicode_motorola: "UCS-2BE".into(),
            decode_unicode_intel: "UCS-2LE".into(),
            encode_jis: String::new(),
            decode_jis_motorola: "JIS".into(),
            decode_jis_intel: "JIS".into(),
        }
    }
}

impl ExifConfig {
    /// Applies an `ini_set()` of one of the [`INI_ENTRIES`].  Returns
    /// `Ok(false)` for a name that is not an exif setting and the
    /// `"Illegal encoding ignored: '%s'"` warning (the update then fails and
    /// the old value is kept) for an unknown encoding.  The encode settings
    /// accept the empty string; the decode settings validate it too.
    pub fn set(
        &mut self,
        name: &str,
        value: &str,
        conv: &dyn EncodingConverter,
    ) -> Result<bool, Diagnostic> {
        let (slot, is_encode) = match name {
            "exif.encode_unicode" => (&mut self.encode_unicode, true),
            "exif.decode_unicode_motorola" => (&mut self.decode_unicode_motorola, false),
            "exif.decode_unicode_intel" => (&mut self.decode_unicode_intel, false),
            "exif.encode_jis" => (&mut self.encode_jis, true),
            "exif.decode_jis_motorola" => (&mut self.decode_jis_motorola, false),
            "exif.decode_jis_intel" => (&mut self.decode_jis_intel, false),
            _ => return Ok(false),
        };
        if (!is_encode || !value.is_empty()) && !encoding_list_valid(value, conv) {
            return Err(Diagnostic::warning(format!(
                "Illegal encoding ignored: '{value}'"
            )));
        }
        value.clone_into(slot);
        Ok(true)
    }

    /// The current value of an exif INI setting.
    pub fn get(&self, name: &str) -> Option<&str> {
        Some(match name {
            "exif.encode_unicode" => &self.encode_unicode,
            "exif.decode_unicode_motorola" => &self.decode_unicode_motorola,
            "exif.decode_unicode_intel" => &self.decode_unicode_intel,
            "exif.encode_jis" => &self.encode_jis,
            "exif.decode_jis_motorola" => &self.decode_jis_motorola,
            "exif.decode_jis_intel" => &self.decode_jis_intel,
            _ => return None,
        })
    }
}

/// `zend_multibyte_parse_encoding_list()` succeeding: a comma-separated list
/// of known encodings (an empty list fails).
fn encoding_list_valid(value: &str, conv: &dyn EncodingConverter) -> bool {
    let mut any = false;
    for part in value.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if !p.eq_ignore_ascii_case("auto") && !conv.is_known(p) {
            return false;
        }
        any = true;
    }
    any
}

// ---------------------------------------------------------------------------
// Image info storage.

#[derive(Debug, Clone, Copy)]
enum Num {
    U(u32),
    I(i32),
    R(u32, u32),
    F(f32),
    D(f64),
}

#[derive(Debug, Clone)]
enum IifValue {
    None,
    Bytes(Vec<u8>),
    Nums(Vec<Num>),
}

#[derive(Debug, Clone)]
struct IifData {
    tag: u16,
    format: u16,
    length: u32,
    name: Vec<u8>,
    value: IifValue,
}

#[derive(Debug, Default)]
struct Thumbnail {
    filetype: i64,
    width: u64,
    height: u64,
    size: u64,
    offset: u64,
    data: Option<Vec<u8>>,
}

/// Where the image comes from (what `exif_read_from_impl()` learns from
/// the stream).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamOrigin {
    /// A plain-file stream whose `orig_path` could be `stat()`ed: the
    /// `FILE` section gets the base name, the modification time and the
    /// size from the stat.  `regular == false` makes exif fail with "Not a
    /// file".
    File {
        /// The path the stream was opened with.
        path: Vec<u8>,
        /// `st_mtime`.
        mtime: i64,
        /// `st_size`.
        size: u64,
        /// `S_ISREG(st_mode)`.
        regular: bool,
    },
    /// A plain-file stream without a usable path: the size stays 0.
    UnstatableFile,
    /// Any other stream (memory, temp, data:, ...): the size is found by
    /// seeking to the end and there is no `FileName`.
    Other,
}

/// `php_basename()` for the common case (no suffix).
fn basename(path: &[u8]) -> Vec<u8> {
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return if path.is_empty() {
            Vec::new()
        } else {
            b"/".to_vec()
        };
    }
    let start = path[..end]
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |p| p + 1);
    path[start..end].to_vec()
}

/// Bounds of the memory an IFD's offsets refer to (`exif_offset_info`).
#[derive(Clone, Copy)]
struct OffsetInfo<'a> {
    buf: &'a [u8],
    base: i128,
    start: i128,
    end: i128,
}

impl<'a> OffsetInfo<'a> {
    fn new(buf: &'a [u8], base: i128, start: usize, len: usize) -> Self {
        let start = start.min(buf.len());
        let end = (start + len).min(buf.len());
        Self {
            buf,
            base,
            start: start as i128,
            end: end as i128,
        }
    }

    /// `exif_offset_info_try_get()`: index of `base + offset` when `length`
    /// bytes there are valid.
    fn try_get(&self, offset: u64, length: u64) -> Option<usize> {
        let s = self.base + i128::from(offset);
        let e = s + i128::from(length);
        (s >= self.start && e <= self.end).then_some(s as usize)
    }

    /// `exif_offset_info_contains()`.
    fn contains(&self, start: i128, length: u64) -> bool {
        start >= self.start && start + i128::from(length) <= self.end
    }
}

struct ImageInfo<'c, R: Read + Seek> {
    stream: PhpStream<R>,
    conv: &'c dyn EncodingConverter,
    cfg: &'c ExifConfig,
    diags: Vec<Diagnostic>,

    file_name: Option<Vec<u8>>,
    file_date_time: i64,
    file_size: u64,
    file_type: i64,
    height: i32,
    width: i32,
    is_color: i32,
    make: Option<Vec<u8>>,
    model: Option<Vec<u8>>,
    aperture_f_number: f32,
    exposure_time: f32,
    focalplane_units: f64,
    ccd_width: f32,
    focalplane_x_res: f64,
    exif_image_width: u64,
    focal_length: f32,
    distance: f32,
    motorola_intel: i32,
    user_comment: Option<Vec<u8>>,
    user_comment_length: i32,
    user_comment_encoding: Option<Vec<u8>>,
    copyright: Option<Vec<u8>>,
    copyright_photographer: Option<Vec<u8>>,
    copyright_editor: Option<Vec<u8>>,
    xp_fields: Vec<(u32, Option<Vec<u8>>)>,
    thumbnail: Thumbnail,
    sections_found: u32,
    info_list: Vec<Vec<IifData>>,
    read_thumbnail: bool,
    read_all: bool,
    ifd_nesting_level: i32,
    ifd_count: i32,
    num_errors: i32,
}

impl<'c, R: Read + Seek> ImageInfo<'c, R> {
    fn new(stream: PhpStream<R>, cfg: &'c ExifConfig, conv: &'c dyn EncodingConverter) -> Self {
        Self {
            stream,
            conv,
            cfg,
            diags: Vec::new(),
            file_name: None,
            file_date_time: 0,
            file_size: 0,
            file_type: IMAGETYPE_UNKNOWN,
            height: 0,
            width: 0,
            is_color: 0,
            make: None,
            model: None,
            aperture_f_number: 0.0,
            exposure_time: 0.0,
            focalplane_units: 0.0,
            ccd_width: 0.0,
            focalplane_x_res: 0.0,
            exif_image_width: 0,
            focal_length: 0.0,
            distance: 0.0,
            motorola_intel: -1,
            user_comment: None,
            user_comment_length: 0,
            user_comment_encoding: None,
            copyright: None,
            copyright_photographer: None,
            copyright_editor: None,
            xp_fields: Vec::new(),
            thumbnail: Thumbnail::default(),
            sections_found: 0,
            info_list: vec![Vec::new(); sec::COUNT],
            read_thumbnail: false,
            read_all: false,
            ifd_nesting_level: 0,
            ifd_count: 0,
            num_errors: 0,
        }
    }

    fn mi(&self) -> bool {
        self.motorola_intel != 0
    }

    /// `exif_error_docref()` with its error-count limit.
    fn error(&mut self, level: Level, msg: impl Into<Vec<u8>>) {
        // Once the memory limit is hit PHP has bailed out: nothing follows.
        if budget::exhausted() {
            return;
        }
        self.num_errors += 1;
        if self.num_errors > EXIF_MAX_ERRORS {
            if self.num_errors == EXIF_MAX_ERRORS + 1 {
                self.diags.push(Diagnostic {
                    level,
                    message: b"Further exif parsing errors have been suppressed".to_vec(),
                });
            }
            return;
        }
        self.diags.push(Diagnostic {
            level,
            message: msg.into(),
        });
    }

    fn warn(&mut self, msg: impl Into<Vec<u8>>) {
        self.error(Level::Warning, msg);
    }

    fn notice(&mut self, msg: impl Into<Vec<u8>>) {
        self.error(Level::Notice, msg);
    }

    // -- image_info_list -------------------------------------------------

    /// `exif_iif_add_value()`.
    fn iif_add_value(
        &mut self,
        section: usize,
        name: &[u8],
        tag: u16,
        format: u16,
        length: u64,
        value: Option<&[u8]>,
        value_len: u64,
        mi: bool,
    ) {
        let mut data = IifData {
            tag,
            format,
            length: length as u32,
            name: cstr(name).to_vec(),
            value: IifValue::None,
        };
        let mut value = value;
        match format {
            FMT_STRING => {
                if length > value_len {
                    self.warn(format!("length > value_len: {length} > {value_len}"));
                    value = None;
                }
                if let Some(v) = value {
                    let n = (length as usize).min(v.len());
                    let s = cstr(&v[..n]).to_vec();
                    data.length = s.len() as u32;
                    data.value = IifValue::Bytes(s);
                } else {
                    data.length = 0;
                    data.value = IifValue::Bytes(Vec::new());
                }
            }
            FMT_USHORT | FMT_ULONG | FMT_URATIONAL | FMT_SSHORT | FMT_SLONG | FMT_SRATIONAL
            | FMT_SINGLE | FMT_DOUBLE => {
                if length != 0 {
                    let size = bpf(format) as usize;
                    let zero = match format {
                        FMT_USHORT | FMT_ULONG => Num::U(0),
                        FMT_URATIONAL | FMT_SRATIONAL => Num::R(0, 0),
                        FMT_SINGLE => Num::F(0.0),
                        FMT_DOUBLE => Num::D(0.0),
                        _ => Num::I(0),
                    };
                    let v = value.unwrap_or(&[]);
                    let avail = (value_len as usize).min(v.len());
                    let mut list = Vec::with_capacity((length as usize).min(avail / size + 1));
                    let mut short = false;
                    for idx in 0..length as usize {
                        let off = idx * size;
                        if short || avail < off + size {
                            if !short {
                                self.warn("Value too short");
                                short = true;
                            }
                            // The C list stays uninitialised; use zeros.
                            list.push(zero);
                            continue;
                        }
                        let p = &v[off..];
                        list.push(match format {
                            FMT_USHORT => Num::U(get16u(p, mi)),
                            FMT_ULONG => Num::U(get32u(p, mi)),
                            FMT_URATIONAL | FMT_SRATIONAL => {
                                Num::R(get32u(p, mi), get32u(&p[4..], mi))
                            }
                            FMT_SSHORT => Num::I(i32::from(get16s(p, mi))),
                            FMT_SLONG => Num::I(get32u(p, mi) as i32),
                            FMT_SINGLE => Num::F(get_float(p)),
                            _ => Num::D(get_double(p)),
                        });
                    }
                    data.value = IifValue::Nums(list);
                }
            }
            _ => {
                if !matches!(format, FMT_BYTE | FMT_SBYTE | FMT_UNDEFINED) {
                    // "otherwise not freed from memory" (sic: sets the tag).
                    data.tag = FMT_UNDEFINED;
                }
                if format == FMT_UNDEFINED || length != 0 {
                    if length > value_len {
                        self.warn(format!("length > value_len: {length} > {value_len}"));
                        value = None;
                    }
                    if let Some(v) = value {
                        let mut n = (length as usize).min(v.len());
                        if u32::from(tag) == TAG_MAKER_NOTE {
                            n = cstr(&v[..n]).len();
                        }
                        data.value = IifValue::Bytes(v[..n].to_vec());
                        data.length = n as u32;
                    } else {
                        data.length = 0;
                        data.value = IifValue::Bytes(Vec::new());
                    }
                }
            }
        }
        if budget::exhausted() {
            return;
        }
        self.info_list[section].push(data);
        self.sections_found |= 1 << section;
    }

    fn iif_add_tag(
        &mut self,
        section: usize,
        name: &[u8],
        tag: u16,
        format: u16,
        length: u64,
        value: Option<&[u8]>,
        value_len: u64,
    ) {
        let mi = self.mi();
        self.iif_add_value(section, name, tag, format, length, value, value_len, mi);
    }

    fn iif_add_int(&mut self, section: usize, name: &str, value: i32) {
        self.info_list[section].push(IifData {
            tag: TAG_NONE,
            format: FMT_SLONG,
            length: 1,
            name: name.as_bytes().to_vec(),
            value: IifValue::Nums(vec![Num::I(value)]),
        });
        self.sections_found |= 1 << section;
    }

    fn iif_add_str(&mut self, section: usize, name: &[u8], value: Option<&[u8]>) {
        if let Some(v) = value {
            self.info_list[section].push(IifData {
                tag: TAG_NONE,
                format: FMT_STRING,
                length: 1,
                name: cstr(name).to_vec(),
                value: IifValue::Bytes(cstr(v).to_vec()),
            });
            self.sections_found |= 1 << section;
        }
    }

    fn iif_add_buffer(&mut self, section: usize, name: &str, length: i32, value: Option<&[u8]>) {
        if let Some(v) = value {
            let n = (length.max(0) as usize).min(v.len());
            self.info_list[section].push(IifData {
                tag: TAG_NONE,
                format: FMT_UNDEFINED,
                length: length as u32,
                name: name.as_bytes().to_vec(),
                value: IifValue::Bytes(v[..n].to_vec()),
            });
            self.sections_found |= 1 << section;
        }
    }

    // -- JPEG ------------------------------------------------------------

    fn process_com(&mut self, data: &[u8]) {
        let len = data.len() as u64 - 2;
        self.iif_add_tag(
            sec::COMMENT,
            b"Comment",
            TAG_COMPUTED_VALUE,
            FMT_STRING,
            len,
            Some(&data[2..]),
            len,
        );
    }

    fn process_app12(&mut self, buf: &[u8]) {
        let length = buf.len();
        let l1 = cstr(&buf[2..]).len();
        if l1 > 0 {
            self.iif_add_tag(
                sec::APP12,
                b"Company",
                TAG_NONE,
                FMT_STRING,
                l1 as u64,
                Some(&buf[2..]),
                l1 as u64,
            );
            if length > 2 + l1 + 1 {
                let rest = &buf[2 + l1 + 1..];
                let l2 = cstr(rest).len();
                self.iif_add_tag(
                    sec::APP12,
                    b"Info",
                    TAG_NONE,
                    FMT_STRING,
                    l2 as u64,
                    Some(rest),
                    l2 as u64,
                );
            }
        }
    }

    fn process_app1(&mut self, buf: &[u8], displacement: u64) {
        if buf.len() <= 8 || &buf[2..8] != b"Exif\0\0" {
            self.warn("Incorrect APP1 Exif Identifier Code");
            return;
        }
        self.process_tiff_in_jpeg(&buf[8..], displacement + 8);
    }

    fn process_tiff_in_jpeg(&mut self, buf: &[u8], displacement: u64) {
        let length = buf.len();
        if length < 2 {
            self.warn("Missing TIFF alignment marker");
            return;
        }
        if &buf[..2] == b"II" {
            self.motorola_intel = 0;
        } else if &buf[..2] == b"MM" {
            self.motorola_intel = 1;
        } else {
            self.warn("Invalid TIFF alignment marker");
            return;
        }
        if length < 8 {
            self.warn("Invalid TIFF start (1)");
            return;
        }
        let mi = self.mi();
        let value_2a = get16u(&buf[2..], mi);
        let offset_of_ifd = get32u(&buf[4..], mi);
        if value_2a != 0x2a || offset_of_ifd < 8 {
            self.warn("Invalid TIFF start (1)");
            return;
        }
        if offset_of_ifd as usize > length {
            self.warn("Invalid IFD start");
            return;
        }
        self.sections_found |= FOUND_IFD0;
        let info = OffsetInfo::new(buf, 0, 0, length);
        self.process_ifd_in_jpeg(i128::from(offset_of_ifd), &info, displacement, sec::IFD0, 0);
        if self.focalplane_x_res != 0.0 {
            self.ccd_width = (self.exif_image_width as f64 * self.focalplane_units
                / self.focalplane_x_res) as f32;
        }
    }

    fn scan_jpeg_header(&mut self) -> bool {
        const M_PSEUDO: i32 = 0x123;
        const M_COM: i32 = 0xFE;
        let mut last_marker = M_PSEUDO;
        let mut comment_correction = 1;
        loop {
            if budget::exhausted() {
                return false;
            }
            if last_marker == M_COM && comment_correction != 0 {
                comment_correction = 2;
            }
            let mut marker;
            loop {
                let Some(c) = self.stream.getc() else {
                    self.warn(ERROR_CORRUPT);
                    return false;
                };
                marker = i32::from(c);
                if last_marker == M_COM && comment_correction > 0 {
                    if marker == 0xFF {
                        last_marker = M_PSEUDO;
                    } else {
                        marker = 0xFF;
                        comment_correction -= 1;
                    }
                }
                if marker != 0xFF {
                    break;
                }
            }
            if last_marker == M_COM && comment_correction == 0 {
                self.notice(
                    "Image has corrupt COM section: some software set wrong length information",
                );
            }
            if last_marker == M_COM && comment_correction != 0 {
                return true;
            }
            let fpos = self.stream.tell();
            let Some(lh) = self.stream.getc() else {
                self.warn(ERROR_CORRUPT);
                return false;
            };
            let Some(ll) = self.stream.getc() else {
                self.warn(ERROR_CORRUPT);
                return false;
            };
            let itemlen = usize::from(lh) << 8 | usize::from(ll);
            if itemlen < 2 {
                self.warn(ERROR_CORRUPT);
                return false;
            }
            // Every section is retained (plus its file_section entry).
            if !budget::reserve(itemlen as u64 + 32) {
                return false;
            }
            let mut data = vec![0u8; itemlen];
            data[0] = lh;
            data[1] = ll;
            let got = self.stream.read(&mut data[2..]);
            if got != itemlen - 2 {
                let want = itemlen - 2;
                self.warn(format!(
                    "Error reading from file: got=x{}(={got}) != itemlen-2=x{}(={want})",
                    hex_u64(got as u64, 4),
                    hex_u64(want as u64, 4)
                ));
                return false;
            }
            match marker {
                0xDA => {
                    if self.read_all {
                        let pos = self.stream.tell();
                        let size = self.file_size.wrapping_sub(pos);
                        let rest = self
                            .stream
                            .read_vec(usize::try_from(size).unwrap_or(usize::MAX));
                        if rest.len() as u64 != size {
                            self.warn(ERROR_FILEEOF);
                            return false;
                        }
                    }
                    return true;
                }
                0xD9 => {
                    self.warn("No image in jpeg!");
                    return self.sections_found & !FOUND_COMPUTED != 0;
                }
                0xFE => self.process_com(&data),
                0xE1 if self.sections_found & FOUND_IFD0 == 0 => {
                    self.process_app1(&data, fpos);
                }
                0xEC => self.process_app12(&data),
                0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                    if itemlen - 2 < 6 {
                        return false;
                    }
                    self.height = (u32::from(data[3]) << 8 | u32::from(data[4])) as i32;
                    self.width = (u32::from(data[5]) << 8 | u32::from(data[6])) as i32;
                    self.is_color = i32::from(data[7] == 3);
                }
                _ => {}
            }
            last_marker = marker;
        }
    }

    // -- IFDs ------------------------------------------------------------

    /// `exif_process_IFD_in_JPEG()`.  `dir_start` indexes `info.buf`.
    fn process_ifd_in_jpeg(
        &mut self,
        dir_start: i128,
        info: &OffsetInfo<'_>,
        displacement: u64,
        section: usize,
        tag: u32,
    ) -> bool {
        self.sections_found |= FOUND_IFD0;
        if !info.contains(dir_start, 2) {
            self.warn("Illegal IFD size");
            return false;
        }
        let ds = dir_start as usize;
        let mi = self.mi();
        let n = get16u(&info.buf[ds..], mi) as usize;
        if !info.contains(dir_start + 2, (n * 12) as u64) {
            let rel = dir_start + 2 - info.start;
            self.warn(format!(
                "Illegal IFD size: x{} + 2 + x{}*12 = x{} > x{}",
                hex_u32(rel as u32, 4),
                hex_u32(n as u32, 4),
                hex_u32((rel + (n as i128) * 12) as u32, 4),
                hex_u32((info.end - info.start) as u32, 4)
            ));
            return false;
        }
        let table = tag_table(section);
        for de in 0..n {
            let p = ds + 2 + 12 * de;
            let entry: [u8; 12] = info.buf[p..p + 12].try_into().unwrap_or([0; 12]);
            if !self.process_ifd_tag(&entry, info, displacement, section, true, table) {
                return false;
            }
        }
        if section == sec::THUMBNAIL {
            return true;
        }
        let next_pos = dir_start + 2 + (n as i128) * 12;
        if !info.contains(next_pos, 4) {
            return true;
        }
        let mut next_dir_offset: i32 = 0;
        if tag != TAG_EXIF_IFD_POINTER && tag != TAG_GPS_IFD_POINTER {
            next_dir_offset = get32u(&info.buf[next_pos as usize..], self.mi()) as i32;
        }
        if next_dir_offset != 0 {
            // An `int` converted to `size_t`.
            let Some(next) = info.try_get(i64::from(next_dir_offset) as u64, 0) else {
                self.warn("Illegal IFD offset");
                return false;
            };
            if self.process_ifd_in_jpeg(next as i128, info, displacement, sec::THUMBNAIL, 0) {
                if self.thumbnail.filetype != IMAGETYPE_UNKNOWN
                    && self.thumbnail.size != 0
                    && self.thumbnail.offset != 0
                    && self.read_thumbnail
                {
                    self.thumbnail_extract(info);
                }
                return true;
            }
            return false;
        }
        true
    }

    /// `exif_process_IFD_TAG()` (the depth/count guard).
    fn process_ifd_tag(
        &mut self,
        entry: &[u8; 12],
        info: &OffsetInfo<'_>,
        displacement: u64,
        section: usize,
        read_next: bool,
        table: TagTable,
    ) -> bool {
        let count = self.ifd_count;
        self.ifd_count += 1;
        if budget::exhausted() {
            return false;
        }
        if count > MAX_IFD_TAGS {
            self.warn("corrupt EXIF header: maximum IFD tag count reached");
            return false;
        }
        if self.ifd_nesting_level > MAX_IFD_NESTING_LEVEL {
            self.warn("corrupt EXIF header: maximum directory nesting level reached");
            return false;
        }
        self.ifd_nesting_level += 1;
        let r = self.process_ifd_tag_impl(entry, info, displacement, section, read_next, table);
        self.ifd_nesting_level -= 1;
        r
    }

    fn cannot_be_empty(&mut self, tag: u32, table: TagTable) -> bool {
        self.warn(format!(
            "Process tag(x{}={}): Cannot be empty",
            hex_u32(tag, 4),
            tagname_debug(tag, table)
        ));
        false
    }

    fn process_ifd_tag_impl(
        &mut self,
        entry: &[u8; 12],
        info: &OffsetInfo<'_>,
        displacement: u64,
        section: usize,
        read_next: bool,
        table: TagTable,
    ) -> bool {
        let mi = self.mi();
        let tag = get16u(entry, mi);
        let mut format = get16u(&entry[2..], mi);
        let components = get32u(&entry[4..], mi);
        if format == 0 || format > NUM_FORMATS {
            self.warn(format!(
                "Process tag(x{}={}): Illegal format code 0x{}, suppose BYTE",
                hex_u32(tag, 4),
                tagname_debug(tag, table),
                hex_u32(format, 4)
            ));
            format = u32::from(FMT_BYTE);
        }
        let format = format as u16;
        let byte_count_signed = i64::from(components) * bpf(format) as i64;
        if byte_count_signed > i64::from(i32::MAX) {
            self.warn(format!(
                "Process tag(x{}={}): Illegal byte_count",
                hex_u32(tag, 4),
                tagname_debug(tag, table)
            ));
            return false;
        }
        let byte_count = byte_count_signed as u64;

        // `value_full` holds at least `byte_count` bytes (4 for inline values).
        let mut value_full: Cow<'_, [u8]> = if byte_count > 4 {
            let offset_val = u64::from(get32u(&entry[8..], mi));
            if let Some(p) = info.try_get(offset_val, byte_count) {
                Cow::Borrowed(&info.buf[p..p + byte_count as usize])
            } else {
                let fs = self.file_size;
                if byte_count > fs
                    || offset_val > fs - byte_count
                    || !matches!(
                        self.file_type,
                        IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM | IMAGETYPE_JPEG
                    )
                {
                    self.warn(format!(
                        "Process tag(x{}={}): Illegal pointer offset(x{} + x{} = x{} > x{})",
                        hex_u32(tag, 4),
                        tagname_debug(tag, table),
                        hex_u64(offset_val, 4),
                        hex_u64(byte_count, 4),
                        hex_u64(offset_val.wrapping_add(byte_count), 4),
                        hex_u64(fs, 4)
                    ));
                    return false;
                }
                let fpos = self.stream.tell();
                let target = displacement.wrapping_add(offset_val);
                self.stream.seek_set(i128::from(target));
                let fgot = self.stream.tell();
                if fgot != target {
                    self.warn(format!(
                        "Wrong file pointer: 0x{} != 0x{}",
                        hex_u64(fgot, 8),
                        hex_u64(target, 8)
                    ));
                    return false;
                }
                if !budget::reserve(byte_count) {
                    return false;
                }
                let buf = self.stream.read_vec(byte_count as usize);
                self.stream.seek_set(i128::from(fpos));
                if buf.len() as u64 != byte_count {
                    self.warn(ERROR_FILEEOF);
                    return false;
                }
                Cow::Owned(buf)
            }
        } else {
            Cow::Owned(entry[8..12].to_vec())
        };

        self.sections_found |= FOUND_ANY_TAG;

        if section == sec::THUMBNAIL {
            if self.thumbnail.data.is_none() {
                if byte_count == 0 {
                    return self.cannot_be_empty(tag, table);
                }
                let v = &value_full[..];
                let uf = to_unsigned_format(format);
                match tag {
                    TAG_IMAGEWIDTH | TAG_COMP_IMAGE_WIDTH => {
                        self.thumbnail.width = convert_any_to_int(v, uf, mi);
                    }
                    TAG_IMAGEHEIGHT | TAG_COMP_IMAGE_HEIGHT => {
                        self.thumbnail.height = convert_any_to_int(v, uf, mi);
                    }
                    TAG_STRIP_OFFSETS | TAG_JPEG_INTERCHANGE_FORMAT => {
                        self.thumbnail.offset = convert_any_to_int(v, uf, mi);
                    }
                    TAG_STRIP_BYTE_COUNTS => {
                        self.thumbnail.filetype =
                            if matches!(self.file_type, IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM) {
                                self.file_type
                            } else {
                                IMAGETYPE_TIFF_MM
                            };
                        self.thumbnail.size = convert_any_to_int(v, uf, mi);
                    }
                    TAG_JPEG_INTERCHANGE_FORMAT_LEN
                        if self.thumbnail.filetype == IMAGETYPE_UNKNOWN =>
                    {
                        self.thumbnail.filetype = IMAGETYPE_JPEG;
                        self.thumbnail.size = convert_any_to_int(v, uf, mi);
                    }
                    _ => {}
                }
            }
        } else if section == sec::IFD0 || section == sec::EXIF {
            match tag {
                TAG_COPYRIGHT => {
                    let v = &value_full[..byte_count as usize];
                    let length = cstr(v).len();
                    if byte_count > 1 && length > 0 {
                        if (length as u64) < byte_count - 1 {
                            let photographer = v[..length].to_vec();
                            let editor = v[length + 1..].to_vec();
                            let mut c = photographer.clone();
                            c.extend_from_slice(b", ");
                            c.extend_from_slice(cstr(&editor));
                            self.copyright_photographer = Some(photographer);
                            self.copyright_editor = Some(editor);
                            self.copyright = Some(c);
                        } else {
                            self.copyright = Some(v.to_vec());
                        }
                    }
                }
                TAG_USERCOMMENT => {
                    let mut owned = value_full.into_owned();
                    let n = byte_count as usize;
                    let (comment, enc, len) = self.process_user_comment(&mut owned[..n]);
                    self.user_comment = comment;
                    self.user_comment_encoding = enc;
                    self.user_comment_length = len;
                    value_full = Cow::Owned(owned);
                }
                TAG_XP_TITLE..=TAG_XP_SUBJECT => {
                    self.sections_found |= FOUND_WINXP;
                    let v = &value_full[..byte_count as usize];
                    let from = if mi {
                        &self.cfg.decode_unicode_motorola
                    } else {
                        &self.cfg.decode_unicode_intel
                    };
                    let val = match self.conv.convert(v, &self.cfg.encode_unicode, from) {
                        Some(c) => Some(c),
                        None => string_raw(v),
                    };
                    self.xp_fields.push((tag, val));
                }
                TAG_FNUMBER => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    self.aperture_f_number = convert_any_format(&value_full, format, mi) as f32;
                }
                TAG_APERTURE | TAG_MAX_APERTURE if self.aperture_f_number == 0.0 => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    let x =
                        convert_any_format(&value_full, format, mi) * f64::from(2.0_f32.ln()) * 0.5;
                    self.aperture_f_number = (x as f32).exp();
                }
                TAG_SHUTTERSPEED if self.exposure_time == 0.0 => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    let x = -convert_any_format(&value_full, format, mi) * f64::from(2.0_f32.ln());
                    self.exposure_time = (x as f32).exp();
                }
                TAG_EXPOSURETIME => self.exposure_time = -1.0,
                TAG_COMP_IMAGE_WIDTH => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    self.exif_image_width =
                        convert_any_to_int(&value_full, to_unsigned_format(format), mi);
                }
                TAG_FOCALPLANE_X_RES => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    self.focalplane_x_res = convert_any_format(&value_full, format, mi);
                }
                TAG_SUBJECT_DISTANCE => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    self.distance = convert_any_format(&value_full, format, mi) as f32;
                }
                TAG_FOCALPLANE_RESOLUTION_UNIT => {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    match convert_any_to_int(&value_full, format, mi) {
                        1 | 2 => self.focalplane_units = 25.4,
                        3 => self.focalplane_units = 10.0,
                        4 => self.focalplane_units = 1.0,
                        5 => self.focalplane_units = 0.001,
                        _ => {}
                    }
                }
                TAG_SUB_IFD if format == FMT_IFD => {
                    self.notice("Skip SUB IFD");
                }
                TAG_MAKE => self.make = Some(value_full[..byte_count as usize].to_vec()),
                TAG_MODEL => self.model = Some(value_full[..byte_count as usize].to_vec()),
                TAG_MAKER_NOTE => {
                    let v = &value_full[..byte_count as usize];
                    if !self.process_ifd_in_makernote(v, info, displacement) {
                        return false;
                    }
                }
                TAG_EXIF_IFD_POINTER | TAG_GPS_IFD_POINTER | TAG_INTEROP_IFD_POINTER
                    if read_next =>
                {
                    if byte_count == 0 {
                        return self.cannot_be_empty(tag, table);
                    }
                    let sub = match tag {
                        TAG_EXIF_IFD_POINTER => {
                            self.sections_found |= FOUND_EXIF;
                            sec::EXIF
                        }
                        TAG_GPS_IFD_POINTER => {
                            self.sections_found |= FOUND_GPS;
                            sec::GPS
                        }
                        _ => {
                            self.sections_found |= FOUND_INTEROP;
                            sec::INTEROP
                        }
                    };
                    let offset_val = u64::from(get32u(&value_full, self.mi()));
                    let Some(sub_start) = info.try_get(offset_val, 0) else {
                        self.warn("Illegal IFD Pointer");
                        return false;
                    };
                    if !self.process_ifd_in_jpeg(sub_start as i128, info, displacement, sub, tag) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        let v = &value_full[..byte_count as usize];
        self.iif_add_tag(
            section,
            &tagname_key(tag, table),
            tag as u16,
            format,
            u64::from(components),
            Some(v),
            byte_count,
        );
        true
    }

    /// `exif_process_user_comment()`: (comment, encoding, length).
    fn process_user_comment(&self, value: &mut [u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>, i32) {
        let mut encoding = None;
        let mut start = 0usize;
        let mut byte_count = value.len() as i32;
        if byte_count >= 8 {
            let head: [u8; 8] = value[..8].try_into().unwrap_or([0; 8]);
            let (to, from) = match &head {
                b"UNICODE\0" => {
                    encoding = Some(b"UNICODE".to_vec());
                    start = 8;
                    let rest = &value[8..];
                    let from = if rest.len() >= 2 && rest[..2] == [0xFE, 0xFF] {
                        start += 2;
                        "UCS-2BE"
                    } else if rest.len() >= 2 && rest[..2] == [0xFF, 0xFE] {
                        start += 2;
                        "UCS-2LE"
                    } else if self.mi() {
                        self.cfg.decode_unicode_motorola.as_str()
                    } else {
                        self.cfg.decode_unicode_intel.as_str()
                    };
                    (Some(self.cfg.encode_unicode.as_str()), Some(from))
                }
                b"ASCII\0\0\0" => {
                    encoding = Some(b"ASCII".to_vec());
                    start = 8;
                    (None, None)
                }
                b"JIS\0\0\0\0\0" => {
                    encoding = Some(b"JIS".to_vec());
                    start = 8;
                    let from = if self.mi() {
                        self.cfg.decode_jis_motorola.as_str()
                    } else {
                        self.cfg.decode_jis_intel.as_str()
                    };
                    (Some(self.cfg.encode_jis.as_str()), Some(from))
                }
                b"\0\0\0\0\0\0\0\0" => {
                    encoding = Some(b"UNDEFINED".to_vec());
                    start = 8;
                    (None, None)
                }
                _ => (None, None),
            };
            byte_count = (value.len() - start) as i32;
            if let (Some(to), Some(from)) = (to, from) {
                let src = &value[start..];
                return if let Some(c) = self.conv.convert(src, to, from) {
                    let len = c.len() as i32;
                    (Some(c), encoding, len)
                } else {
                    let raw = string_raw(src);
                    let len = raw.as_ref().map_or(0, |r| r.len() as i32);
                    (raw, encoding, len)
                };
            }
        }
        let text = &mut value[start..];
        if byte_count > 0 {
            let mut a = (byte_count - 1) as usize;
            while a != 0 && text[a] == b' ' {
                text[a] = 0;
                a -= 1;
            }
        }
        let s = cstr(text).to_vec();
        let len = s.len() as i32;
        (Some(s), encoding, len)
    }

    /// `exif_process_IFD_in_MAKERNOTE()`.
    fn process_ifd_in_makernote(
        &mut self,
        value: &[u8],
        info: &OffsetInfo<'_>,
        displacement: u64,
    ) -> bool {
        let value_len = value.len();
        let make = self.make.as_deref().map(cstr);
        let Some(mn) = MAKER_NOTES.iter().find(|mn| {
            if make != Some(mn.make) {
                return false;
            }
            if let Some(id) = mn.id {
                if value_len >= mn.id_len && strncmp_differs(id, value, mn.id_len) {
                    return false;
                }
            }
            true
        }) else {
            return true;
        };
        if value_len < 2 || mn.offset >= value_len - 1 {
            self.warn(format!(
                "IFD data too short: 0x{} offset 0x{}",
                hex_u32(value_len as u32, 4),
                hex_u32(mn.offset as u32, 4)
            ));
            return true;
        }
        let Some(table) = mn.table else {
            return true;
        };
        let dir = &value[mn.offset..];
        self.sections_found |= FOUND_MAKERNOTE;
        let old_mi = self.motorola_intel;
        match mn.order {
            MnOrder::Intel => self.motorola_intel = 0,
            MnOrder::Motorola => self.motorola_intel = 1,
            MnOrder::Normal => {}
        }
        let mut n = get16u(dir, self.mi()) as usize;
        if 2 + n * 12 > value_len {
            self.notice("Potentially invalid endianness, trying again with different endianness before imminent failure.");
            self.motorola_intel = i32::from(self.motorola_intel == 0);
            n = get16u(dir, self.mi()) as usize;
        }
        if 2 + n * 12 > value_len {
            self.warn(format!(
                "Illegal IFD size: 2 + 0x{}*12 = 0x{} > 0x{}",
                hex_u32(n as u32, 4),
                hex_u32((2 + n * 12) as u32, 4),
                hex_u32(value_len as u32, 4)
            ));
            return false;
        }
        if mn.offset > value_len - (2 + n * 12) {
            self.warn(format!(
                "Illegal IFD size: 0x{} > 0x{}",
                hex_u32((mn.offset + 2 + n * 12) as u32, 4),
                hex_u32(value_len as u32, 4)
            ));
            return false;
        }
        let maker_info;
        let info = if mn.offset_maker {
            maker_info = OffsetInfo::new(value, 0, 0, value_len);
            &maker_info
        } else {
            info
        };
        for de in 0..n {
            let p = 2 + 12 * de;
            let entry: [u8; 12] = dir[p..p + 12].try_into().unwrap_or([0; 12]);
            if !self.process_ifd_tag(&entry, info, displacement, sec::MAKERNOTE, false, table) {
                return false;
            }
        }
        self.motorola_intel = old_mi;
        true
    }

    // -- Thumbnails ------------------------------------------------------

    fn thumbnail_extract(&mut self, info: &OffsetInfo<'_>) {
        if self.thumbnail.data.is_some() {
            self.warn("Multiple possible thumbnails");
            return;
        }
        if !self.read_thumbnail {
            return;
        }
        if self.thumbnail.size >= 65536 || self.thumbnail.size == 0 || self.thumbnail.offset == 0 {
            self.warn("Illegal thumbnail size/offset");
            return;
        }
        let Some(p) = info.try_get(self.thumbnail.offset, self.thumbnail.size) else {
            self.warn(ERROR_THUMBEOF);
            return;
        };
        if !budget::reserve(self.thumbnail.size) {
            return;
        }
        self.thumbnail.data = Some(info.buf[p..p + self.thumbnail.size as usize].to_vec());
        self.thumbnail_build();
    }

    /// `exif_thumbnail_build()`: wraps TIFF thumbnails in a TIFF header.
    fn thumbnail_build(&mut self) {
        if !self.read_thumbnail || self.thumbnail.offset == 0 || self.thumbnail.size == 0 {
            return;
        }
        if !matches!(
            self.thumbnail.filetype,
            IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM
        ) {
            return;
        }
        let Some(old) = self.thumbnail.data.take() else {
            return;
        };
        let mi = self.mi();
        let list = &self.info_list[sec::THUMBNAIL];
        let count = list.len();
        let mut new_size = 8 + 2 + count * 12 + 4;
        let mut new_value = new_size;
        for d in list {
            let bc = (bpf(d.format) * u64::from(d.length)) as usize;
            if bc > 4 {
                new_size += bc;
            }
        }
        let new_move = new_size;
        if !budget::reserve(new_size as u64) {
            return;
        }
        let mut data = vec![0u8; new_size + old.len()];
        data[new_move..].copy_from_slice(&old);
        if mi {
            data[..8].copy_from_slice(b"MM\x00\x2a\x00\x00\x00\x08");
        } else {
            data[..8].copy_from_slice(b"II\x2a\x00\x08\x00\x00\x00");
        }
        set16u(&mut data[8..], count as u32, mi);
        let mut p = 10;
        for d in list {
            let bc = (bpf(d.format) * u64::from(d.length)) as usize;
            let tag = u32::from(d.tag);
            if tag == TAG_STRIP_OFFSETS || tag == TAG_JPEG_INTERCHANGE_FORMAT {
                set16u(&mut data[p..], tag, mi);
                set16u(&mut data[p + 2..], u32::from(FMT_ULONG), mi);
                set32u(&mut data[p + 4..], 1, mi);
                set32u(&mut data[p + 8..], new_move as u64, mi);
            } else {
                set16u(&mut data[p..], tag, mi);
                set16u(&mut data[p + 2..], u32::from(d.format), mi);
                set32u(&mut data[p + 4..], u64::from(d.length), mi);
                let v = ifd_make_value(d, bc, mi);
                if bc <= 4 {
                    data[p + 8..p + 12].copy_from_slice(&v[..4]);
                } else {
                    set32u(&mut data[p + 8..], new_value as u64, mi);
                    data[new_value..new_value + bc].copy_from_slice(&v[..bc]);
                    new_value += bc;
                }
            }
            p += 12;
        }
        self.thumbnail.size += new_size as u64;
        self.thumbnail.data = Some(data);
    }

    /// `exif_scan_thumbnail()`: finds the size of a JPEG thumbnail.
    fn scan_thumbnail(&mut self) -> bool {
        let size = self.thumbnail.size as usize;
        let Some(data) = self.thumbnail.data.as_ref() else {
            return false;
        };
        if size < 4 {
            return false;
        }
        let data = &data[..size.min(data.len())];
        let size = data.len();
        if size < 4 || &data[..3] != b"\xFF\xD8\xFF" {
            if size >= 4 && self.thumbnail.width == 0 && self.thumbnail.height == 0 {
                self.warn("Thumbnail is not a JPEG image");
            }
            return false;
        }
        let mut pos = 0usize;
        let mut length = 2usize;
        loop {
            pos += length;
            if pos >= size {
                return false;
            }
            let c = data[pos];
            pos += 1;
            if pos >= size {
                return false;
            }
            if c != 0xFF {
                return false;
            }
            let mut n: i32 = 8;
            let mut c;
            loop {
                c = data[pos];
                pos += 1;
                let cont = c == 0xFF && n != 0;
                n -= 1;
                if !cont {
                    break;
                }
                if pos + 3 >= size {
                    return false;
                }
            }
            if c == 0xFF {
                return false;
            }
            let marker = c;
            if pos >= size {
                return false;
            }
            if pos + 1 >= size {
                return false;
            }
            length = usize::from(data[pos]) << 8 | usize::from(data[pos + 1]);
            if length > size || pos >= size - length {
                return false;
            }
            match marker {
                0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                    if length < 8 || size - 8 < pos {
                        return false;
                    }
                    let d = &data[pos..];
                    let h = u64::from(d[3]) << 8 | u64::from(d[4]);
                    let w = u64::from(d[5]) << 8 | u64::from(d[6]);
                    self.thumbnail.height = h;
                    self.thumbnail.width = w;
                    return true;
                }
                0xDA | 0xD9 => {
                    self.warn("Could not compute size of thumbnail");
                    return false;
                }
                _ => {}
            }
        }
    }

    fn thumbnail_in_file(&mut self) -> bool {
        if self.thumbnail.size > self.file_size
            || self.thumbnail.offset > self.file_size - self.thumbnail.size
        {
            self.warn(ERROR_THUMBEOF);
            return false;
        }
        true
    }

    fn read_tiff_thumbnail(&mut self) {
        if self.thumbnail.data.is_none() && self.thumbnail_in_file() {
            if !budget::reserve(self.thumbnail.size) {
                return;
            }
            self.stream.seek_set(i128::from(self.thumbnail.offset));
            let buf = self.stream.read_vec(self.thumbnail.size as usize);
            if buf.len() as u64 == self.thumbnail.size {
                self.thumbnail.data = Some(buf);
                self.thumbnail_build();
            } else {
                self.warn(ERROR_THUMBEOF);
            }
        }
    }

    // -- TIFF ------------------------------------------------------------

    fn process_ifd_in_tiff(&mut self, dir_offset: u64, section: usize) -> bool {
        let count = self.ifd_count;
        self.ifd_count += 1;
        if count > MAX_IFD_TAGS
            || self.ifd_nesting_level > MAX_IFD_NESTING_LEVEL
            || budget::exhausted()
        {
            return false;
        }
        self.ifd_nesting_level += 1;
        let r = self.process_ifd_in_tiff_impl(dir_offset, section);
        self.ifd_nesting_level -= 1;
        r
    }

    fn process_ifd_in_tiff_impl(&mut self, dir_offset: u64, section: usize) -> bool {
        let table = tag_table(section);
        let fs = self.file_size;
        if !(fs >= 2 && fs - 2 >= dir_offset) {
            self.warn(format!(
                "Error in TIFF: filesize(x{}) less than start of IFD dir(x{})",
                hex_u64(fs, 4),
                hex_u64(dir_offset.wrapping_add(2), 4)
            ));
            return false;
        }
        self.stream.seek_set(i128::from(dir_offset));
        let mut head = [0u8; 2];
        if self.stream.read(&mut head) != 2 {
            return false;
        }
        let mi = self.mi();
        let num_entries = get16u(&head, mi) as usize;
        let dir_size = 2 + 12 * num_entries + 4;
        if !(fs >= dir_size as u64 && fs - dir_size as u64 >= dir_offset) {
            self.warn(format!(
                "Error in TIFF: filesize(x{}) less than size of IFD dir(x{})",
                hex_u64(fs, 4),
                hex_u64(dir_offset.wrapping_add(dir_size as u64), 4)
            ));
            return false;
        }
        if !budget::reserve(dir_size as u64 + 32) {
            return false;
        }
        let mut data = vec![0u8; dir_size];
        data[..2].copy_from_slice(&head);
        if self.stream.read(&mut data[2..]) != dir_size - 2 {
            return false;
        }
        let next_offset = get32u(&data[dir_size - 4..], mi);
        let mut ifd_size = dir_size as u64;
        let mut entry_value: u64 = 0;
        for i in 0..num_entries {
            let e = &data[2 + i * 12..2 + i * 12 + 12];
            let entry_tag = get16u(e, mi);
            let mut entry_type = get16u(&e[2..], mi);
            if entry_type > NUM_FORMATS {
                self.notice(format!(
                    "Read from TIFF: tag(0x{},{}): Illegal format code 0x{}, switching to BYTE",
                    hex_u32(entry_tag, 4),
                    pad_left(tagname_debug(entry_tag, table), 12),
                    hex_u32(entry_type, 4)
                ));
                entry_type = u32::from(FMT_BYTE);
            }
            let entry_length_signed =
                i64::from(get32u(&e[4..], mi)) * bpf(entry_type as u16) as i64;
            if entry_length_signed > i64::from(i32::MAX) {
                self.warn(format!(
                    "Read from TIFF: entry length of {entry_length_signed} exceeds INT32_MAX"
                ));
                return false;
            }
            let entry_length = entry_length_signed as u64;
            if entry_length <= 4 {
                match entry_type as u16 {
                    FMT_USHORT => entry_value = u64::from(get16u(&e[8..], mi)),
                    FMT_SSHORT => entry_value = i64::from(get16s(&e[8..], mi)) as u64,
                    FMT_ULONG | FMT_SLONG => entry_value = u64::from(get32u(&e[8..], mi)),
                    _ => {}
                }
                match entry_tag {
                    TAG_IMAGEWIDTH | TAG_COMP_IMAGE_WIDTH => self.width = entry_value as i32,
                    TAG_IMAGEHEIGHT | TAG_COMP_IMAGE_HEIGHT => self.height = entry_value as i32,
                    TAG_PHOTOMETRIC_INTERPRETATION => match entry_value {
                        0 | 1 | 4 => self.is_color = 0,
                        2 | 3 | 5 | 6 | 8 => self.is_color = 1,
                        _ => {}
                    },
                    _ => {}
                }
            } else {
                let entry_offset = u64::from(get32u(&e[8..], mi));
                if entry_length_signed + entry_offset as i64 > i64::from(i32::MAX) {
                    self.warn(format!("Read from TIFF: entry length of {entry_length_signed} exceeds INT32_MAX when combined with entry offset"));
                    return false;
                }
                if entry_offset + entry_length > dir_offset + ifd_size
                    && entry_offset == dir_offset + ifd_size
                {
                    ifd_size = entry_offset + entry_length - dir_offset;
                }
            }
        }
        // `ImageInfo->file.list[sn].size` is still `dir_size` here.
        if ifd_size > dir_size as u64 {
            if fs < ifd_size || dir_offset > fs - ifd_size {
                self.warn(format!(
                    "Error in TIFF: filesize(x{}) less than size of IFD(x{} + x{})",
                    hex_u64(fs, 4),
                    hex_u64(dir_offset, 4),
                    hex_u64(ifd_size, 4)
                ));
                return false;
            }
            if !budget::reserve(ifd_size - dir_size as u64) {
                return false;
            }
            data.resize(ifd_size as usize, 0);
            self.stream.read(&mut data[dir_size..]);
        }
        for i in 0..num_entries {
            let e: [u8; 12] = data[2 + i * 12..2 + i * 12 + 12]
                .try_into()
                .unwrap_or([0; 12]);
            let mi = self.mi();
            let entry_tag = get16u(&e, mi);
            if matches!(
                entry_tag,
                TAG_EXIF_IFD_POINTER | TAG_INTEROP_IFD_POINTER | TAG_GPS_IFD_POINTER | TAG_SUB_IFD
            ) {
                let sub = match entry_tag {
                    TAG_EXIF_IFD_POINTER => {
                        self.sections_found |= FOUND_EXIF;
                        sec::EXIF
                    }
                    TAG_GPS_IFD_POINTER => {
                        self.sections_found |= FOUND_GPS;
                        sec::GPS
                    }
                    TAG_INTEROP_IFD_POINTER => {
                        self.sections_found |= FOUND_INTEROP;
                        sec::INTEROP
                    }
                    _ => {
                        self.sections_found |= FOUND_THUMBNAIL;
                        sec::THUMBNAIL
                    }
                };
                let entry_offset = u64::from(get32u(&e[8..], mi));
                self.process_ifd_in_tiff(entry_offset, sub);
                if section != sec::THUMBNAIL
                    && entry_tag == TAG_SUB_IFD
                    && self.thumbnail.filetype != IMAGETYPE_UNKNOWN
                    && self.thumbnail.size != 0
                    && self.thumbnail.offset != 0
                    && self.read_thumbnail
                {
                    self.read_tiff_thumbnail();
                }
            } else {
                let info = OffsetInfo::new(&data, -i128::from(dir_offset), 0, ifd_size as usize);
                if !self.process_ifd_tag(&e, &info, 0, section, false, table) {
                    return false;
                }
            }
        }
        if next_offset != 0 && section != sec::THUMBNAIL {
            self.process_ifd_in_tiff(u64::from(next_offset), sec::THUMBNAIL);
            if self.thumbnail.data.is_none()
                && self.thumbnail.offset != 0
                && self.thumbnail.size != 0
                && self.read_thumbnail
            {
                self.read_tiff_thumbnail();
            }
        }
        true
    }

    // -- HEIF / WebP -----------------------------------------------------

    fn scan_heif_header(&mut self, buf: &mut [u8; 16]) -> bool {
        let fs = self.file_size;
        let mut offset = u64::from(get32u(buf, true));
        let mut ret = false;
        while fs.wrapping_sub(16) > offset {
            if !self.stream.seek_set(i128::from(offset)) || self.stream.read(buf) != 16 {
                break;
            }
            let (hsize, bsize, btype) = isobmff_box(buf);
            if bsize < hsize as u64 {
                break;
            }
            if btype == *b"meta" {
                let limit = bsize - hsize as u64;
                if limit < 36 || limit > fs {
                    break;
                }
                if !budget::reserve(limit) {
                    break;
                }
                let mut data = vec![0u8; limit as usize];
                let remain = 16 - hsize;
                data[..remain].copy_from_slice(&buf[hsize..16]);
                let mut pos = (0u32, 0u32);
                if self.stream.read(&mut data[remain..]) == limit as usize - remain {
                    pos = isobmff_parse_meta(&data);
                }
                let (poff, psize) = (u64::from(pos.0), u64::from(pos.1));
                if psize >= 2
                    && psize < fs
                    && fs - psize >= poff
                    && self.stream.seek_set(i128::from(poff + 2))
                {
                    let lim = (psize - 2) as usize;
                    let d = self.stream.read_vec(lim);
                    if d.len() == lim {
                        self.process_app1(&d, poff + 2);
                        ret = true;
                    }
                }
                break;
            }
            if offset.checked_add(bsize).is_none() {
                break;
            }
            offset += bsize;
        }
        ret
    }

    fn scan_webp_header(&mut self, riff_size: u64) -> bool {
        let fs = self.file_size;
        let mut offset: u64 = 12;
        let riff_end = if riff_size <= fs - 8 {
            riff_size + 8
        } else {
            fs
        };
        while offset + 8 <= riff_end {
            let mut hdr = [0u8; 8];
            if !self.stream.seek_set(i128::from(offset)) || self.stream.read(&mut hdr) != 8 {
                return false;
            }
            let chunk_size = u64::from(get32u(&hdr[4..], false));
            let payload = offset + 8;
            if chunk_size > riff_end - payload {
                return false;
            }
            if &hdr[..4] == b"EXIF" {
                if chunk_size < 8 {
                    return false;
                }
                let data = self.stream.read_vec(chunk_size as usize);
                if data.len() as u64 != chunk_size {
                    return false;
                }
                let skip = if chunk_size >= 14 && &data[..6] == b"Exif\0\0" {
                    6
                } else {
                    0
                };
                self.process_tiff_in_jpeg(&data[skip..], payload + skip as u64);
                return true;
            }
            offset = payload + chunk_size + (chunk_size & 1);
        }
        false
    }

    fn scan_file_header(&mut self) -> bool {
        self.file_type = IMAGETYPE_UNKNOWN;
        if self.file_size < 2 {
            self.warn(format!("File too small ({})", self.file_size));
            return false;
        }
        self.stream.seek_set(0);
        let mut hdr = [0u8; 16];
        if self.stream.read(&mut hdr[..2]) != 2 {
            return false;
        }
        if hdr[0] == 0xFF && hdr[1] == 0xD8 {
            self.file_type = IMAGETYPE_JPEG;
            if self.scan_jpeg_header() {
                return true;
            }
            self.warn("Invalid JPEG file");
            return false;
        }
        if self.file_size < 8 {
            return false;
        }
        if self.stream.read(&mut hdr[2..8]) != 6 {
            return false;
        }
        if &hdr[..4] == b"II\x2a\x00" || &hdr[..4] == b"MM\x00\x2a" {
            let intel = hdr[0] == b'I';
            self.file_type = if intel {
                IMAGETYPE_TIFF_II
            } else {
                IMAGETYPE_TIFF_MM
            };
            self.motorola_intel = i32::from(!intel);
            self.sections_found |= FOUND_IFD0;
            let off = u64::from(get32u(&hdr[4..], !intel));
            if self.process_ifd_in_tiff(off, sec::IFD0) {
                return true;
            }
            self.warn("Invalid TIFF file");
            return false;
        }
        if self.file_size > 16 && &hdr[4..8] == b"ftyp" && self.stream.read(&mut hdr[8..12]) == 4 {
            let brand = &hdr[8..12];
            if brand == b"heic" || brand == b"heix" || brand == b"mif1" {
                if self.scan_heif_header(&mut hdr) {
                    self.file_type = IMAGETYPE_HEIF;
                    return true;
                }
                self.warn("Invalid HEIF file");
                return false;
            }
        }
        if self.file_size >= 16
            && &hdr[..4] == b"RIFF"
            && self.stream.read(&mut hdr[8..12]) == 4
            && &hdr[8..12] == b"WEBP"
        {
            if self.scan_webp_header(u64::from(get32u(&hdr[4..], false))) {
                self.file_type = IMAGETYPE_WEBP;
                return true;
            }
            self.warn("Invalid WebP file");
            return false;
        }
        self.warn("File not supported");
        false
    }
}

/// `exif_process_string_raw()`: the bytes plus a NUL, `None` when empty.
fn string_raw(v: &[u8]) -> Option<Vec<u8>> {
    if v.is_empty() {
        None
    } else {
        let mut r = v.to_vec();
        r.push(0);
        Some(r)
    }
}

/// `exif_ifd_make_value()`: serialises a stored value back to IFD bytes
/// (at least 4 bytes long).
fn ifd_make_value(d: &IifData, byte_count: usize, mi: bool) -> Vec<u8> {
    let mut v = vec![0u8; byte_count.max(4)];
    if d.length == 0 {
        return v;
    }
    match (&d.value, d.format) {
        (IifValue::Bytes(s), FMT_UNDEFINED | FMT_STRING | FMT_BYTE | FMT_SBYTE) => {
            let n = byte_count.min(s.len());
            v[..n].copy_from_slice(&s[..n]);
        }
        (IifValue::Nums(list), f) => {
            let mut p = 0;
            for n in list.iter().take(d.length as usize) {
                match (f, *n) {
                    (FMT_USHORT, Num::U(x)) => {
                        set16u(&mut v[p..], x, mi);
                        p += 2;
                    }
                    (FMT_SSHORT, Num::I(x)) => {
                        set16u(&mut v[p..], x as u32, mi);
                        p += 2;
                    }
                    (FMT_ULONG, Num::U(x)) => {
                        set32u(&mut v[p..], u64::from(x), mi);
                        p += 4;
                    }
                    (FMT_SLONG, Num::I(x)) => {
                        set32u(&mut v[p..], i64::from(x) as u64, mi);
                        p += 4;
                    }
                    (FMT_URATIONAL | FMT_SRATIONAL, Num::R(a, b)) => {
                        set32u(&mut v[p..], u64::from(a), mi);
                        set32u(&mut v[p + 4..], u64::from(b), mi);
                        p += 8;
                    }
                    (FMT_SINGLE, Num::F(x)) => {
                        v[p..p + 4].copy_from_slice(&x.to_le_bytes());
                        p += 4;
                    }
                    (FMT_DOUBLE, Num::D(x)) => {
                        v[p..p + 8].copy_from_slice(&x.to_le_bytes());
                        p += 8;
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    v
}

/// `exif_isobmff_parse_box()`: (header size, box size, type).
fn isobmff_box(b: &[u8]) -> (usize, u64, [u8; 4]) {
    let size = u64::from(get32u(b, true));
    let ty = [b[4], b[5], b[6], b[7]];
    if size == 1 {
        (16, get64u(&b[8..], true), ty)
    } else {
        (8, size, ty)
    }
}

/// `exif_isobmff_parse_meta()`: (offset, size) of the Exif item.
#[allow(unused_assignments)]
fn isobmff_parse_meta(data: &[u8]) -> (u32, u32) {
    let end = data.len() as i128;
    let mut pos = (0u32, 0u32);
    let mut exif_id: i32 = -1;
    let mut box_offset: i128 = 4;
    let get = |p: i128, n: usize| -> &[u8] { &data[p as usize..p as usize + n] };
    while box_offset < end - 16 {
        let (hs, bsize, btype) = isobmff_box(get(box_offset, 16));
        if bsize < hs as u64 {
            return pos;
        }
        let mut p = box_offset;
        let mut remain = (end - p) as u128;
        macro_rules! advance {
            ($n:expr) => {{
                let n = $n as u128;
                if remain < n {
                    return pos;
                }
                remain -= n;
                p += n as i128;
            }};
        }
        if &btype == b"iinf" {
            advance!(hs + 4);
            let version = data[(p - 4) as usize];
            let item_count: i32 = if version < 2 {
                advance!(2);
                get16u(get(p - 2, 2), true) as i32
            } else {
                advance!(4);
                get32u(get(p - 4, 4), true) as i32
            };
            let mut i = 0;
            while i < item_count && p < end - 20 {
                let (ihs, isize, _) = isobmff_box(get(p, 16));
                if isize < ihs as u64 {
                    return pos;
                }
                if remain < (ihs + 12) as u128 {
                    return pos;
                }
                if get(p + ihs as i128 + 8, 4) == b"Exif" {
                    exif_id = get16u(get(p + ihs as i128 + 4, 2), true) as i32;
                    break;
                }
                advance!(isize);
                i += 1;
            }
            if exif_id < 0 {
                break;
            }
        } else if &btype == b"iloc" {
            advance!(hs + 6);
            let version = data[(p - 6) as usize];
            let item_count: i32 = if version < 2 {
                advance!(2);
                get16u(get(p - 2, 2), true) as i32
            } else {
                advance!(4);
                get32u(get(p - 4, 4), true) as i32
            };
            let mut i = 0;
            while i < item_count && p < end - 16 {
                if get16u(get(p, 2), true) as i32 == exif_id {
                    pos = (get32u(get(p + 8, 4), true), get32u(get(p + 12, 4), true));
                    break;
                }
                i += 1;
                p += 16;
            }
            break;
        }
        if end - 16 - box_offset <= i128::from(bsize) {
            break;
        }
        box_offset += i128::from(bsize);
    }
    pos
}

// ---------------------------------------------------------------------------
// Result array.

fn sym_key(name: &[u8]) -> ArrayKey {
    ArrayKey::from_bytes(name)
}

/// `add_assoc_image_info()`.
fn add_assoc_image_info(out: &mut HashTable, sub_array: bool, list: &[IifData], section: usize) {
    if list.is_empty() {
        return;
    }
    let mut sub = HashTable::new();
    let target: &mut HashTable = if sub_array { &mut sub } else { out };
    let mut idx: i64 = 0;
    for d in list {
        let name = sym_key(&d.name);
        if d.length == 0 {
            target.insert(name, Value::Null);
            continue;
        }
        match d.format {
            FMT_STRING => {
                let s = match &d.value {
                    IifValue::Bytes(b) => cstr(b).to_vec(),
                    _ => Vec::new(),
                };
                if section == sec::COMMENT {
                    target.insert(idx, Value::string(s.as_slice()));
                    idx += 1;
                } else {
                    target.insert(name, Value::string(s.as_slice()));
                }
            }
            FMT_URATIONAL | FMT_SRATIONAL | FMT_USHORT | FMT_SSHORT | FMT_SINGLE | FMT_DOUBLE
            | FMT_ULONG | FMT_SLONG => {
                let IifValue::Nums(list) = &d.value else {
                    target.insert(name, Value::Null);
                    continue;
                };
                let conv = |n: &Num| -> Value {
                    match (d.format, *n) {
                        (FMT_USHORT | FMT_ULONG, Num::U(u)) => Value::Int(i64::from(u as i32)),
                        (FMT_URATIONAL, Num::R(a, b)) => Value::string(format!("{a}/{b}")),
                        (FMT_SRATIONAL, Num::R(a, b)) => {
                            Value::string(format!("{}/{}", a as i32, b as i32))
                        }
                        (FMT_SSHORT | FMT_SLONG, Num::I(i)) => Value::Int(i64::from(i)),
                        (_, Num::F(f)) => Value::Float(f64::from(f)),
                        (_, Num::D(x)) => Value::Float(x),
                        (_, Num::U(u)) => Value::Int(i64::from(u as i32)),
                        (_, Num::I(i)) => Value::Int(i64::from(i)),
                        (_, Num::R(a, b)) => Value::string(format!("{a}/{b}")),
                    }
                };
                if d.length == 1 {
                    let v = list.first().map_or(Value::Int(0), conv);
                    target.insert(name, v);
                } else {
                    let mut arr = HashTable::new();
                    for (i, n) in list.iter().enumerate() {
                        arr.insert(i as i64, conv(n));
                    }
                    target.insert(name, Value::Array(ZArray::from_table(arr)));
                }
            }
            _ => {
                let s = match &d.value {
                    IifValue::Bytes(b) => b.as_slice(),
                    _ => &[],
                };
                let n = (d.length as usize).min(s.len());
                target.insert(name, Value::string(&s[..n]));
            }
        }
    }
    if sub_array {
        out.insert(
            ArrayKey::from_bytes(section_name(section).as_bytes()),
            Value::Array(ZArray::from_table(sub)),
        );
    }
}

/// `exif_get_sectionlist()`.
fn section_list(found: u32) -> String {
    (0..sec::COUNT)
        .filter(|i| found & (1 << i) != 0)
        .map(section_name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parses the `$required_sections` argument into a `FOUND_*` mask.
fn parse_required_sections(s: &[u8]) -> u32 {
    let mut list = vec![b','];
    list.extend(cstr(s).iter().map(|&b| if b == b' ' { b',' } else { b }));
    list.push(b',');
    let mut mask = 0;
    for i in 0..sec::COUNT {
        let needle = format!(",{},", section_name(i));
        if list.windows(needle.len()).any(|w| w == needle.as_bytes()) {
            mask |= 1 << i;
        }
    }
    mask
}

// ---------------------------------------------------------------------------
// Public entry points.

/// The arguments of `exif_read_data()` after the stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReadOptions {
    /// `$required_sections` (`null` = `None`).
    pub required_sections: Option<Vec<u8>>,
    /// `$as_arrays`.
    pub as_arrays: bool,
    /// `$read_thumbnail`.
    pub read_thumbnail: bool,
}

/// The decoded thumbnail returned by [`exif_thumbnail`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbnailResult {
    /// The thumbnail bytes (the return value).
    pub data: Vec<u8>,
    /// `$width` (0 when unknown).
    pub width: i64,
    /// `$height` (0 when unknown).
    pub height: i64,
    /// `$image_type` (an `IMAGETYPE_*`).
    pub image_type: i64,
}

fn read_from_stream<'c, R: Read + Seek>(
    reader: R,
    origin: &StreamOrigin,
    read_thumbnail: bool,
    cfg: &'c ExifConfig,
    conv: &'c dyn EncodingConverter,
) -> (ImageInfo<'c, R>, bool) {
    let mut stream = PhpStream::new(reader);
    let old_pos = stream.tell();
    if old_pos != 0 {
        stream.seek_set(0);
    }
    let mut ii = ImageInfo::new(stream, cfg, conv);
    let ok = 'read: {
        match origin {
            StreamOrigin::File {
                path,
                mtime,
                size,
                regular,
            } => {
                if !regular {
                    ii.warn("Not a file");
                    break 'read false;
                }
                ii.file_name = Some(basename(path));
                ii.file_date_time = *mtime;
                ii.file_size = *size;
            }
            StreamOrigin::UnstatableFile => {}
            StreamOrigin::Other => {
                ii.file_size = ii.stream.len();
                ii.stream.seek_set(0);
            }
        }
        ii.read_thumbnail = read_thumbnail;
        ii.thumbnail.filetype = IMAGETYPE_UNKNOWN;
        ii.scan_file_header()
    };
    if old_pos != 0 {
        ii.stream.seek_set(i128::from(old_pos));
    }
    (ii, ok)
}

/// `exif_read_data($file, $required_sections, $as_arrays, $read_thumbnail)`
/// on an opened stream.  `reader` should be positioned where the PHP stream
/// is (its position is restored like PHP does; pass `&mut file`).  Returns
/// the PHP return value (`false` or the array) and the diagnostics.
pub fn exif_read_data<R: Read + Seek>(
    reader: R,
    origin: &StreamOrigin,
    opts: &ReadOptions,
    cfg: &ExifConfig,
    conv: &dyn EncodingConverter,
) -> (Value, Vec<Diagnostic>) {
    let sections_needed = opts
        .required_sections
        .as_deref()
        .map_or(0, parse_required_sections);
    let (mut ii, ok) = read_from_stream(reader, origin, opts.read_thumbnail, cfg, conv);
    let sections_str = section_list(ii.sections_found);
    ii.sections_found |= FOUND_COMPUTED | FOUND_FILE;
    if budget::exhausted() {
        let _ = ii.stream.into_inner();
        return (Value::Bool(false), ii.diags);
    }
    if !ok || (sections_needed != 0 && sections_needed & ii.sections_found == 0) {
        let _ = ii.stream.into_inner();
        return (Value::Bool(false), ii.diags);
    }

    let file_name = ii.file_name.clone();
    ii.iif_add_str(sec::FILE, b"FileName", file_name.as_deref());
    ii.iif_add_int(sec::FILE, "FileDateTime", ii.file_date_time as i32);
    ii.iif_add_int(sec::FILE, "FileSize", ii.file_size as i32);
    ii.iif_add_int(sec::FILE, "FileType", ii.file_type as i32);
    let mime = image_type_to_mime_type(ii.file_type);
    ii.iif_add_str(sec::FILE, b"MimeType", Some(mime.as_bytes()));
    ii.iif_add_str(sec::FILE, b"SectionsFound", Some(sections_str.as_bytes()));

    if ii.width > 0 && ii.height > 0 {
        let html = format!("width=\"{}\" height=\"{}\"", ii.width, ii.height);
        ii.iif_add_str(sec::COMPUTED, b"html", Some(html.as_bytes()));
        ii.iif_add_int(sec::COMPUTED, "Height", ii.height);
        ii.iif_add_int(sec::COMPUTED, "Width", ii.width);
    }
    ii.iif_add_int(sec::COMPUTED, "IsColor", ii.is_color);
    if ii.motorola_intel != -1 {
        ii.iif_add_int(sec::COMPUTED, "ByteOrderMotorola", ii.motorola_intel);
    }
    if ii.focal_length != 0.0 {
        let s = format!("{:>4}mm", php_f(f64::from(ii.focal_length), 1));
        ii.iif_add_str(sec::COMPUTED, b"FocalLength", Some(s.as_bytes()));
        if ii.ccd_width != 0.0 {
            let v = (ii.focal_length / ii.ccd_width * 35.0 + 0.5) as i32;
            ii.iif_add_str(
                sec::COMPUTED,
                b"35mmFocalLength",
                Some(format!("{v}mm").as_bytes()),
            );
        }
    }
    if ii.ccd_width != 0.0 {
        let s = format!("{}mm", ii.ccd_width as i32);
        ii.iif_add_str(sec::COMPUTED, b"CCDWidth", Some(s.as_bytes()));
    }
    if ii.exposure_time > 0.0 {
        let recip = 0.5_f32 + 1.0_f32 / ii.exposure_time;
        let et = php_f(f64::from(ii.exposure_time), 3);
        let s = if f64::from(ii.exposure_time) <= 0.5 && recip < i32::MAX as f32 {
            format!("{et} s (1/{})", recip as i32)
        } else {
            format!("{et} s")
        };
        ii.iif_add_str(sec::COMPUTED, b"ExposureTime", Some(s.as_bytes()));
    }
    if ii.aperture_f_number != 0.0 {
        let s = format!("f/{}", php_f(f64::from(ii.aperture_f_number), 1));
        ii.iif_add_str(sec::COMPUTED, b"ApertureFNumber", Some(s.as_bytes()));
    }
    if ii.distance != 0.0 {
        let s = if ii.distance < 0.0 {
            "Infinite".to_owned()
        } else {
            format!("{}m", php_f(f64::from(ii.distance), 2))
        };
        ii.iif_add_str(sec::COMPUTED, b"FocusDistance", Some(s.as_bytes()));
    }
    if let Some(uc) = ii.user_comment.take() {
        let len = ii.user_comment_length;
        ii.iif_add_buffer(sec::COMPUTED, "UserComment", len, Some(&uc));
        if let Some(enc) = ii.user_comment_encoding.take() {
            if !cstr(&enc).is_empty() {
                ii.iif_add_str(sec::COMPUTED, b"UserCommentEncoding", Some(&enc));
            }
        }
    }
    let c = ii.copyright.take();
    ii.iif_add_str(sec::COMPUTED, b"Copyright", c.as_deref());
    let c = ii.copyright_photographer.take();
    ii.iif_add_str(sec::COMPUTED, b"Copyright.Photographer", c.as_deref());
    let c = ii.copyright_editor.take();
    ii.iif_add_str(sec::COMPUTED, b"Copyright.Editor", c.as_deref());

    for (tag, value) in std::mem::take(&mut ii.xp_fields) {
        let name = tagname_debug(tag, TAG_TABLE_IFD);
        ii.iif_add_str(sec::WINXP, name.as_bytes(), value.as_deref());
    }
    if ii.thumbnail.size != 0 {
        if opts.read_thumbnail {
            let data = ii.thumbnail.data.take();
            let size = ii.thumbnail.size;
            ii.iif_add_tag(
                sec::THUMBNAIL,
                b"THUMBNAIL",
                TAG_NONE,
                FMT_UNDEFINED,
                size,
                data.as_deref(),
                size,
            );
            ii.thumbnail.data = data;
        }
        if ii.thumbnail.width == 0 || ii.thumbnail.height == 0 {
            ii.scan_thumbnail();
        }
        let ft = ii.thumbnail.filetype;
        ii.iif_add_int(sec::COMPUTED, "Thumbnail.FileType", ft as i32);
        let mime = image_type_to_mime_type(ft);
        ii.iif_add_str(sec::COMPUTED, b"Thumbnail.MimeType", Some(mime.as_bytes()));
    }
    if ii.thumbnail.width != 0 && ii.thumbnail.height != 0 {
        let (h, w) = (ii.thumbnail.height as i32, ii.thumbnail.width as i32);
        ii.iif_add_int(sec::COMPUTED, "Thumbnail.Height", h);
        ii.iif_add_int(sec::COMPUTED, "Thumbnail.Width", w);
    }

    if budget::exhausted() {
        let _ = ii.stream.into_inner();
        return (Value::Bool(false), ii.diags);
    }
    let mut out = HashTable::new();
    let arr = opts.as_arrays;
    for (section, sub) in [
        (sec::FILE, arr),
        (sec::COMPUTED, true),
        (sec::ANY_TAG, arr),
        (sec::IFD0, arr),
        (sec::THUMBNAIL, true),
        (sec::COMMENT, true),
        (sec::EXIF, arr),
        (sec::GPS, arr),
        (sec::INTEROP, arr),
        (sec::FPIX, arr),
        (sec::APP12, arr),
        (sec::WINXP, arr),
        (sec::MAKERNOTE, arr),
    ] {
        add_assoc_image_info(&mut out, sub, &ii.info_list[section], section);
    }
    let _ = ii.stream.into_inner();
    (Value::Array(ZArray::from_table(out)), ii.diags)
}

/// `exif_thumbnail($file, &$width, &$height, &$image_type)` on an opened
/// stream.  `want_dimensions` is whether `$width` or `$height` was passed
/// (the thumbnail is then scanned for its size, which may warn).
pub fn exif_thumbnail<R: Read + Seek>(
    reader: R,
    origin: &StreamOrigin,
    want_dimensions: bool,
    cfg: &ExifConfig,
    conv: &dyn EncodingConverter,
) -> (Option<ThumbnailResult>, Vec<Diagnostic>) {
    let (mut ii, ok) = read_from_stream(reader, origin, true, cfg, conv);
    if budget::exhausted() {
        let _ = ii.stream.into_inner();
        return (None, ii.diags);
    }
    if !ok || ii.thumbnail.data.is_none() || ii.thumbnail.size == 0 {
        let _ = ii.stream.into_inner();
        return (None, ii.diags);
    }
    if want_dimensions
        && (ii.thumbnail.width == 0 || ii.thumbnail.height == 0)
        && !ii.scan_thumbnail()
    {
        ii.thumbnail.width = 0;
        ii.thumbnail.height = 0;
    }
    let mut data = ii.thumbnail.data.take().unwrap_or_default();
    data.truncate(ii.thumbnail.size as usize);
    let r = ThumbnailResult {
        data,
        width: ii.thumbnail.width as i64,
        height: ii.thumbnail.height as i64,
        image_type: ii.thumbnail.filetype,
    };
    let _ = ii.stream.into_inner();
    (Some(r), ii.diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagnames() {
        assert_eq!(exif_tagname(0x010F), Some("Make"));
        assert_eq!(exif_tagname(4_294_967_567), Some("Make"));
        assert_eq!(exif_tagname(-1), None);
        assert_eq!(exif_tagname(0xFFFF), Some("No tag value"));
        assert_eq!(exif_tagname(0x10000), None);
    }

    #[test]
    fn section_parsing() {
        assert_eq!(parse_required_sections(b""), 0);
        assert_eq!(
            parse_required_sections(b"EXIF,GPS COMMENT"),
            (1 << sec::EXIF) | (1 << sec::GPS) | (1 << sec::COMMENT)
        );
        assert_eq!(parse_required_sections(b"exif"), 0);
        assert_eq!(section_list(FOUND_ANY_TAG | FOUND_IFD0), "ANY_TAG, IFD0");
    }

    #[test]
    fn basenames() {
        assert_eq!(basename(b"/a/b/c.jpg"), b"c.jpg");
        assert_eq!(basename(b"c.jpg"), b"c.jpg");
        assert_eq!(basename(b"/a/b/"), b"b");
    }

    #[test]
    fn strncmp_semantics() {
        assert!(!strncmp_differs(b"AOC\x00\x00", b"AOC\x00zz", 6));
        assert!(strncmp_differs(b"AOC\x00\x00", b"AOX\x00", 6));
    }
}
