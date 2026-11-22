// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Encoding detection (`mb_guess_encoding_for_strings`), `mb_detect_encoding`,
//! `mb_convert_encoding` and the core of `mb_convert_variables`.
//!
//! Ref: `ext/mbstring/mbstring.c`.

use crate::convert::BAD_INPUT;
use crate::encoding::{Encoding, EncodingNo};
use crate::error::{MbError, MbResult};
use crate::state::{EncodingList, MbState};
use crate::strings::check_str_encoding;
use crate::tables::rare_cp::RARE_CODEPOINT_BITVEC;

/// `estimate_demerits`.
fn estimate_demerits(w: u32) -> u64 {
    if w > 0xFFFF {
        40
    } else if (0x21..=0x2F).contains(&w) {
        6
    } else if (RARE_CODEPOINT_BITVEC[(w >> 5) as usize] >> (w & 0x1F)) & 1 != 0 {
        30
    } else {
        1
    }
}

#[derive(Clone)]
struct Candidate<'a> {
    enc: &'static Encoding,
    input: &'a [u8],
    demerits: u64,
    state: u32,
    multiplier: f32,
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn init_candidates<'a>(
    encodings: &[&'static Encoding],
    strings: &[&[u8]],
    strict: bool,
    order_significant: bool,
) -> Vec<Candidate<'a>> {
    let length = encodings.len();
    // `emalloc(sizeof(conversion_data) * length)`: past `memory_limit` there
    // are no candidates (no encoding found) and the VM raises the fatal.
    let mut out = Vec::new();
    if php_memlimit::TryGrow::try_grow_exact(&mut out, length).is_err() {
        return out;
    }
    'next: for (i, &enc) in encodings.iter().enumerate() {
        let mut demerits = 0u64;
        if let Some(check) = enc.check {
            for s in strings {
                if !check(s) {
                    if strict {
                        continue 'next;
                    }
                    demerits = demerits.wrapping_add(500);
                }
            }
        }
        let multiplier = if order_significant {
            (1.0 + ((0.3 * i as f64) / length as f64)) as f32
        } else {
            1.0
        };
        out.push(Candidate {
            enc,
            input: &[],
            demerits,
            state: 0,
            multiplier,
        });
    }
    out
}

fn start_string<'a>(array: &mut [Candidate<'a>], input: &'a [u8]) {
    for c in array.iter_mut() {
        c.input = input;
        match c.enc.no {
            EncodingNo::Utf8 if input.starts_with(b"\xEF\xBB\xBF") => c.input = &input[3..],
            EncodingNo::Utf16be if input.starts_with(b"\xFE\xFF") => c.input = &input[2..],
            EncodingNo::Utf16le if input.starts_with(b"\xFF\xFE") => c.input = &input[2..],
            _ => {}
        }
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn count_demerits(array: &mut Vec<Candidate<'_>>, strict: bool) {
    let mut wchar_buf = [0u32; 128];
    let mut finished = array.iter().filter(|c| c.input.is_empty()).count();
    while (strict || array.len() > 1) && finished < array.len() {
        let mut i = array.len();
        while i > 0 {
            i -= 1;
            if array[i].input.is_empty() {
                continue;
            }
            let c = &mut array[i];
            let n = (c.enc.to_wchar)(&mut c.input, &mut wchar_buf, &mut c.state);
            let mut eliminated = false;
            for &w in wchar_buf[..n].iter().rev() {
                if w == BAD_INPUT {
                    if strict {
                        eliminated = true;
                        break;
                    }
                    c.demerits = c.demerits.wrapping_add(1000);
                } else {
                    c.demerits = c.demerits.wrapping_add(estimate_demerits(w));
                }
            }
            if eliminated {
                array.remove(i);
                continue;
            }
            if array[i].input.is_empty() {
                finished += 1;
            }
        }
    }
    for c in array.iter_mut() {
        let d = c.demerits as f64 * f64::from(c.multiplier);
        c.demerits = if d < u64::MAX as f64 {
            d as u64
        } else {
            u64::MAX
        };
    }
}

/// `mb_guess_encoding_for_strings`.
pub fn guess_encoding_for_strings(
    strings: &[&[u8]],
    elist: &[&'static Encoding],
    strict: bool,
    order_significant: bool,
) -> Option<&'static Encoding> {
    match elist.len() {
        0 => return None,
        1 => {
            if strict && !strings.iter().all(|s| check_str_encoding_php(s, elist[0])) {
                return None;
            }
            return Some(elist[0]);
        }
        _ => {}
    }
    if strings.len() == 1 && strings[0].is_empty() {
        return Some(elist[0]);
    }
    let mut array = init_candidates(elist, strings, strict, order_significant);
    for s in strings.iter().rev() {
        start_string(&mut array, s);
        count_demerits(&mut array, strict);
        if array.is_empty() {
            return None;
        }
    }
    let mut best = 0;
    for i in 1..array.len() {
        if array[i].demerits < array[best].demerits {
            best = i;
        }
    }
    Some(array[best].enc)
}

/// `php_mb_check_encoding` (without the UTF-8 fast path; identical result).
fn check_str_encoding_php(s: &[u8], enc: &'static Encoding) -> bool {
    check_str_encoding(s, enc)
}

/// `remove_non_encodings_from_elist`.
fn remove_non_encodings(list: &mut Vec<&'static Encoding>) {
    list.retain(|e| e.no > EncodingNo::CharsetMin);
}

impl MbState {
    /// `mb_detect_encoding($string, $encodings, $strict)`; `Ok(None)` is
    /// `false`. `strict == None` means the argument was not passed
    /// (`mbstring.strict_detection` applies). `is_all_encodings_list` must be
    /// true when the array passed is the very array returned by
    /// `mb_list_encodings()` (PHP then ignores the list order).
    pub fn mb_detect_encoding(
        &mut self,
        s: &[u8],
        encodings: Option<EncodingList<'_>>,
        strict: Option<bool>,
        is_all_encodings_list: bool,
    ) -> MbResult<Option<&'static str>> {
        let order_significant = !is_all_encodings_list;
        let mut elist = match encodings {
            Some(l) => self.parse_list_arg(l, 2, "encodings")?,
            None => self.detect_order().to_vec(),
        };
        if elist.is_empty() {
            return Err(MbError::arg_value(
                2,
                "encodings",
                "must specify at least one encoding",
            ));
        }
        remove_non_encodings(&mut elist);
        if elist.is_empty() {
            return Ok(None);
        }
        let strict = strict.unwrap_or(self.strict_detection);
        Ok(guess_encoding_for_strings(&[s], &elist, strict, order_significant).map(|e| e.name))
    }

    /// Resolve `mb_convert_encoding`'s `$to_encoding` and `$from_encoding`
    /// arguments (for array input the VM then calls
    /// [`MbState::convert_encoding_value`] on every key and string).
    pub fn convert_encoding_args(
        &mut self,
        to: &[u8],
        from: Option<EncodingList<'_>>,
    ) -> MbResult<(&'static Encoding, Vec<&'static Encoding>)> {
        let to_enc = self.get_encoding(Some(to), 2, "to_encoding")?;
        let mut from_list = match from {
            Some(l) => self.parse_list_arg(l, 3, "from_encoding")?,
            None => vec![self.internal_encoding()],
        };
        if from_list.len() > 1 {
            remove_non_encodings(&mut from_list);
        }
        if from_list.is_empty() {
            return Err(MbError::arg_value(
                3,
                "from_encoding",
                "must specify at least one encoding",
            ));
        }
        Ok((to_enc, from_list))
    }

    /// `php_mb_convert_encoding`: convert one string, detecting the source
    /// encoding when several are given. `None` means detection failed (a
    /// warning was emitted; PHP returns `false` / skips the array entry).
    pub fn convert_encoding_value(
        &mut self,
        s: &[u8],
        to: &'static Encoding,
        from: &[&'static Encoding],
    ) -> Option<Vec<u8>> {
        let from_enc = if from.len() == 1 {
            from[0]
        } else if let Some(e) = guess_encoding_for_strings(&[s], from, self.strict_detection, true)
        {
            e
        } else {
            self.warn("Unable to detect character encoding");
            return None;
        };
        Some(self.convert_ex(s, to, from_enc))
    }

    /// `mb_convert_encoding($string, $to, $from)` for a string argument;
    /// `Ok(None)` is `false`.
    pub fn mb_convert_encoding(
        &mut self,
        s: &[u8],
        to: &[u8],
        from: Option<EncodingList<'_>>,
    ) -> MbResult<Option<Vec<u8>>> {
        let (to, from) = self.convert_encoding_args(to, from)?;
        Ok(self.convert_encoding_value(s, to, &from))
    }

    /// Core of `mb_convert_variables`: resolve the encodings, detecting the
    /// source encoding from all the `strings` (every string found
    /// recursively in the variables, in traversal order) when several
    /// candidates are given. Returns `(from, to)`; `Ok(None)` means
    /// detection failed (warning emitted, PHP returns `false`). The VM then
    /// converts each string with [`MbState::convert_ex`] and returns
    /// `from.name`.
    pub fn convert_variables_encodings(
        &mut self,
        to: &[u8],
        from: EncodingList<'_>,
        is_all_encodings_list: bool,
        strings: &[&[u8]],
    ) -> MbResult<Option<(&'static Encoding, &'static Encoding)>> {
        let to_enc = self.get_encoding(Some(to), 1, "to_encoding")?;
        let elist = self.parse_list_arg(from, 2, "from_encoding")?;
        if elist.is_empty() {
            return Err(MbError::arg_value(
                2,
                "from_encoding",
                "must specify at least one encoding",
            ));
        }
        if elist.len() == 1 {
            return Ok(Some((elist[0], to_enc)));
        }
        if let Some(e) = guess_encoding_for_strings(
            strings,
            &elist,
            self.strict_detection,
            !is_all_encodings_list,
        ) {
            Ok(Some((e, to_enc)))
        } else {
            self.warn("Unable to detect encoding");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::by_name;

    fn encs(names: &[&str]) -> Vec<&'static Encoding> {
        names
            .iter()
            .map(|n| by_name(n.as_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn detects_obvious_cases() {
        let l = encs(&["ASCII", "UTF-8", "ISO-8859-1"]);
        let g =
            |s: &[u8], strict| guess_encoding_for_strings(&[s], &l, strict, true).map(|e| e.name);
        assert_eq!(g(b"hello", true), Some("ASCII"));
        assert_eq!(g("héllo".as_bytes(), true), Some("UTF-8"));
        assert_eq!(g(b"h\xE9llo", true), Some("ISO-8859-1"));
    }

    /// GH-17503: demerits are rescaled after every string, so with many
    /// strings they saturate and then wrap (`uint64_t` arithmetic in C).
    #[test]
    fn many_strings_do_not_overflow() {
        let l = encs(&["UTF-8", "UTF-16"]);
        let strings: Vec<&[u8]> = vec![&b"<blah>"[..]; 500];
        assert_eq!(
            guess_encoding_for_strings(&strings, &l, false, true).map(|e| e.name),
            Some("UTF-8")
        );
    }
}
