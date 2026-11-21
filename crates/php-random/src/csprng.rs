// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The OS CSPRNG (ext/random/csprng.c): `random_bytes()` / `random_int()`.

use crate::error::{RandomError, Result};

/// The message PHP reports when the OS cannot provide randomness.
pub const CSPRNG_FAILURE: &str = "Could not gather sufficient random data";

/// `php_random_bytes_ex`: fills `buf` from the OS CSPRNG. On failure the
/// error string PHP would report is returned.
pub fn fill(buf: &mut [u8]) -> std::result::Result<(), &'static str> {
    if buf.is_empty() {
        return Ok(());
    }
    getrandom::fill(buf).map_err(|_| CSPRNG_FAILURE)
}

/// `php_random_bytes_throw`: like [`fill`] but with a `Random\RandomException`.
pub fn fill_throw(buf: &mut [u8]) -> Result<()> {
    fill(buf).map_err(|m| RandomError::RandomException(m.to_owned()))
}

/// A random `u64` from the CSPRNG (`php_random_bytes_throw(&u64, 8)`).
pub fn next_u64() -> Result<u64> {
    getrandom::u64().map_err(|_| RandomError::RandomException(CSPRNG_FAILURE.to_owned()))
}

/// `php_random_int`: uniform integer in `[min, max]` straight from the
/// CSPRNG (no attempt limit; `min == max` consumes no randomness).
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
pub fn random_int_unchecked(min: i64, max: i64) -> Result<i64> {
    if min == max {
        return Ok(min);
    }
    let mut umax = (max as u64).wrapping_sub(min as u64);
    let mut trial = next_u64()?;
    if umax == u64::MAX {
        return Ok(trial as i64);
    }
    umax += 1;
    if umax & (umax - 1) != 0 {
        let limit = u64::MAX - (u64::MAX % umax) - 1;
        while trial > limit {
            trial = next_u64()?;
        }
    }
    Ok((trial % umax).wrapping_add(min as u64) as i64)
}

/// `random_int(int $min, int $max)`.
pub fn random_int(min: i64, max: i64) -> Result<i64> {
    if min > max {
        return Err(RandomError::argument_value(
            "random_int",
            1,
            "min",
            "must be less than or equal to argument #2 ($max)",
        ));
    }
    random_int_unchecked(min, max)
}

/// `random_bytes(int $length)`.
pub fn random_bytes(length: i64) -> Result<Vec<u8>> {
    let Ok(len) = usize::try_from(length) else {
        return Err(length_error());
    };
    if len < 1 {
        return Err(length_error());
    }
    let mut v = vec![0u8; len];
    fill_throw(&mut v)?;
    Ok(v)
}

fn length_error() -> RandomError {
    RandomError::argument_value("random_bytes", 1, "length", "must be greater than 0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        assert_eq!(random_bytes(16).map(|v| v.len()), Ok(16));
        assert_eq!(
            random_bytes(0).unwrap_err().message(),
            "random_bytes(): Argument #1 ($length) must be greater than 0"
        );
        assert!(random_bytes(-5).is_err());
        assert!(fill(&mut []).is_ok());
    }

    #[test]
    fn ints() {
        assert_eq!(random_int(5, 5), Ok(5));
        assert_eq!(
            random_int(42, 0).unwrap_err().message(),
            "random_int(): Argument #1 ($min) must be less than or equal to argument #2 ($max)"
        );
        for _ in 0..200 {
            let v = random_int(-3, 3).unwrap();
            assert!((-3..=3).contains(&v));
            let v = random_int(0, 7).unwrap();
            assert!((0..=7).contains(&v));
        }
        assert!(random_int(i64::MIN, i64::MAX).is_ok());
        let v = random_int(i64::MAX - 1, i64::MAX).unwrap();
        assert!(v >= i64::MAX - 1);
    }
}
