// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/bcmath` for phprust: libbcmath's arbitrary-precision decimal
//! arithmetic, the procedural `bc*()` functions and `BcMath\Number`.
//!
//! The crate is independent of the VM: inputs are byte strings and integers,
//! results are `String`s, and failures are [`BcError`]s carrying the PHP
//! exception class and exact message.
//!
//! * [`BcNum`] — the number representation (`bc_num`): parsing
//!   ([`BcNum::parse`]), formatting ([`BcNum::to_string_scaled`]) and the
//!   arithmetic primitives with libbcmath's scale rules.
//! * [`BcMath`] — `bcadd()` … `bcround()` plus the `bcmath.scale` state.
//! * [`Number`] — the `BcMath\Number` class semantics.
//!
//! ```
//! use php_bcmath::{BcMath, Number, Operand, RoundingMode};
//!
//! let bc = BcMath::new();
//! assert_eq!(bc.bcdiv(b"1", b"3", Some(5)).unwrap(), "0.33333");
//! assert_eq!(bc.bcround(b"-1.55", 1, RoundingMode::HalfEven).unwrap(), "-1.6");
//!
//! let n = Number::new(Operand::from("1.5")).unwrap();
//! let r = n.div(Operand::Int(4), None).unwrap();
//! assert_eq!((r.value().as_str(), r.scale()), ("0.375", 3));
//! ```

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod bigint;
mod error;
mod functions;
mod num;
mod number;
mod ops;

pub use error::{BcError, ErrorKind};
pub use functions::{BcMath, MAX_DIGITS, MAX_SCALE};
pub use num::{BcNum, Parsed, Sign};
pub use number::{BinaryOp, Number, Operand, CLASS_NAME, EXPAND_SCALE};
pub use ops::{RaiseError, RaiseModError, RoundingMode};
