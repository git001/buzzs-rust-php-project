// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A bit-exact Rust port of PHP's `ext/random`.
//!
//! * [`engine`]: the engines (`Random\Engine\Mt19937`,
//!   `PcgOneseq128XslRr64`, `Xoshiro256StarStar`, `Secure`, userland
//!   engines via [`engine::UserGenerate`]) behind the [`Engine`] trait
//!   (`php_random_algo`), with their `__serialize()` state formats.
//! * [`range`]: `php_random_range32/64` and the legacy `MT_RAND_PHP` scaling.
//! * [`gammasection`]: the γ-section float algorithm of `getFloat()`.
//! * [`randomizer`]: every `Random\Randomizer` method.
//! * [`shuffle`]: the shuffling/key-picking algorithms shared with
//!   `shuffle()`, `str_shuffle()` and `array_rand()`.
//! * [`legacy`]: the request-global state and `mt_rand()`, `rand()`,
//!   `lcg_value()`, `uniqid()` & co.
//! * [`csprng`]: `random_bytes()` / `random_int()`.
//!
//! The crate does not depend on the runtime's value types: inputs and
//! outputs are integers, floats, byte strings and slices, and every PHP
//! throwable is described by a [`RandomError`] carrying PHP's exact message.
//!
//! ```
//! use php_random::{Mt19937, Mt19937Mode, RandomGlobals, Randomizer};
//!
//! // mt_srand(42); mt_rand(); mt_rand(1, 100);
//! let mut g = RandomGlobals::new();
//! g.mt_srand(Some(42), php_random::MT_RAND_MT19937);
//! assert_eq!(g.mt_rand(), 804_318_771);
//! assert_eq!(g.mt_rand_range(1, 100), Ok(68));
//!
//! // (new Random\Randomizer(new Random\Engine\Mt19937(1)))->getInt(0, 10)
//! let mut r = Randomizer::new(Mt19937::new(1, Mt19937Mode::Mt19937));
//! assert_eq!(r.get_int(0, 10), Ok(0));
//! ```

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod csprng;
pub mod engine;
pub mod error;
pub mod gammasection;
pub mod legacy;
pub mod randomizer;
pub mod range;
pub mod shuffle;
pub mod state;

pub use engine::{
    Engine, EngineKind, Generated, Mt19937, Mt19937Mode, NativeEngine, PcgOneseq128XslRr64, Secure,
    Seed, UserEngine, UserGenerate, Xoshiro256StarStar,
};
pub use error::{RandomError, Result};
pub use legacy::RandomGlobals;
pub use randomizer::{IntervalBoundary, Randomizer};
pub use shuffle::{ArraySlots, Dense};
pub use state::{StateArray, StateValue};

/// `PHP_RANDOM_RANGE_ATTEMPTS`: rejection rounds before giving up with a
/// `Random\BrokenRandomEngineError`.
pub const RANGE_ATTEMPTS: u32 = 50;

/// Value of the `MT_RAND_MT19937` constant.
pub const MT_RAND_MT19937: i64 = 0;
/// Value of the (deprecated) `MT_RAND_PHP` constant.
pub const MT_RAND_PHP: i64 = 1;
