// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Robustness: random mutations of every fixture must never panic (nor hang)
//! in any of the parsers.

use std::io::Cursor;
use std::path::Path;

use php_exif::encoding::BuiltinConverter;
use php_exif::exif::{exif_read_data, exif_thumbnail, ExifConfig, ReadOptions, StreamOrigin};
use php_exif::imagesize::getimagesizefromstring;
use php_exif::imagetype::exif_imagetype;

/// xorshift64* — deterministic, dependency free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % (n as u64).max(1)).unwrap_or(0)
    }
}

fn fixtures() -> Vec<Vec<u8>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(dir).expect("dir").flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(d) = std::fs::read(&p) {
                // Keep the run fast: skip the one large fixture.
                if d.len() < 200_000 {
                    out.push(d);
                }
            }
        }
    }
    out.sort();
    out
}

fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut d = data.to_vec();
    let edits = 1 + rng.below(8);
    for _ in 0..edits {
        if d.is_empty() {
            d.push(0);
        }
        let pos = rng.below(d.len());
        match rng.below(7) {
            0 => d[pos] = (rng.next() & 0xFF) as u8,
            1 => d[pos] ^= 1 << rng.below(8),
            2 => d[pos] = [0, 0xFF, 0x7F, 0x80, 1][rng.below(5)],
            3 => d.truncate(pos),
            4 => {
                let n = rng.below(16);
                d.splice(pos..pos, std::iter::repeat_n(0xFFu8, n));
            }
            5 => {
                // Overwrite 4 bytes with an interesting 32-bit value.
                let v: u32 = [0, 1, 8, 0xFFFF, 0x7FFF_FFFF, 0xFFFF_FFFF, 0x8000_0000][rng.below(7)];
                let b = if rng.below(2) == 0 {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                };
                for (i, x) in b.iter().enumerate() {
                    if pos + i < d.len() {
                        d[pos + i] = *x;
                    }
                }
            }
            _ => {
                let end = (pos + rng.below(64)).min(d.len());
                let chunk = d[pos..end].to_vec();
                let at = rng.below(d.len());
                d.splice(at..at, chunk);
            }
        }
    }
    d
}

fn exercise(data: &[u8]) {
    let cfg = ExifConfig::default();
    let origin = StreamOrigin::Other;
    for (arrays, thumb) in [(false, false), (true, true)] {
        let opts = ReadOptions {
            required_sections: None,
            as_arrays: arrays,
            read_thumbnail: thumb,
        };
        let _ = exif_read_data(Cursor::new(data), &origin, &opts, &cfg, &BuiltinConverter);
    }
    let _ = exif_thumbnail(Cursor::new(data), &origin, true, &cfg, &BuiltinConverter);
    let _ = exif_imagetype(Cursor::new(data), b"x");
    let o = getimagesizefromstring(data, true);
    let _ = o.to_value();
}

#[test]
fn mutated_fixtures_never_panic() {
    let iterations: usize = std::env::var("EXIF_FUZZ_ITERATIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for f in fixtures() {
        exercise(&f);
        for _ in 0..iterations {
            let m = mutate(&mut rng, &f);
            exercise(&m);
        }
    }
}

#[test]
fn tiny_and_synthetic_inputs_never_panic() {
    let mut rng = Rng(42);
    for len in 0..64 {
        for _ in 0..50 {
            let d: Vec<u8> = (0..len).map(|_| (rng.next() & 0xFF) as u8).collect();
            exercise(&d);
            // Valid-looking headers followed by noise.
            for head in [
                &b"\xFF\xD8\xFF\xE1"[..],
                b"II*\x00\x08\x00\x00\x00",
                b"MM\x00*\x00\x00\x00\x08",
                b"\x00\x00\x00\x18ftypheic",
                b"RIFF\x00\x00\x00\x00WEBPVP8X",
                b"\x00\x00\x00\x0cjP  \r\n\x87\n",
                b"CWS\x08",
                b"FORM\x00\x00\x00\x00ILBM",
                b"\x00\x00\x01\x00",
                b"#define a_width 1\n",
            ] {
                let mut x = head.to_vec();
                x.extend_from_slice(&d);
                exercise(&x);
            }
        }
    }
}
