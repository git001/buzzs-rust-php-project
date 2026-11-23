// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! API-level behaviour: stream origins, position handling, INI settings.

use std::io::{Cursor, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use php_exif::encoding::BuiltinConverter;
use php_exif::exif::{exif_read_data, exif_thumbnail, ExifConfig, ReadOptions, StreamOrigin};
use php_runtime::Value;

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/exif")
        .join(rel)
}

fn messages(d: &[php_exif::diag::Diagnostic]) -> Vec<String> {
    d.iter()
        .map(php_exif::diag::Diagnostic::message_lossy)
        .collect()
}

#[test]
fn stream_position_is_restored_when_not_at_start() {
    // exif_read_data_streams_seek.phpt
    let mut f = std::fs::File::open(fixture("image027.tiff")).expect("open");
    f.seek(SeekFrom::Start(100)).expect("seek");
    let origin = StreamOrigin::Other;
    let (v, _) = exif_read_data(
        &mut f,
        &origin,
        &ReadOptions::default(),
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    assert!(v.as_array().is_some());
    assert_eq!(f.stream_position().expect("pos"), 100);
}

#[test]
fn other_streams_have_no_file_name_and_size_from_seeking() {
    let data = std::fs::read(fixture("test2.jpg")).expect("read");
    let (v, d) = exif_read_data(
        Cursor::new(&data),
        &StreamOrigin::Other,
        &ReadOptions::default(),
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    assert!(d.is_empty());
    let a = v.as_array().expect("array");
    assert!(a.get_str(b"FileName").is_none());
    assert_eq!(a.get_str(b"FileDateTime").and_then(Value::as_int), Some(0));
    assert_eq!(
        a.get_str(b"FileSize").and_then(Value::as_int),
        Some(i64::try_from(data.len()).expect("len"))
    );
}

#[test]
fn unstatable_plain_files_report_size_zero() {
    let data = std::fs::read(fixture("test2.jpg")).expect("read");
    let (v, d) = exif_read_data(
        Cursor::new(&data),
        &StreamOrigin::UnstatableFile,
        &ReadOptions::default(),
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    assert!(matches!(v, Value::Bool(false)));
    assert_eq!(messages(&d), ["File too small (0)"]);
}

#[test]
fn non_regular_files_are_rejected() {
    // bug76409.phpt: exif_read_data('.')
    let (v, d) = exif_read_data(
        Cursor::new(Vec::<u8>::new()),
        &StreamOrigin::File {
            path: b".".to_vec(),
            mtime: 0,
            size: 4096,
            regular: false,
        },
        &ReadOptions::default(),
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    assert!(matches!(v, Value::Bool(false)));
    assert_eq!(messages(&d), ["Not a file"]);
}

#[test]
fn required_sections_filter() {
    let path = fixture("test2.jpg");
    let size = std::fs::metadata(&path).expect("meta").len();
    let origin = StreamOrigin::File {
        path: path.to_string_lossy().as_bytes().to_vec(),
        mtime: 5,
        size,
        regular: true,
    };
    let run = |s: &str| {
        let opts = ReadOptions {
            required_sections: Some(s.as_bytes().to_vec()),
            ..ReadOptions::default()
        };
        exif_read_data(
            std::fs::File::open(&path).expect("open"),
            &origin,
            &opts,
            &ExifConfig::default(),
            &BuiltinConverter,
        )
        .0
    };
    assert!(matches!(run("GPS"), Value::Bool(false)));
    assert!(run("GPS,COMMENT").as_array().is_some());
    assert!(run("FILE").as_array().is_some());
    // Unknown names select nothing, so nothing is filtered.
    assert!(run("gps").as_array().is_some());
}

#[test]
fn ini_settings_validate_encodings() {
    let mut cfg = ExifConfig::default();
    let c = BuiltinConverter;
    assert_eq!(cfg.set("exif.encode_unicode", "UTF-8", &c), Ok(true));
    assert_eq!(cfg.get("exif.encode_unicode"), Some("UTF-8"));
    let err = cfg
        .set("exif.decode_unicode_intel", "bogus", &c)
        .expect_err("invalid");
    assert_eq!(err.message_lossy(), "Illegal encoding ignored: 'bogus'");
    assert_eq!(cfg.get("exif.decode_unicode_intel"), Some("UCS-2LE"));
    assert_eq!(cfg.set("exif.encode_jis", "", &c), Ok(true));
    assert!(cfg.set("exif.decode_jis_intel", "", &c).is_err());
    assert_eq!(cfg.set("date.timezone", "UTC", &c), Ok(false));
}

#[test]
fn unicode_user_comment_follows_encode_unicode() {
    // test3.jpg has a UNICODE UserComment.
    let path = fixture("test3.jpg");
    let origin = StreamOrigin::Other;
    let mut cfg = ExifConfig::default();
    cfg.set("exif.encode_unicode", "UTF-8", &BuiltinConverter)
        .expect("ini");
    let (v, _) = exif_read_data(
        std::fs::File::open(&path).expect("open"),
        &origin,
        &ReadOptions::default(),
        &cfg,
        &BuiltinConverter,
    );
    let computed = v
        .as_array()
        .and_then(|a| a.get_str(b"COMPUTED"))
        .and_then(Value::as_array)
        .expect("computed");
    let uc = computed
        .get_str(b"UserComment")
        .and_then(Value::as_str)
        .expect("comment");
    assert_eq!(
        std::str::from_utf8(uc.as_bytes()).expect("utf8"),
        "\u{c4}\u{d6}\u{dc}\u{df}\u{e4}\u{f6}\u{fc}"
    );
}

#[test]
fn thumbnail_dimensions_only_when_requested() {
    let path = fixture("test2.jpg");
    let (t, _) = exif_thumbnail(
        std::fs::File::open(&path).expect("open"),
        &StreamOrigin::Other,
        true,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    let t = t.expect("thumbnail");
    assert_eq!((t.width, t.height, t.image_type), (1, 1, 2));
    let (t, _) = exif_thumbnail(
        std::fs::File::open(&path).expect("open"),
        &StreamOrigin::Other,
        false,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    assert_eq!(t.expect("thumbnail").width, 0);
}
