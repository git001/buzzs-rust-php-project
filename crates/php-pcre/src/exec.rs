// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Shared match driver and the capture-array builder used by
//! `preg_match()`, `preg_match_all()` and `preg_replace_callback()`.

use std::sync::Arc;

use crate::cache::MatchConfig;
use crate::ffi::{self, MatchContext, MatchData};
use crate::regex::Regex;
use crate::value::{Key, MatchArray, Value};
use crate::{PREG_OFFSET_CAPTURE, PREG_UNMATCHED_AS_NULL};

/// One running match: pattern + subject + PCRE2 match data/context.
pub(crate) struct Exec<'a> {
    pub(crate) re: &'a Regex,
    pub(crate) subject: &'a [u8],
    md: MatchData,
    ctx: MatchContext,
    known_valid_utf8: bool,
}

impl<'a> Exec<'a> {
    /// Allocates match data and a match context; `None` on allocation failure
    /// (PHP reports `PREG_INTERNAL_ERROR`).
    pub(crate) fn new(re: &'a Regex, cfg: &MatchConfig, subject: &'a [u8]) -> Option<Self> {
        let md = MatchData::for_code(re.code(), re.capture_count() + 1)?;
        let ctx = MatchContext::new(cfg.backtrack_limit, cfg.recursion_limit, cfg.jit)?;
        Some(Self {
            re,
            subject,
            md,
            ctx,
            known_valid_utf8: cfg.known_valid_utf8,
        })
    }

    /// Options for the first match of a call starting at `start`: UTF
    /// patterns get their subject validated unless the caller vouched for it
    /// and `start` is not inside a character (`is_known_valid_utf8`);
    /// everything else skips the check.
    pub(crate) fn first_options(&self, start: usize) -> u32 {
        if !self.re.is_utf() || self.skip_utf_check(start) {
            ffi::PCRE2_NO_UTF_CHECK
        } else {
            0
        }
    }

    fn skip_utf_check(&self, start: usize) -> bool {
        self.known_valid_utf8
            && (start == self.subject.len()
                || self.subject.get(start).is_some_and(|b| b & 0xC0 != 0x80))
    }

    /// `pcre2_match()` at `start` with `options`.
    pub(crate) fn run(&mut self, start: usize, options: u32) -> i32 {
        self.md
            .run(self.re.code(), self.subject, start, options, &self.ctx)
    }

    /// Plain follow-up match (subject already validated).
    pub(crate) fn next(&mut self, start: usize) -> i32 {
        self.run(start, ffi::PCRE2_NO_UTF_CHECK)
    }

    /// The retry PHP performs after an empty match.
    pub(crate) fn retry_not_empty(&mut self, start: usize) -> i32 {
        self.run(
            start,
            ffi::PCRE2_NO_UTF_CHECK | ffi::PCRE2_NOTEMPTY_ATSTART | ffi::PCRE2_ANCHORED,
        )
    }

    pub(crate) fn ov(&self) -> &[usize] {
        self.md.ovector()
    }

    /// `(start, end)` of the whole match.
    pub(crate) fn span(&self) -> (usize, usize) {
        let ov = self.ov();
        (ov[0], ov[1])
    }

    pub(crate) fn mark(&self) -> Option<Vec<u8>> {
        self.md.mark()
    }

    /// Bytes of the character at `pos` (`calculate_unit_length`).
    pub(crate) fn unit_length(&self, pos: usize) -> usize {
        self.re.unit_length(self.subject, pos)
    }
}

/// Converts a byte offset to a PHP int.
pub(crate) fn off(o: usize) -> i64 {
    i64::try_from(o).unwrap_or(i64::MAX)
}

/// `populate_match_value_str` + `PCRE2_UNSET` handling.
pub(crate) fn match_value(
    subject: &[u8],
    start: usize,
    end: usize,
    unmatched_as_null: bool,
) -> Value {
    if start == ffi::PCRE2_UNSET {
        if unmatched_as_null {
            Value::Null
        } else {
            Value::Str(Vec::new())
        }
    } else {
        Value::Str(subject[start..end].to_vec())
    }
}

/// `add_offset_pair`'s value: `[match, offset]`, `["", -1]` or `[null, -1]`.
pub(crate) fn offset_pair(
    subject: &[u8],
    start: usize,
    end: usize,
    unmatched_as_null: bool,
) -> Value {
    if start == ffi::PCRE2_UNSET {
        let first = if unmatched_as_null {
            Value::Null
        } else {
            Value::Str(Vec::new())
        };
        Value::pair(first, -1)
    } else {
        Value::pair(Value::Str(subject[start..end].to_vec()), off(start))
    }
}

/// `add_named`: matched values overwrite, unmatched ones never replace an
/// existing entry (so with `/J` the group that matched wins).
fn add_named(arr: &mut MatchArray, name: &Arc<[u8]>, val: Value, unmatched: bool) {
    let key = Key::Name(Arc::clone(name));
    if unmatched {
        arr.add(key, val);
    } else {
        arr.set(key, val);
    }
}

/// `populate_subpat_array`: the `$matches` of `preg_match()`, each set of
/// `preg_match_all(PREG_SET_ORDER)` and the callback argument of
/// `preg_replace_callback()`.
///
/// `count` is PCRE2's return value (highest matched group + 1); groups past
/// it are omitted unless `PREG_UNMATCHED_AS_NULL` is set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn populate_subpat_array(
    subject: &[u8],
    ov: &[usize],
    names: Option<&[Option<Arc<[u8]>>]>,
    num_subpats: usize,
    count: usize,
    mark: Option<Vec<u8>>,
    flags: i64,
) -> MatchArray {
    let offset_capture = flags & PREG_OFFSET_CAPTURE != 0;
    let unmatched_as_null = flags & PREG_UNMATCHED_AS_NULL != 0;
    let mut arr = MatchArray::with_capacity(count + usize::from(mark.is_some()));
    let name_of = |i: usize| names.and_then(|n| n.get(i)).and_then(Option::as_ref);

    for i in 0..count {
        let (s, e) = (ov[2 * i], ov[2 * i + 1]);
        let unset = s == ffi::PCRE2_UNSET;
        let val = if offset_capture {
            offset_pair(subject, s, e, unmatched_as_null)
        } else {
            match_value(subject, s, e, unmatched_as_null)
        };
        if let Some(name) = name_of(i) {
            add_named(&mut arr, name, val.clone(), unset);
        }
        arr.push(val);
    }
    if unmatched_as_null {
        for i in count..num_subpats {
            let val = if offset_capture {
                Value::pair(Value::Null, -1)
            } else {
                Value::Null
            };
            if let Some(name) = name_of(i) {
                // Both branches of PHP end up in zend_hash_add for unmatched groups.
                arr.add(Key::Name(Arc::clone(name)), val.clone());
            }
            arr.push(val);
        }
    }
    if let Some(mark) = mark {
        arr.set(Key::name(b"MARK"), Value::Str(mark));
    }
    arr
}

/// PCRE2 result code → number of filled ovector pairs.
pub(crate) fn pairs(count: i32, num_subpats: usize) -> usize {
    // A zero return means "ovector too small", which PHP treats as all groups
    // (after a notice); our match data is always sized from the pattern.
    usize::try_from(count)
        .ok()
        .filter(|&c| c > 0)
        .unwrap_or(num_subpats)
}
