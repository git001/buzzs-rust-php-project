// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `IMAGETYPE_*` constants, `image_type_to_mime_type()`,
//! `image_type_to_extension()` and the file-type sniffing of
//! `php_getimagetype()` (ref: `ext/standard/php_image.h`,
//! `ext/standard/image.c`).

use std::io::{Read, Seek};

use crate::avif;
use crate::diag::Diagnostic;
use crate::imagesize::{get_wbmp, get_xbm};
use crate::stream::PhpStream;

/// `IMAGE_FILETYPE_UNKNOWN`.
pub const IMAGETYPE_UNKNOWN: i64 = 0;
/// `IMAGE_FILETYPE_GIF`.
pub const IMAGETYPE_GIF: i64 = 1;
/// `IMAGE_FILETYPE_JPEG`.
pub const IMAGETYPE_JPEG: i64 = 2;
/// `IMAGE_FILETYPE_PNG`.
pub const IMAGETYPE_PNG: i64 = 3;
/// `IMAGE_FILETYPE_SWF`.
pub const IMAGETYPE_SWF: i64 = 4;
/// `IMAGE_FILETYPE_PSD`.
pub const IMAGETYPE_PSD: i64 = 5;
/// `IMAGE_FILETYPE_BMP`.
pub const IMAGETYPE_BMP: i64 = 6;
/// `IMAGE_FILETYPE_TIFF_II` (Intel byte order).
pub const IMAGETYPE_TIFF_II: i64 = 7;
/// `IMAGE_FILETYPE_TIFF_MM` (Motorola byte order).
pub const IMAGETYPE_TIFF_MM: i64 = 8;
/// `IMAGE_FILETYPE_JPC`.
pub const IMAGETYPE_JPC: i64 = 9;
/// `IMAGE_FILETYPE_JP2`.
pub const IMAGETYPE_JP2: i64 = 10;
/// `IMAGE_FILETYPE_JPX`.
pub const IMAGETYPE_JPX: i64 = 11;
/// `IMAGE_FILETYPE_JB2`.
pub const IMAGETYPE_JB2: i64 = 12;
/// `IMAGE_FILETYPE_SWC`.
pub const IMAGETYPE_SWC: i64 = 13;
/// `IMAGE_FILETYPE_IFF`.
pub const IMAGETYPE_IFF: i64 = 14;
/// `IMAGE_FILETYPE_WBMP`.
pub const IMAGETYPE_WBMP: i64 = 15;
/// `IMAGETYPE_JPEG2000` is a userland alias of `IMAGETYPE_JPC`.
pub const IMAGETYPE_JPEG2000: i64 = IMAGETYPE_JPC;
/// `IMAGE_FILETYPE_XBM`.
pub const IMAGETYPE_XBM: i64 = 16;
/// `IMAGE_FILETYPE_ICO`.
pub const IMAGETYPE_ICO: i64 = 17;
/// `IMAGE_FILETYPE_WEBP`.
pub const IMAGETYPE_WEBP: i64 = 18;
/// `IMAGE_FILETYPE_AVIF`.
pub const IMAGETYPE_AVIF: i64 = 19;
/// `IMAGE_FILETYPE_HEIF`.
pub const IMAGETYPE_HEIF: i64 = 20;
/// `IMAGE_FILETYPE_FIXED_COUNT` (the `IMAGETYPE_COUNT` constant).
pub const IMAGETYPE_COUNT: i64 = 21;

/// Every `IMAGETYPE_*` constant in registration order
/// (`basic_functions.stub.php`).
pub const IMAGETYPE_CONSTANTS: &[(&str, i64)] = &[
    ("IMAGETYPE_GIF", IMAGETYPE_GIF),
    ("IMAGETYPE_JPEG", IMAGETYPE_JPEG),
    ("IMAGETYPE_PNG", IMAGETYPE_PNG),
    ("IMAGETYPE_SWF", IMAGETYPE_SWF),
    ("IMAGETYPE_PSD", IMAGETYPE_PSD),
    ("IMAGETYPE_BMP", IMAGETYPE_BMP),
    ("IMAGETYPE_TIFF_II", IMAGETYPE_TIFF_II),
    ("IMAGETYPE_TIFF_MM", IMAGETYPE_TIFF_MM),
    ("IMAGETYPE_JPC", IMAGETYPE_JPC),
    ("IMAGETYPE_JP2", IMAGETYPE_JP2),
    ("IMAGETYPE_JPX", IMAGETYPE_JPX),
    ("IMAGETYPE_JB2", IMAGETYPE_JB2),
    ("IMAGETYPE_SWC", IMAGETYPE_SWC),
    ("IMAGETYPE_IFF", IMAGETYPE_IFF),
    ("IMAGETYPE_WBMP", IMAGETYPE_WBMP),
    ("IMAGETYPE_JPEG2000", IMAGETYPE_JPEG2000),
    ("IMAGETYPE_XBM", IMAGETYPE_XBM),
    ("IMAGETYPE_ICO", IMAGETYPE_ICO),
    ("IMAGETYPE_WEBP", IMAGETYPE_WEBP),
    ("IMAGETYPE_AVIF", IMAGETYPE_AVIF),
    ("IMAGETYPE_HEIF", IMAGETYPE_HEIF),
    ("IMAGETYPE_UNKNOWN", IMAGETYPE_UNKNOWN),
    ("IMAGETYPE_COUNT", IMAGETYPE_COUNT),
];

/// `php_image_type_to_mime_type()` / `image_type_to_mime_type()`.
pub fn image_type_to_mime_type(image_type: i64) -> &'static str {
    match image_type {
        IMAGETYPE_GIF => "image/gif",
        IMAGETYPE_JPEG => "image/jpeg",
        IMAGETYPE_PNG => "image/png",
        IMAGETYPE_SWF | IMAGETYPE_SWC => "application/x-shockwave-flash",
        IMAGETYPE_PSD => "image/psd",
        IMAGETYPE_BMP => "image/bmp",
        IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM => "image/tiff",
        IMAGETYPE_IFF => "image/iff",
        IMAGETYPE_WBMP => "image/vnd.wap.wbmp",
        IMAGETYPE_JP2 => "image/jp2",
        IMAGETYPE_XBM => "image/xbm",
        IMAGETYPE_ICO => "image/vnd.microsoft.icon",
        IMAGETYPE_WEBP => "image/webp",
        IMAGETYPE_AVIF => "image/avif",
        IMAGETYPE_HEIF => "image/heif",
        _ => {
            crate::handlers::handler(image_type).map_or("application/octet-stream", |h| h.mime_type)
        }
    }
}

/// `image_type_to_extension()`: the extension without the dot when
/// `include_dot` is false; `None` (PHP `false`) for unknown types.
pub fn image_type_to_extension(image_type: i64, include_dot: bool) -> Option<&'static str> {
    let ext = match image_type {
        IMAGETYPE_GIF => ".gif",
        IMAGETYPE_JPEG => ".jpeg",
        IMAGETYPE_PNG => ".png",
        IMAGETYPE_SWF | IMAGETYPE_SWC => ".swf",
        IMAGETYPE_PSD => ".psd",
        IMAGETYPE_BMP | IMAGETYPE_WBMP => ".bmp",
        IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM => ".tiff",
        IMAGETYPE_IFF => ".iff",
        IMAGETYPE_JPC => ".jpc",
        IMAGETYPE_JP2 => ".jp2",
        IMAGETYPE_JPX => ".jpx",
        IMAGETYPE_JB2 => ".jb2",
        IMAGETYPE_XBM => ".xbm",
        IMAGETYPE_ICO => ".ico",
        IMAGETYPE_WEBP => ".webp",
        IMAGETYPE_AVIF => ".avif",
        IMAGETYPE_HEIF => ".heif",
        _ => crate::handlers::handler(image_type)?.extension,
    };
    Some(if include_dot {
        ext
    } else {
        ext.get(1..).unwrap_or_default()
    })
}

/// `php_sig_*` byte signatures.
pub mod sig {
    /// GIF.
    pub const GIF: &[u8; 3] = b"GIF";
    /// PSD.
    pub const PSD: &[u8; 4] = b"8BPS";
    /// BMP.
    pub const BMP: &[u8; 2] = b"BM";
    /// SWF.
    pub const SWF: &[u8; 3] = b"FWS";
    /// SWC (compressed SWF).
    pub const SWC: &[u8; 3] = b"CWS";
    /// JPEG.
    pub const JPG: &[u8; 3] = b"\xff\xd8\xff";
    /// PNG.
    pub const PNG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    /// TIFF, Intel byte order.
    pub const TIF_II: &[u8; 4] = b"II\x2a\x00";
    /// TIFF, Motorola byte order.
    pub const TIF_MM: &[u8; 4] = b"MM\x00\x2a";
    /// JPEG 2000 code stream.
    pub const JPC: &[u8; 3] = b"\xff\x4f\xff";
    /// JP2.
    pub const JP2: &[u8; 12] = b"\x00\x00\x00\x0cjP  \x0d\x0a\x87\x0a";
    /// IFF.
    pub const IFF: &[u8; 4] = b"FORM";
    /// ICO.
    pub const ICO: &[u8; 4] = b"\x00\x00\x01\x00";
    /// RIFF container.
    pub const RIFF: &[u8; 4] = b"RIFF";
    /// WebP inside RIFF.
    pub const WEBP: &[u8; 4] = b"WEBP";
    /// ISOBMFF `ftyp` box.
    pub const FTYP: &[u8; 4] = b"ftyp";
    /// HEIF brand `mif1`.
    pub const MIF1: &[u8; 4] = b"mif1";
    /// HEIF brand `heic`.
    pub const HEIC: &[u8; 4] = b"heic";
    /// HEIF brand `heix`.
    pub const HEIX: &[u8; 4] = b"heix";
}

fn read_error(input: &[u8]) -> Diagnostic {
    // "%s" stops at the first NUL byte.
    let end = input.iter().position(|&b| b == 0).unwrap_or(input.len());
    let mut msg = b"Error reading from ".to_vec();
    msg.extend_from_slice(&input[..end]);
    msg.push(b'!');
    Diagnostic::notice(msg)
}

/// `php_getimagetype()`: sniffs the type of the image at the current stream
/// position.  `input` is the file name (or, for `getimagesizefromstring()`,
/// the data itself) used in the "Error reading from %s!" notice.
pub(crate) fn getimagetype<R: Read + Seek>(
    stream: &mut PhpStream<R>,
    input: &[u8],
    diags: &mut Vec<Diagnostic>,
) -> i64 {
    let mut ft = [0u8; 12];
    if stream.read(&mut ft[..3]) != 3 {
        diags.push(read_error(input));
        return IMAGETYPE_UNKNOWN;
    }
    if &ft[..3] == sig::GIF {
        return IMAGETYPE_GIF;
    } else if &ft[..3] == sig::JPG {
        return IMAGETYPE_JPEG;
    } else if ft[..3] == sig::PNG[..3] {
        if stream.read(&mut ft[3..8]) != 5 {
            diags.push(read_error(input));
            return IMAGETYPE_UNKNOWN;
        }
        if &ft[..8] == sig::PNG {
            return IMAGETYPE_PNG;
        }
        diags.push(Diagnostic::warning(
            "PNG file corrupted by ASCII conversion",
        ));
        return IMAGETYPE_UNKNOWN;
    } else if &ft[..3] == sig::SWF {
        return IMAGETYPE_SWF;
    } else if &ft[..3] == sig::SWC {
        return IMAGETYPE_SWC;
    } else if ft[..3] == sig::PSD[..3] {
        return IMAGETYPE_PSD;
    } else if &ft[..2] == sig::BMP {
        return IMAGETYPE_BMP;
    } else if &ft[..3] == sig::JPC {
        return IMAGETYPE_JPC;
    } else if ft[..3] == sig::RIFF[..3] {
        if stream.read(&mut ft[3..12]) != 9 {
            diags.push(read_error(input));
            return IMAGETYPE_UNKNOWN;
        }
        if &ft[8..12] == sig::WEBP {
            return IMAGETYPE_WEBP;
        }
        return IMAGETYPE_UNKNOWN;
    }

    if stream.read(&mut ft[3..4]) != 1 {
        diags.push(read_error(input));
        return IMAGETYPE_UNKNOWN;
    }
    if &ft[..4] == sig::TIF_II {
        return IMAGETYPE_TIFF_II;
    } else if &ft[..4] == sig::TIF_MM {
        return IMAGETYPE_TIFF_MM;
    } else if &ft[..4] == sig::IFF {
        return IMAGETYPE_IFF;
    } else if &ft[..4] == sig::ICO {
        return IMAGETYPE_ICO;
    }

    // WBMP may be smaller than 12 bytes, so delay the error.
    let twelve = stream.read(&mut ft[4..12]) == 8;
    if twelve && &ft == sig::JP2 {
        return IMAGETYPE_JP2;
    }
    if stream.rewind() && avif::is_avif(stream) {
        return IMAGETYPE_AVIF;
    }
    // GH-20201: after the AVIF check so AVIF is not identified as HEIF.
    if twelve
        && &ft[4..8] == sig::FTYP
        && (&ft[8..12] == sig::MIF1 || &ft[8..12] == sig::HEIC || &ft[8..12] == sig::HEIX)
    {
        return IMAGETYPE_HEIF;
    }
    if get_wbmp(stream, true).is_some() {
        return IMAGETYPE_WBMP;
    }
    if !twelve {
        diags.push(read_error(input));
        return IMAGETYPE_UNKNOWN;
    }
    if get_xbm(stream).is_some() {
        return IMAGETYPE_XBM;
    }
    crate::handlers::identify(stream).unwrap_or(IMAGETYPE_UNKNOWN)
}

/// `exif_imagetype($filename)` on the opened stream: the `IMAGETYPE_*`
/// value, or `None` (PHP `false`) for unknown images.  `input` is the file
/// name used in "Error reading from %s!" notices.
pub fn exif_imagetype<R: Read + Seek>(reader: R, input: &[u8]) -> (Option<i64>, Vec<Diagnostic>) {
    let mut stream = PhpStream::new(reader);
    let mut diags = Vec::new();
    let t = getimagetype(&mut stream, input, &mut diags);
    ((t != IMAGETYPE_UNKNOWN).then_some(t), diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_and_extension_tables() {
        // Registered handlers extend the tables past IMAGETYPE_COUNT: hold
        // the registry still and probe one past the current end.
        let _registry = crate::handlers::REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(image_type_to_mime_type(IMAGETYPE_JPEG), "image/jpeg");
        assert_eq!(
            image_type_to_mime_type(IMAGETYPE_JPC),
            "application/octet-stream"
        );
        assert_eq!(image_type_to_mime_type(-1), "application/octet-stream");
        assert_eq!(image_type_to_mime_type(IMAGETYPE_HEIF), "image/heif");
        assert_eq!(image_type_to_extension(IMAGETYPE_WBMP, true), Some(".bmp"));
        assert_eq!(image_type_to_extension(IMAGETYPE_JPEG, false), Some("jpeg"));
        assert_eq!(image_type_to_extension(IMAGETYPE_UNKNOWN, true), None);
        assert_eq!(
            image_type_to_extension(crate::handlers::image_type_count(), true),
            None
        );
        assert_eq!(IMAGETYPE_CONSTANTS.len(), 23);
    }
}
