// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Bit-exact implementations of PHP's `ext/hash` (and the `md5()`, `sha1()`,
//! `crc32()` helpers of `ext/standard`).
//!
//! This crate deliberately knows nothing about PHP values: it exposes plain
//! Rust APIs (bytes in, bytes out) plus error descriptions carrying PHP's
//! exact messages, which the VM wraps into `hash()`, `hash_init()`,
//! `HashContext`, etc.
//!
//! * [`hash_algos`] / [`hash_hmac_algos`] — the registered algorithm names in
//!   PHP's order.
//! * [`hash`], [`hash_hmac`], [`hash_hkdf`], [`hash_pbkdf2`], [`hash_equals`].
//! * [`HashContext`] — the incremental API (`hash_init`/`hash_update`/
//!   `hash_final`/`hash_copy`) including `HASH_HMAC` and
//!   `__serialize`/`__unserialize` support.
//! * [`md5`], [`sha1`], [`crc32`], [`bin2hex`] and the `mhash*` helpers.

// Hash algorithms are specified with single-letter state variables
// (a, b, c, d, e, h, k, ...); renaming them would obscure the
// correspondence with the reference C code.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::many_single_char_names)]

mod algo;
mod spec;

use std::fmt;

use algo::checksum::{Checksum32, Fnv64, Kind32};
use algo::gost::Gost;
use algo::haval::Haval;
use algo::md::{Md2, Md4, Md5};
use algo::murmur::{Murmur3a, Murmur3c, Murmur3f};
use algo::ripemd::{Ripemd, RipemdVariant};
use algo::sha1::Sha1;
use algo::sha2::{Sha256, Sha256Variant, Sha512, Sha512Variant};
use algo::sha3::{Sha3, KECCAK_MAGIC};
use algo::snefru::Snefru;
use algo::tiger::Tiger;
use algo::whirlpool::Whirlpool;
use algo::xxhash::{Xxh3, Xxh32, Xxh3Init, Xxh64, XXH3_SECRET_SIZE_MAX, XXH3_SECRET_SIZE_MIN};
use algo::HashState;

pub use spec::{StateElem, StateValue};

/// The `HASH_HMAC` flag of `hash_init()`.
pub const HASH_HMAC: i64 = 1;

/// `PHP_HASH_SERIALIZE_MAGIC_SPEC`.
pub const SERIALIZE_MAGIC_SPEC: i64 = 2;

// ------------------------------------------------------------------ errors

/// The PHP class of a thrown error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorClass {
    /// `ValueError`.
    ValueError,
    /// `TypeError`.
    TypeError,
    /// `Error`.
    Error,
    /// `Exception`.
    Exception,
}

/// An error PHP would throw, with its exact message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HashError {
    /// The class of the throwable.
    pub class: ErrorClass,
    /// For argument errors: the 1-based argument number and parameter name.
    pub argument: Option<(u32, &'static str)>,
    /// The message (without the `func(): Argument #n ($name) ` prefix).
    pub message: String,
    /// Set when PHP would fail the request with a memory-allocation fatal
    /// error rather than throw: the number of bytes PHP tried to `emalloc`.
    /// The binding should report `Allowed memory size of %zu bytes
    /// exhausted (tried to allocate %zu bytes)` (or `Out of memory`) itself,
    /// since only it knows `memory_limit`.
    pub out_of_memory: Option<usize>,
    /// Diagnostics PHP emits *before* throwing this error (e.g. the
    /// non-string secret deprecation preceding a too-short-secret `Error`).
    pub diagnostics: Vec<Diagnostic>,
}

impl HashError {
    fn arg(class: ErrorClass, n: u32, name: &'static str, message: impl Into<String>) -> Self {
        Self {
            class,
            argument: Some((n, name)),
            message: message.into(),
            out_of_memory: None,
            diagnostics: Vec::new(),
        }
    }

    fn plain(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            argument: None,
            message: message.into(),
            out_of_memory: None,
            diagnostics: Vec::new(),
        }
    }

    fn invalid_algo() -> Self {
        Self::arg(
            ErrorClass::ValueError,
            1,
            "algo",
            "must be a valid hashing algorithm",
        )
    }

    fn invalid_crypto_algo() -> Self {
        Self::arg(
            ErrorClass::ValueError,
            1,
            "algo",
            "must be a valid cryptographic hashing algorithm",
        )
    }

    /// The full message as PHP reports it when raised by `function`, e.g.
    /// `hash(): Argument #1 ($algo) must be a valid hashing algorithm`.
    pub fn message_for(&self, function: &str) -> String {
        match self.argument {
            Some((n, name)) => format!("{function}(): Argument #{n} (${name}) {}", self.message),
            None => self.message.clone(),
        }
    }
}

impl fmt::Display for HashError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.argument {
            Some((n, name)) => write!(f, "Argument #{n} (${name}) {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for HashError {}

/// Severity of a non-fatal diagnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// `E_DEPRECATED`.
    Deprecated,
    /// `E_WARNING`.
    Warning,
}

/// A `php_error_docref` diagnostic; the VM prefixes `func(): `.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Severity.
    pub level: DiagnosticLevel,
    /// Message text.
    pub message: String,
}

// ----------------------------------------------------------------- options

/// The `"seed"` entry of the `$options` array.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeedOption {
    /// An `int` seed.
    Int(i64),
    /// A value of any other type (deprecated; treated as no seed).
    NonInt,
}

/// The `"secret"` entry of the `$options` array, already converted to a
/// string by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretOption {
    /// The string value.
    pub bytes: Vec<u8>,
    /// Whether the original value was a string (otherwise a deprecation is
    /// emitted).
    pub was_string: bool,
}

/// The `$options` array accepted by `hash()`, `hash_file()`, `hash_init()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HashOptions {
    /// `$options["seed"]`, if present.
    pub seed: Option<SeedOption>,
    /// `$options["secret"]`, if present.
    pub secret: Option<SecretOption>,
}

impl HashOptions {
    /// Options with only an integer seed.
    pub fn with_seed(seed: i64) -> Self {
        Self {
            seed: Some(SeedOption::Int(seed)),
            secret: None,
        }
    }

    /// Options with only a string secret.
    pub fn with_secret(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            seed: None,
            secret: Some(SecretOption {
                bytes: secret.into(),
                was_string: true,
            }),
        }
    }
}

// ---------------------------------------------------------------- registry

/// How an algorithm interprets `$options`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OptionKind {
    None,
    /// murmur3*, xxh32, xxh64: integer seed.
    Seed,
    /// xxh3/xxh128: seed or secret.
    Xxh3,
}

/// Resolved initialisation arguments.
#[derive(Clone, Debug)]
struct InitArgs {
    seed: u64,
    xxh3: Xxh3Init,
}

impl InitArgs {
    const fn none() -> Self {
        Self {
            seed: 0,
            xxh3: Xxh3Init::Seed(0),
        }
    }
}

type MakeFn = fn(&InitArgs) -> Box<dyn HashState>;

/// One entry of the algorithm registry (a `php_hash_ops`).
#[derive(Debug)]
struct Def {
    name: &'static str,
    /// `ops->algo` (differs from `name` for `snefru256`).
    ops_name: &'static str,
    digest_size: usize,
    block_size: usize,
    is_crypto: bool,
    spec: Option<&'static str>,
    magic: i64,
    options: OptionKind,
    make: MakeFn,
}

macro_rules! def {
    ($name:expr, $ops:expr, $digest:expr, $block:expr, $crypto:expr, $spec:expr, $make:expr) => {
        Def {
            name: $name,
            ops_name: $ops,
            digest_size: $digest,
            block_size: $block,
            is_crypto: $crypto,
            spec: $spec,
            magic: SERIALIZE_MAGIC_SPEC,
            options: OptionKind::None,
            make: $make,
        }
    };
}

#[allow(clippy::cast_possible_truncation)]
static ALGOS: [Def; 60] = [
    def!("md2", "md2", 16, 16, true, Some("b48b16b16b."), |_| {
        Box::new(Md2::new())
    }),
    def!("md4", "md4", 16, 64, true, Some("l4l2b64."), |_| Box::new(
        Md4::new()
    )),
    def!("md5", "md5", 16, 64, true, Some("llllllb64l16."), |_| {
        Box::new(Md5::new())
    }),
    def!(
        "sha1",
        "sha1",
        20,
        64,
        true,
        Some("l5l2b64."),
        |_| Box::new(Sha1::new())
    ),
    def!("sha224", "sha224", 28, 64, true, Some("l8l2b64."), |_| {
        Box::new(Sha256::new(Sha256Variant::Sha224))
    }),
    def!("sha256", "sha256", 32, 64, true, Some("l8l2b64."), |_| {
        Box::new(Sha256::new(Sha256Variant::Sha256))
    }),
    def!("sha384", "sha384", 48, 128, true, Some("q8q2b128."), |_| {
        Box::new(Sha512::new(Sha512Variant::Sha384))
    }),
    def!(
        "sha512/224",
        "sha512/224",
        28,
        128,
        true,
        Some("q8q2b128."),
        |_| { Box::new(Sha512::new(Sha512Variant::Sha512_224)) }
    ),
    def!(
        "sha512/256",
        "sha512/256",
        32,
        128,
        true,
        Some("q8q2b128."),
        |_| { Box::new(Sha512::new(Sha512Variant::Sha512_256)) }
    ),
    def!("sha512", "sha512", 64, 128, true, Some("q8q2b128."), |_| {
        Box::new(Sha512::new(Sha512Variant::Sha512))
    }),
    Def {
        magic: KECCAK_MAGIC,
        ..def!(
            "sha3-224",
            "sha3-224",
            28,
            144,
            true,
            Some("b200IiIIB"),
            |_| Box::new(Sha3::new(224))
        )
    },
    Def {
        magic: KECCAK_MAGIC,
        ..def!(
            "sha3-256",
            "sha3-256",
            32,
            136,
            true,
            Some("b200IiIIB"),
            |_| Box::new(Sha3::new(256))
        )
    },
    Def {
        magic: KECCAK_MAGIC,
        ..def!(
            "sha3-384",
            "sha3-384",
            48,
            104,
            true,
            Some("b200IiIIB"),
            |_| Box::new(Sha3::new(384))
        )
    },
    Def {
        magic: KECCAK_MAGIC,
        ..def!(
            "sha3-512",
            "sha3-512",
            64,
            72,
            true,
            Some("b200IiIIB"),
            |_| Box::new(Sha3::new(512))
        )
    },
    def!(
        "ripemd128",
        "ripemd128",
        16,
        64,
        true,
        Some("l4l2b64."),
        |_| { Box::new(Ripemd::new(RipemdVariant::R128)) }
    ),
    def!(
        "ripemd160",
        "ripemd160",
        20,
        64,
        true,
        Some("l5l2b64."),
        |_| { Box::new(Ripemd::new(RipemdVariant::R160)) }
    ),
    def!(
        "ripemd256",
        "ripemd256",
        32,
        64,
        true,
        Some("l8l2b64."),
        |_| { Box::new(Ripemd::new(RipemdVariant::R256)) }
    ),
    def!(
        "ripemd320",
        "ripemd320",
        40,
        64,
        true,
        Some("l10l2b64."),
        |_| { Box::new(Ripemd::new(RipemdVariant::R320)) }
    ),
    def!(
        "whirlpool",
        "whirlpool",
        64,
        64,
        true,
        Some("q8b32iib64."),
        |_| Box::new(Whirlpool::new())
    ),
    def!(
        "tiger128,3",
        "tiger128,3",
        16,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(false))
    ),
    def!(
        "tiger160,3",
        "tiger160,3",
        20,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(false))
    ),
    def!(
        "tiger192,3",
        "tiger192,3",
        24,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(false))
    ),
    def!(
        "tiger128,4",
        "tiger128,4",
        16,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(true))
    ),
    def!(
        "tiger160,4",
        "tiger160,4",
        20,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(true))
    ),
    def!(
        "tiger192,4",
        "tiger192,4",
        24,
        64,
        true,
        Some("q3qb64l"),
        |_| Box::new(Tiger::new(true))
    ),
    def!("snefru", "snefru", 32, 32, true, Some("l16l2bb32"), |_| {
        Box::new(Snefru::new())
    }),
    def!(
        "snefru256",
        "snefru",
        32,
        32,
        true,
        Some("l16l2bb32"),
        |_| Box::new(Snefru::new())
    ),
    def!("gost", "gost", 32, 32, true, Some("l16l2bb32"), |_| {
        Box::new(Gost::new(false))
    }),
    def!(
        "gost-crypto",
        "gost-crypto",
        32,
        32,
        true,
        Some("l16l2bb32"),
        |_| Box::new(Gost::new(true))
    ),
    def!("adler32", "adler32", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Adler32)
    )),
    def!("crc32", "crc32", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Crc32)
    )),
    def!("crc32b", "crc32b", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Crc32b)
    )),
    def!("crc32c", "crc32c", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Crc32c)
    )),
    def!("fnv132", "fnv132", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Fnv132)
    )),
    def!("fnv1a32", "fnv1a32", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Fnv1a32)
    )),
    def!("fnv164", "fnv164", 8, 4, false, Some("q."), |_| Box::new(
        Fnv64::new(false)
    )),
    def!("fnv1a64", "fnv1a64", 8, 4, false, Some("q."), |_| Box::new(
        Fnv64::new(true)
    )),
    def!("joaat", "joaat", 4, 4, false, Some("l."), |_| Box::new(
        Checksum32::new(Kind32::Joaat)
    )),
    Def {
        options: OptionKind::Seed,
        ..def!("murmur3a", "murmur3a", 4, 4, false, Some("lll"), |a| {
            Box::new(Murmur3a::new(a.seed as u32))
        })
    },
    Def {
        options: OptionKind::Seed,
        ..def!(
            "murmur3c",
            "murmur3c",
            16,
            4,
            false,
            Some("lllllllll"),
            |a| Box::new(Murmur3c::new(a.seed as u32))
        )
    },
    Def {
        options: OptionKind::Seed,
        ..def!("murmur3f", "murmur3f", 16, 8, false, Some("qqqql"), |a| {
            Box::new(Murmur3f::new(a.seed))
        })
    },
    Def {
        options: OptionKind::Seed,
        ..def!("xxh32", "xxh32", 4, 4, false, Some("llllllllllll"), |a| {
            Box::new(Xxh32::new(a.seed as u32))
        })
    },
    Def {
        options: OptionKind::Seed,
        ..def!("xxh64", "xxh64", 8, 8, false, Some("qqqqqqqqqllq"), |a| {
            Box::new(Xxh64::new(a.seed))
        })
    },
    Def {
        options: OptionKind::Xxh3,
        ..def!("xxh3", "xxh3", 8, 8, false, None, |a| Box::new(Xxh3::new(
            false, &a.xxh3
        )))
    },
    Def {
        options: OptionKind::Xxh3,
        ..def!("xxh128", "xxh128", 16, 8, false, None, |a| Box::new(
            Xxh3::new(true, &a.xxh3)
        ))
    },
    def!(
        "haval128,3",
        "haval128,3",
        16,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(3, 128))
    ),
    def!(
        "haval160,3",
        "haval160,3",
        20,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(3, 160))
    ),
    def!(
        "haval192,3",
        "haval192,3",
        24,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(3, 192))
    ),
    def!(
        "haval224,3",
        "haval224,3",
        28,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(3, 224))
    ),
    def!(
        "haval256,3",
        "haval256,3",
        32,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(3, 256))
    ),
    def!(
        "haval128,4",
        "haval128,4",
        16,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(4, 128))
    ),
    def!(
        "haval160,4",
        "haval160,4",
        20,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(4, 160))
    ),
    def!(
        "haval192,4",
        "haval192,4",
        24,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(4, 192))
    ),
    def!(
        "haval224,4",
        "haval224,4",
        28,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(4, 224))
    ),
    def!(
        "haval256,4",
        "haval256,4",
        32,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(4, 256))
    ),
    def!(
        "haval128,5",
        "haval128,5",
        16,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(5, 128))
    ),
    def!(
        "haval160,5",
        "haval160,5",
        20,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(5, 160))
    ),
    def!(
        "haval192,5",
        "haval192,5",
        24,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(5, 192))
    ),
    def!(
        "haval224,5",
        "haval224,5",
        28,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(5, 224))
    ),
    def!(
        "haval256,5",
        "haval256,5",
        32,
        128,
        true,
        Some("l8l2b128"),
        |_| Box::new(Haval::new(5, 256))
    ),
];

/// A registered hashing algorithm (a `php_hash_ops`).
#[derive(Clone, Copy, Debug)]
pub struct Algorithm(&'static Def);

impl PartialEq for Algorithm {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for Algorithm {}

impl Algorithm {
    /// Looks an algorithm up by name, case-insensitively
    /// (`php_hash_fetch_ops`).
    pub fn find(name: &[u8]) -> Option<Self> {
        ALGOS
            .iter()
            .find(|d| d.name.as_bytes().eq_ignore_ascii_case(name))
            .map(Self)
    }

    /// All algorithms in registration order.
    pub fn all() -> impl Iterator<Item = Self> {
        ALGOS.iter().map(Self)
    }

    /// The registered (lower-case) name, as listed by `hash_algos()`.
    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// `ops->algo`: the name used by `__debugInfo()` and serialization
    /// (`"snefru"` for `snefru256`).
    pub fn ops_name(self) -> &'static str {
        self.0.ops_name
    }

    /// Digest length in bytes.
    pub fn digest_size(self) -> usize {
        self.0.digest_size
    }

    /// Block length in bytes (used by HMAC).
    pub fn block_size(self) -> usize {
        self.0.block_size
    }

    /// Whether the algorithm is usable with HMAC/HKDF/PBKDF2.
    pub fn is_crypto(self) -> bool {
        self.0.is_crypto
    }

    /// Creates a state with the given (already resolved) arguments.
    fn state(self, args: &InitArgs) -> Box<dyn HashState> {
        (self.0.make)(args)
    }

    /// Resolves `$options` for this algorithm into init arguments, emitting
    /// PHP's diagnostics and errors.
    fn resolve_options(
        self,
        options: &HashOptions,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> Result<InitArgs, HashError> {
        let mut args = InitArgs::none();
        match self.0.options {
            OptionKind::None => {}
            OptionKind::Seed => match &options.seed {
                #[allow(clippy::cast_sign_loss)]
                Some(SeedOption::Int(s)) => args.seed = *s as u64,
                Some(SeedOption::NonInt) => diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Deprecated,
                    message: "Passing a seed of a type other than int is deprecated because it is the same as setting the seed to 0".into(),
                }),
                None => {}
            },
            OptionKind::Xxh3 => {
                let name = self.0.name;
                if options.seed.is_some() && options.secret.is_some() {
                    return Err(HashError::plain(
                        ErrorClass::Error,
                        format!("{name}: Only one of seed or secret is to be passed for initialization"),
                    ));
                }
                match (&options.seed, &options.secret) {
                    #[allow(clippy::cast_sign_loss)]
                    (Some(SeedOption::Int(s)), _) => args.xxh3 = Xxh3Init::Seed(*s as u64),
                    (Some(SeedOption::NonInt), _) => diagnostics.push(Diagnostic {
                        level: DiagnosticLevel::Deprecated,
                        message: "Passing a seed of a type other than int is deprecated because it is ignored".into(),
                    }),
                    (None, Some(secret)) => {
                        if !secret.was_string {
                            diagnostics.push(Diagnostic {
                                level: DiagnosticLevel::Deprecated,
                                message: "Passing a secret of a type other than string is deprecated because it implicitly converts to a string, potentially hiding bugs".into(),
                            });
                        }
                        let len = secret.bytes.len();
                        if len < XXH3_SECRET_SIZE_MIN {
                            return Err(HashError::plain(
                                ErrorClass::Error,
                                format!(
                                    "{name}: Secret length must be >= {XXH3_SECRET_SIZE_MIN} bytes, {len} bytes passed"
                                ),
                            ));
                        }
                        if len > XXH3_SECRET_SIZE_MAX {
                            diagnostics.push(Diagnostic {
                                level: DiagnosticLevel::Warning,
                                message: format!(
                                    "{name}: Secret content exceeding {XXH3_SECRET_SIZE_MAX} bytes discarded"
                                ),
                            });
                        }
                        args.xxh3 = Xxh3Init::Secret(secret.bytes[..len.min(XXH3_SECRET_SIZE_MAX)].to_vec());
                    }
                    (None, None) => {}
                }
            }
        }
        Ok(args)
    }
}

/// `hash_algos()`: every registered algorithm, in PHP's order.
pub fn hash_algos() -> Vec<&'static str> {
    ALGOS.iter().map(|d| d.name).collect()
}

/// `hash_hmac_algos()`: the cryptographic algorithms, in PHP's order.
pub fn hash_hmac_algos() -> Vec<&'static str> {
    ALGOS
        .iter()
        .filter(|d| d.is_crypto)
        .map(|d| d.name)
        .collect()
}

fn find_crypto(name: &[u8]) -> Result<Algorithm, HashError> {
    Algorithm::find(name)
        .filter(|a| a.is_crypto())
        .ok_or_else(HashError::invalid_crypto_algo)
}

// ----------------------------------------------------------------- one-shot

/// The result of a successful one-shot hash: raw digest plus any
/// diagnostics raised while parsing `$options`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    /// The raw digest bytes (`$binary = true` output).
    pub bytes: Vec<u8>,
    /// Deprecations/warnings to report (prefixed with `func(): ` by the VM).
    pub diagnostics: Vec<Diagnostic>,
}

impl Digest {
    /// The lowercase hex form (`$binary = false` output).
    pub fn hex(&self) -> String {
        bin2hex(&self.bytes)
    }
}

/// `hash($algo, $data, true, $options)`: returns the raw digest.
pub fn hash(algo: &[u8], data: &[u8], options: &HashOptions) -> Result<Digest, HashError> {
    let mut ctx = HashContext::new(algo, 0, b"", options)?;
    ctx.update(data)?;
    let diagnostics = std::mem::take(&mut ctx.diagnostics);
    Ok(Digest {
        bytes: ctx.finalize()?,
        diagnostics,
    })
}

/// Computes the plain digest of `data` with a default-initialised state.
fn digest_of(algo: Algorithm, parts: &[&[u8]]) -> Vec<u8> {
    let mut st = algo.state(&InitArgs::none());
    for p in parts {
        st.update(p);
    }
    let mut out = vec![0; algo.digest_size()];
    st.finish(&mut out);
    out
}

/// `php_hash_hmac_prep_key`: the key padded to the block size and `XORed`
/// with the ipad (0x36).
fn hmac_prep_key(algo: Algorithm, key: &[u8]) -> Vec<u8> {
    let mut k = vec![0u8; algo.block_size()];
    if key.len() > algo.block_size() {
        let d = digest_of(algo, &[key]);
        k[..d.len()].copy_from_slice(&d);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    for b in &mut k {
        *b ^= 0x36;
    }
    k
}

/// `HMAC(key, data)` for a cryptographic algorithm.
fn hmac_raw(algo: Algorithm, key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut k = hmac_prep_key(algo, key);
    let inner = digest_of(algo, &[&k, data]);
    for b in &mut k {
        *b ^= 0x6A;
    }
    digest_of(algo, &[&k, &inner])
}

/// `hash_hmac($algo, $data, $key, true)`: returns the raw MAC.
pub fn hash_hmac(algo: &[u8], data: &[u8], key: &[u8]) -> Result<Vec<u8>, HashError> {
    let a = find_crypto(algo)?;
    Ok(hmac_raw(a, key, data))
}

/// A streaming HMAC context as used by `hash_hmac_file()` (which reports a
/// non-cryptographic algorithm like `hash_hmac()`, not like `hash_init()`).
pub fn hmac_context(algo: &[u8], key: &[u8]) -> Result<HashContext, HashError> {
    let a = find_crypto(algo)?;
    Ok(HashContext::with_algorithm_hmac(a, key))
}

/// `hash_hkdf($algo, $key, $length, $info, $salt)`: RFC 5869 HKDF.
pub fn hash_hkdf(
    algo: &[u8],
    ikm: &[u8],
    length: i64,
    info: &[u8],
    salt: &[u8],
) -> Result<Vec<u8>, HashError> {
    let a = find_crypto(algo)?;
    if ikm.is_empty() {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            2,
            "key",
            "must not be empty",
        ));
    }
    let ds = a.digest_size();
    let length = match length {
        l if l < 0 => {
            return Err(HashError::arg(
                ErrorClass::ValueError,
                3,
                "length",
                "must be greater than or equal to 0",
            ))
        }
        0 => ds,
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        l if l as u64 <= (ds * 255) as u64 => l as usize,
        _ => {
            return Err(HashError::arg(
                ErrorClass::ValueError,
                3,
                "length",
                format!("must be less than or equal to {}", ds * 255),
            ))
        }
    };
    let prk = hmac_raw(a, salt, ikm);
    let mut out = Vec::with_capacity(length + ds);
    let mut prev: Vec<u8> = Vec::new();
    let rounds = (length - 1) / ds + 1;
    for i in 1..=rounds {
        #[allow(clippy::cast_possible_truncation)]
        let c = [(i & 0xff) as u8];
        let mut msg = prev.clone();
        msg.extend_from_slice(info);
        msg.extend_from_slice(&c);
        prev = hmac_raw(a, &prk, &msg);
        out.extend_from_slice(&prev);
    }
    out.truncate(length);
    Ok(out)
}

fn out_of_memory(size: usize) -> HashError {
    HashError {
        out_of_memory: Some(size),
        ..HashError::plain(
            ErrorClass::Error,
            format!("Out of memory (tried to allocate {size} bytes)"),
        )
    }
}

/// Computes `(length, digest_length, result_size)` for `hash_pbkdf2` with
/// checked arithmetic; `length` must already be validated as non-negative.
fn pbkdf2_sizes(
    a: Algorithm,
    length: i64,
    binary: bool,
) -> Result<(usize, usize, usize), HashError> {
    let ds = a.digest_size();
    let mut length = usize::try_from(length).map_err(|_| out_of_memory(usize::MAX))?;
    if length == 0 {
        length = if binary { ds } else { ds * 2 };
    }
    let digest_length = if binary {
        length
    } else {
        length / 2 + length % 2
    };
    let loops = (digest_length - 1) / ds + 1;
    // `safe_emalloc(loops, digest_size, 0)`.
    let size = loops.checked_mul(ds).ok_or_else(|| HashError {
        out_of_memory: Some(usize::MAX),
        ..HashError::plain(
            ErrorClass::Error,
            format!("Possible integer overflow in memory allocation ({loops} * {ds} + 0)"),
        )
    })?;
    Ok((length, digest_length, size))
}

/// The size of the first large buffer `hash_pbkdf2()` allocates
/// (`safe_emalloc(loops, digest_size, 0)`), so the binding can enforce
/// `memory_limit` before doing any work. `None` for an unknown or
/// non-cryptographic algorithm or a negative length.
pub fn hash_pbkdf2_alloc_size(algo: &[u8], length: i64, binary: bool) -> Option<usize> {
    let a = find_crypto(algo).ok()?;
    if length < 0 {
        return None;
    }
    pbkdf2_sizes(a, length, binary)
        .ok()
        .map(|(_, _, size)| size)
}

/// `hash_pbkdf2($algo, $password, $salt, $iterations, $length, $binary)`:
/// returns exactly what PHP returns (hex unless `binary`).
pub fn hash_pbkdf2(
    algo: &[u8],
    password: &[u8],
    salt: &[u8],
    iterations: i64,
    length: i64,
    binary: bool,
) -> Result<Vec<u8>, HashError> {
    let a = find_crypto(algo)?;
    if salt.len() > (i32::MAX as usize) - 4 {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            3,
            "salt",
            "must be less than or equal to INT_MAX - 4 bytes",
        ));
    }
    if iterations <= 0 {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            4,
            "iterations",
            "must be greater than 0",
        ));
    }
    if length < 0 {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            5,
            "length",
            "must be greater than or equal to 0",
        ));
    }
    let (length, digest_length, result_size) = pbkdf2_sizes(a, length, binary)?;
    let ds = a.digest_size();
    let loops = result_size / ds;

    let k1 = hmac_prep_key(a, password);
    let k2: Vec<u8> = k1.iter().map(|b| b ^ 0x6A).collect();
    let round = |data: &[u8]| -> Vec<u8> {
        let inner = digest_of(a, &[&k1, data]);
        digest_of(a, &[&k2, &inner])
    };
    let mut result = Vec::new();
    if result.try_reserve_exact(result_size).is_err() {
        return Err(out_of_memory(result_size));
    }
    let mut computed_salt = salt.to_vec();
    for i in 1..=loops {
        computed_salt.truncate(salt.len());
        #[allow(clippy::cast_possible_truncation)]
        computed_salt.extend_from_slice(&(i as u32).to_be_bytes());
        let mut digest = round(&computed_salt);
        let mut temp = digest.clone();
        for _ in 1..iterations {
            digest = round(&digest);
            for (t, d) in temp.iter_mut().zip(&digest) {
                *t ^= d;
            }
        }
        result.extend_from_slice(&temp);
    }
    if binary {
        result.truncate(length);
        Ok(result)
    } else {
        let mut hex = bin2hex(&result[..digest_length]).into_bytes();
        hex.truncate(length);
        Ok(hex)
    }
}

/// `hash_equals($known, $user)`: timing-safe comparison (a length
/// difference leaks, as in PHP).
pub fn hash_equals(known: &[u8], user: &[u8]) -> bool {
    if known.len() != user.len() {
        return false;
    }
    known
        .iter()
        .zip(user)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Lowercase hex encoding (`bin2hex`).
pub fn bin2hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 15)]));
    }
    s
}

/// `md5($data, true)`.
pub fn md5(data: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut st = Md5::new();
    st.update(data);
    st.finish(&mut out);
    out
}

/// `sha1($data, true)`.
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut out = [0u8; 20];
    let mut st = Sha1::new();
    st.update(data);
    st.finish(&mut out);
    out
}

/// `crc32($data)` (the zlib CRC, identical to `hash('crc32b')`). PHP
/// returns it as a non-negative int on 64-bit builds.
pub fn crc32(data: &[u8]) -> u32 {
    !algo::checksum::crc32b_update(!0, data)
}

/// Incremental `crc32()` as used by `php_crc32_bulk_update`: feed the raw
/// register (start with `0xFFFF_FFFF`, invert at the end).
pub fn crc32_bulk_update(crc: u32, data: &[u8]) -> u32 {
    algo::checksum::crc32b_update(crc, data)
}

// ------------------------------------------------------------- HashContext

/// The incremental hashing object (`HashContext`).
#[derive(Debug)]
pub struct HashContext {
    algo: Algorithm,
    /// `None` once finalized.
    state: Option<Box<dyn HashState>>,
    options: i64,
    /// The HMAC key (already `XORed` with ipad) when `HASH_HMAC` is set.
    key: Option<Vec<u8>>,
    diagnostics: Vec<Diagnostic>,
}

/// The `__serialize()` payload of a [`HashContext`] (minus the members
/// array, which the VM owns).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerializedContext {
    /// Index 0: `ops->algo`.
    pub algo: &'static str,
    /// Index 1: options flags.
    pub options: i64,
    /// Index 2: hash-specific state array.
    pub state: Vec<StateValue>,
    /// Index 3: the magic number describing the state layout.
    pub magic: i64,
}

fn not_finalized_error() -> HashError {
    HashError::arg(
        ErrorClass::TypeError,
        1,
        "context",
        "must be a valid, non-finalized HashContext",
    )
}

impl HashContext {
    /// `hash_init($algo, $flags, $key, $options)`.
    ///
    /// Diagnostics raised by `$options` are available via
    /// [`HashContext::take_diagnostics`].
    pub fn new(
        algo: &[u8],
        flags: i64,
        key: &[u8],
        options: &HashOptions,
    ) -> Result<Self, HashError> {
        let a = Algorithm::find(algo).ok_or_else(HashError::invalid_algo)?;
        if flags & HASH_HMAC != 0 {
            if !a.is_crypto() {
                return Err(HashError::arg(
                    ErrorClass::ValueError,
                    1,
                    "algo",
                    "must be a cryptographic hashing algorithm if HMAC is requested",
                ));
            }
            if key.is_empty() {
                return Err(HashError::arg(
                    ErrorClass::ValueError,
                    3,
                    "key",
                    "must not be empty when HMAC is requested",
                ));
            }
        }
        let mut diagnostics = Vec::new();
        let args = a
            .resolve_options(options, &mut diagnostics)
            .map_err(|mut e| {
                e.diagnostics = std::mem::take(&mut diagnostics);
                e
            })?;
        let mut ctx = Self {
            algo: a,
            state: Some(a.state(&args)),
            options: flags,
            key: None,
            diagnostics,
        };
        if flags & HASH_HMAC != 0 {
            ctx.start_hmac(key);
        }
        Ok(ctx)
    }

    fn with_algorithm_hmac(algo: Algorithm, key: &[u8]) -> Self {
        let mut ctx = Self {
            algo,
            state: Some(algo.state(&InitArgs::none())),
            options: HASH_HMAC,
            key: None,
            diagnostics: Vec::new(),
        };
        ctx.start_hmac(key);
        ctx
    }

    fn start_hmac(&mut self, key: &[u8]) {
        let k = hmac_prep_key(self.algo, key);
        if let Some(st) = &mut self.state {
            st.update(&k);
        }
        self.key = Some(k);
    }

    /// Takes the diagnostics raised while parsing `$options`.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// The algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algo
    }

    /// `__debugInfo()['algo']` (`ops->algo`).
    pub fn algo_name(&self) -> &'static str {
        self.algo.ops_name()
    }

    /// The flags passed to `hash_init()`.
    pub fn options(&self) -> i64 {
        self.options
    }

    /// Whether `hash_final()` has been called.
    pub fn is_finalized(&self) -> bool {
        self.state.is_none()
    }

    /// `hash_update()`.
    pub fn update(&mut self, data: &[u8]) -> Result<(), HashError> {
        let st = self.state.as_mut().ok_or_else(not_finalized_error)?;
        st.update(data);
        Ok(())
    }

    /// `hash_final($ctx, true)`: returns the raw digest and invalidates the
    /// context.
    pub fn finalize(&mut self) -> Result<Vec<u8>, HashError> {
        let mut st = self.state.take().ok_or_else(not_finalized_error)?;
        let mut digest = vec![0u8; self.algo.digest_size()];
        st.finish(&mut digest);
        if let Some(mut k) = self.key.take() {
            for b in &mut k {
                *b ^= 0x6A;
            }
            digest = digest_of(self.algo, &[&k, &digest]);
        }
        Ok(digest)
    }

    /// `clone $ctx` / `hash_copy()`.
    pub fn try_clone(&self) -> Result<Self, HashError> {
        let st = self.state.as_ref().ok_or_else(|| {
            HashError::plain(
                ErrorClass::ValueError,
                "Cannot clone a finalized HashContext",
            )
        })?;
        Ok(Self {
            algo: self.algo,
            state: Some(st.box_clone()),
            options: self.options,
            key: self.key.clone(),
            diagnostics: Vec::new(),
        })
    }

    /// `HashContext::__serialize()` (state part).
    pub fn serialize(&self) -> Result<SerializedContext, HashError> {
        if self.options & HASH_HMAC != 0 {
            return Err(HashError::plain(
                ErrorClass::Exception,
                "HashContext with HASH_HMAC option cannot be serialized",
            ));
        }
        let fail = || {
            HashError::plain(
                ErrorClass::Exception,
                format!(
                    "HashContext for algorithm \"{}\" cannot be serialized",
                    self.algo.ops_name()
                ),
            )
        };
        let (Some(st), Some(spec)) = (&self.state, self.algo.0.spec) else {
            return Err(fail());
        };
        let state = spec::serialize(&st.image(), spec).ok_or_else(fail)?;
        Ok(SerializedContext {
            algo: self.algo.ops_name(),
            options: self.options,
            state,
            magic: self.algo.0.magic,
        })
    }

    /// `HashContext::__unserialize()` (after the VM validated the shape of
    /// the outer array). `state` is `None` when index 2 is not an array;
    /// otherwise it looks up element `j` of that array.
    pub fn unserialize<'a>(
        algo: &[u8],
        options: i64,
        magic: i64,
        state: Option<&dyn Fn(usize) -> Option<StateElem<'a>>>,
    ) -> Result<Self, HashError> {
        if options & HASH_HMAC != 0 {
            return Err(HashError::plain(
                ErrorClass::Exception,
                "HashContext with HASH_HMAC option cannot be serialized",
            ));
        }
        let a = Algorithm::find(algo)
            .ok_or_else(|| HashError::plain(ErrorClass::Exception, "Unknown hash algorithm"))?;
        let ill_formed = |code: i64| {
            HashError::plain(
                ErrorClass::Exception,
                format!(
                    "Incomplete or ill-formed serialization data (\"{}\" code {code})",
                    a.ops_name()
                ),
            )
        };
        let mut st = a.state(&InitArgs::none());
        let (Some(spec), Some(lookup)) = (a.0.spec, state) else {
            return Err(ill_formed(spec::SPEC_FAILURE));
        };
        if magic != a.0.magic {
            return Err(ill_formed(spec::SPEC_FAILURE));
        }
        let mut image = st.image();
        spec::unserialize(&mut image, spec, lookup).map_err(ill_formed)?;
        st.load_image(&image);
        if !st.is_valid() {
            return Err(ill_formed(spec::CONTEXT_VALIDATION_FAILURE));
        }
        Ok(Self {
            algo: a,
            state: Some(st),
            options,
            key: None,
            diagnostics: Vec::new(),
        })
    }
}

impl Clone for HashContext {
    /// Clones the context; a finalized context clones to a finalized one.
    fn clone(&self) -> Self {
        Self {
            algo: self.algo,
            state: self.state.as_ref().map(|s| s.box_clone()),
            options: self.options,
            key: self.key.clone(),
            diagnostics: Vec::new(),
        }
    }
}

// ------------------------------------------------------------------- mhash

/// `(MHASH name, hash algorithm)` indexed by mhash id; `None` for holes.
const MHASH: [Option<(&str, &str)>; 42] = [
    Some(("CRC32", "crc32")),
    Some(("MD5", "md5")),
    Some(("SHA1", "sha1")),
    Some(("HAVAL256", "haval256,3")),
    None,
    Some(("RIPEMD160", "ripemd160")),
    None,
    Some(("TIGER", "tiger192,3")),
    Some(("GOST", "gost")),
    Some(("CRC32B", "crc32b")),
    Some(("HAVAL224", "haval224,3")),
    Some(("HAVAL192", "haval192,3")),
    Some(("HAVAL160", "haval160,3")),
    Some(("HAVAL128", "haval128,3")),
    Some(("TIGER128", "tiger128,3")),
    Some(("TIGER160", "tiger160,3")),
    Some(("MD4", "md4")),
    Some(("SHA256", "sha256")),
    Some(("ADLER32", "adler32")),
    Some(("SHA224", "sha224")),
    Some(("SHA512", "sha512")),
    Some(("SHA384", "sha384")),
    Some(("WHIRLPOOL", "whirlpool")),
    Some(("RIPEMD128", "ripemd128")),
    Some(("RIPEMD256", "ripemd256")),
    Some(("RIPEMD320", "ripemd320")),
    None,
    Some(("SNEFRU256", "snefru256")),
    Some(("MD2", "md2")),
    Some(("FNV132", "fnv132")),
    Some(("FNV1A32", "fnv1a32")),
    Some(("FNV164", "fnv164")),
    Some(("FNV1A64", "fnv1a64")),
    Some(("JOAAT", "joaat")),
    Some(("CRC32C", "crc32c")),
    Some(("MURMUR3A", "murmur3a")),
    Some(("MURMUR3C", "murmur3c")),
    Some(("MURMUR3F", "murmur3f")),
    Some(("XXH32", "xxh32")),
    Some(("XXH64", "xxh64")),
    Some(("XXH3", "xxh3")),
    Some(("XXH128", "xxh128")),
];

fn mhash_entry(id: i64) -> Option<(&'static str, &'static str)> {
    usize::try_from(id)
        .ok()
        .and_then(|i| MHASH.get(i).copied().flatten())
}

/// The `MHASH_*` constants: `(constant suffix, value)` pairs, e.g.
/// `("MD5", 1)` for `MHASH_MD5`.
pub fn mhash_constants() -> Vec<(&'static str, i64)> {
    MHASH
        .iter()
        .zip(0i64..)
        .filter_map(|(e, i)| e.map(|(name, _)| (name, i)))
        .collect()
}

/// `mhash_count()`.
pub fn mhash_count() -> i64 {
    41
}

/// `mhash_get_hash_name($id)`.
pub fn mhash_get_hash_name(id: i64) -> Option<&'static str> {
    mhash_entry(id).map(|(name, _)| name)
}

/// `mhash_get_block_size($id)` (which, as in PHP, is the digest size).
pub fn mhash_get_block_size(id: i64) -> Option<usize> {
    mhash_entry(id)
        .and_then(|(_, algo)| Algorithm::find(algo.as_bytes()))
        .map(Algorithm::digest_size)
}

/// `mhash($id, $data, $key)`: `None` means `false`.
pub fn mhash(id: i64, data: &[u8], key: Option<&[u8]>) -> Result<Option<Vec<u8>>, HashError> {
    let Some((_, algo)) = mhash_entry(id) else {
        return Ok(None);
    };
    match key {
        Some(k) => hash_hmac(algo.as_bytes(), data, k).map(Some),
        None => hash(algo.as_bytes(), data, &HashOptions::default()).map(|d| Some(d.bytes)),
    }
}

/// `mhash_keygen_s2k($id, $password, $salt, $length)`: `None` means `false`.
pub fn mhash_keygen_s2k(
    id: i64,
    password: &[u8],
    salt: &[u8],
    length: i64,
) -> Result<Option<Vec<u8>>, HashError> {
    if length <= 0 {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            4,
            "length",
            "must be a greater than 0",
        ));
    }
    if length > i64::from(i32::MAX) {
        return Err(HashError::arg(
            ErrorClass::ValueError,
            4,
            "length",
            format!("must be less than or equal to {}", i32::MAX),
        ));
    }
    let Some(a) = mhash_entry(id).and_then(|(_, algo)| Algorithm::find(algo.as_bytes())) else {
        return Ok(None);
    };
    let mut padded_salt = [0u8; 8];
    let n = salt.len().min(8);
    padded_salt[..n].copy_from_slice(&salt[..n]);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = length as usize;
    let ds = a.digest_size();
    let times = bytes.div_ceil(ds);
    let mut key = Vec::with_capacity(times * ds);
    for i in 0..times {
        let zeros = vec![0u8; i];
        key.extend_from_slice(&digest_of(a, &[&zeros, &padded_salt, password]));
    }
    key.truncate(bytes);
    Ok(Some(key))
}

#[cfg(test)]
mod tests;
