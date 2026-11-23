// buzzs-rust-php-project - Copyright (c) 2026 git001 (Aleks)
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The bytecode cache: compiled scripts serialized to bytes and back, so
//! `opcache.file_cache` stores bytecode (as `zend_file_cache.c` does)
//! rather than source that has to be compiled again.
//!
//! An image is `FORMAT_VERSION`, the string table, then the body
//! ([`graph`]). The file header around it (magic, system id, timestamp,
//! checksum) belongs to `php-opcache`'s file cache; [`format_id`] is the
//! part of the system id that names this encoding.
//!
//! ref: `ext/opcache/zend_file_cache.c`

pub mod codec;
mod graph;
mod ops;

use crate::compiler::CompiledUnit;
use codec::{DResult, DecodeError, EResult, Reader, Writer};

/// Version of the encoding. Bump it whenever the encoding of any type
/// changes (a field, a variant, a tag), so stale cache files are
/// rejected instead of misread.
pub const FORMAT_VERSION: u32 = 1;

/// Identifies this encoding for the cache's system id: the format
/// version, the engine version and the build id (a hash of the engine's
/// sources, `build.rs`; php-src's `ZEND_BIN_ID`). A cache directory shared
/// by two builds keeps their entries apart, also when only the compiler,
/// the optimizer or the meaning of an op changed.
#[must_use]
pub fn format_id() -> String {
    format!(
        "phprust-bytecode-{FORMAT_VERSION}-{}-{}",
        env!("CARGO_PKG_VERSION"),
        env!("PHPRUST_BUILD_ID")
    )
}

/// Serializes a compiled unit.
///
/// # Errors
/// When the unit holds a value without a compile-time form (an object,
/// a resource or a reference); such a unit is not cached.
pub fn encode(unit: &CompiledUnit) -> EResult<Vec<u8>> {
    let mut w = Writer::new();
    graph::encode_unit(unit, &mut w)?;
    let (table, body) = w.finish();
    let mut out = Vec::with_capacity(4 + 8 + table.len() + body.len());
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(table.len() as u64).to_le_bytes());
    out.extend_from_slice(&table);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Reads a unit written by [`encode`]. The result shares nothing with
/// any other unit and has empty runtime caches.
///
/// # Errors
/// When the image is from another format version, truncated or malformed.
pub fn decode(image: &[u8]) -> DResult<CompiledUnit> {
    let (version, rest) = image
        .split_first_chunk::<4>()
        .ok_or(DecodeError("truncated header"))?;
    if u32::from_le_bytes(*version) != FORMAT_VERSION {
        return Err(DecodeError("format version mismatch"));
    }
    let (table_len, rest) = rest
        .split_first_chunk::<8>()
        .ok_or(DecodeError("truncated header"))?;
    let table_len =
        usize::try_from(u64::from_le_bytes(*table_len)).map_err(|_| DecodeError("table size"))?;
    if table_len > rest.len() {
        return Err(DecodeError("string table exceeds image"));
    }
    let (table, body) = rest.split_at(table_len);
    let mut r = Reader::new(table, body)?;
    let unit = graph::decode_unit(&mut r)?;
    r.expect_end()?;
    Ok(unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::optimizer::DEFAULT_OPTIMIZATION_LEVEL;
    use crate::compiler::{compile, CompileOptions};
    use std::rc::Rc;

    fn compile_src(src: &str, level: u32) -> CompiledUnit {
        let program =
            php_parser::parse(src.as_bytes(), &php_parser::ParseOptions::default()).expect("parse");
        let opts = CompileOptions {
            filename: "/app/test.php".into(),
            optimization_level: level,
            special_builtins: u32::MAX,
            ..CompileOptions::default()
        };
        compile(&program, Rc::from(src.as_bytes()), opts).expect("compile")
    }

    /// Covers every shared object kind: nested functions and closures
    /// (listed in two tables), classes with methods, constants, property
    /// defaults and hooks, enums with cases, attributes on constants and
    /// parameters, typed parameters with constant-expression defaults,
    /// static variables and constant arrays.
    const PROGRAM: &str = r"<?php
        namespace App;
        use Attribute;
        #[Attribute] class Marker { public function __construct(public string $why = 'x') {} }
        #[Marker('const')] const LIMIT = [1, 2, 'three' => 3.5, 'nested' => ['a', null, true]];
        interface Shape { public function area(): float; }
        trait Named { public function name(): string { return static::class; } }
        abstract class Base implements Shape {
            use Named;
            public const ORIGIN = ['x' => 0, 'y' => 0];
            protected static int $count = 0;
            public function __construct(protected readonly float $scale = 1.0 * 2) { static::$count++; }
        }
        final class Circle extends Base {
            public string $label { get => 'circle ' . $this->r; set => $value; }
            public function __construct(private float $r, #[Marker] int|string $id = PHP_INT_MAX) { parent::__construct(); }
            public function area(): float { return M_PI * $this->r ** 2 * $this->scale; }
        }
        enum Suit: string { case Hearts = 'H'; case Spades = 'S';
            public function color(): string { return match ($this) { self::Hearts => 'Red', self::Spades => 'Black' }; } }
        function outer(array $xs = LIMIT): \Closure {
            static $calls = 0; $calls++;
            function inner_declared(int $n): int { return $n * 2; }
            $k = 3;
            return fn(int $x) => $x * $k + inner_declared($x) + $calls;
        }
        $obj = new class(2) extends Base { public function area(): float { return 0.0; } };
        echo outer()(4), Suit::Hearts->color(), (new Circle(1.5))->area(), $obj->area();
        __halt_compiler(); trailing data
    ";

    #[test]
    fn units_roundtrip_byte_for_byte() {
        for level in [0, DEFAULT_OPTIMIZATION_LEVEL] {
            let unit = compile_src(PROGRAM, level);
            let image = encode(&unit).expect("encode");
            let back = decode(&image).expect("decode");
            let again = encode(&back).expect("re-encode");
            assert_eq!(image, again, "level {level}: re-encoding differs");
            assert_eq!(back.anon_end, unit.anon_end);
            assert_eq!(back.meta, unit.meta);
            assert_eq!(back.warnings, unit.warnings);
        }
    }

    #[test]
    fn decoded_units_keep_shared_identity() {
        let unit = compile_src(PROGRAM, DEFAULT_OPTIMIZATION_LEVEL);
        let back = decode(&encode(&unit).expect("encode")).expect("decode");
        // The unit's table is the main op array's table.
        assert!(Rc::ptr_eq(&back.decls, &back.main.code.decls));
        // A function declared inside `outer` is in `outer`'s table and in
        // the unit's table: both must be the same decoded function.
        let unit_fns = back.decls.functions.borrow();
        let outer = unit_fns
            .iter()
            .find(|f| f.name.as_bytes().eq_ignore_ascii_case(b"app\\outer"))
            .expect("outer");
        let nested = outer.code.decls.functions.borrow();
        assert!(!nested.is_empty(), "outer declares functions");
        for f in nested.iter() {
            assert!(
                unit_fns.iter().any(|g| Rc::ptr_eq(f, g)),
                "nested {:?} is not shared with the unit table",
                String::from_utf8_lossy(f.name.as_bytes())
            );
        }
        // Runtime state starts empty.
        assert!(back.main.statics.borrow().is_empty());
        assert!(back.main.code.fcall_cache.borrow().is_empty());
    }

    #[test]
    fn foreign_versions_and_garbage_are_rejected() {
        let unit = compile_src("<?php echo 1 + 2;", 0);
        let mut image = encode(&unit).expect("encode");
        assert!(decode(&image).is_ok());
        assert!(decode(&image[..image.len() - 1]).is_err());
        assert!(decode(&[]).is_err());
        image[0] ^= 0xff;
        assert_eq!(
            decode(&image).err(),
            Some(DecodeError("format version mismatch"))
        );
    }
}
