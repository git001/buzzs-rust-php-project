// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `cargo run -p php-exif --example exif_dump -- FILE [SECTIONS] [ARRAYS] [THUMB]`
//! prints what `var_dump(exif_read_data(...))` would, preceded by the
//! diagnostics.

use std::fs::File;
use std::io::Write;

use php_exif::encoding::BuiltinConverter;
use php_exif::exif::{exif_read_data, ExifConfig, ReadOptions, StreamOrigin};
use php_runtime::dump::var_dump;
use php_runtime::CollectingContext;

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first() else {
        eprintln!("usage: exif_dump FILE [SECTIONS] [ARRAYS] [THUMB]");
        return Ok(());
    };
    let meta = std::fs::metadata(path)?;
    let origin = StreamOrigin::File {
        path: path.as_bytes().to_vec(),
        mtime: 0,
        size: meta.len(),
        regular: meta.is_file(),
    };
    let opts = ReadOptions {
        required_sections: args
            .get(1)
            .filter(|s| *s != "-")
            .map(|s| s.as_bytes().to_vec()),
        as_arrays: args.get(2).is_some_and(|s| s == "1"),
        read_thumbnail: args.get(3).is_some_and(|s| s == "1"),
    };
    let (v, diags) = exif_read_data(
        File::open(path)?,
        &origin,
        &opts,
        &ExifConfig::default(),
        &BuiltinConverter,
    );
    for d in diags {
        println!(
            "{}: exif_read_data(): {}",
            d.level.label(),
            d.message_lossy()
        );
    }
    let mut out = Vec::new();
    if var_dump(&mut out, &v, &mut CollectingContext::new()).is_ok() {
        std::io::stdout().write_all(&out)?;
    }
    Ok(())
}
