// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Port of PHP's `ext/gmp` on top of a pure-Rust big integer engine.
//!
//! * [`mpz`] — [`Mpz`], an arbitrary-precision signed integer reproducing
//!   GMP's `mpz_*` semantics (division rounding modes, two's complement bit
//!   operations, `mpz_get_si`/`mpz_get_d`, `mpz_set_str`/`mpz_get_str`,
//!   import/export, gcdext cofactors, primality certainty levels, ...).
//! * [`random`] — GMP's Mersenne Twister, so seeded `gmp_random_*` output
//!   matches PHP exactly.
//! * [`php`] — the userland functions, the `GMP` class casts,
//!   (un)serialization helpers and operator overloading with PHP's exact
//!   error messages.

// Number-theoretic code follows the usual mathematical notation (a, b, n, q, r...).
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::many_single_char_names)]

pub mod mpz;
pub mod php;
pub mod random;

pub use mpz::{Endian, Mpz, Round, WordOrder};
pub use php::{GmpError, Num};
