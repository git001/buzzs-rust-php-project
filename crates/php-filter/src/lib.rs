// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/filter`: `filter_var()`, `filter_var_array()`,
//! `filter_input()`, `filter_input_array()`, `filter_has_var()`,
//! `filter_list()` and `filter_id()`.
//!
//! References: `ext/filter/filter.c`, `logical_filters.c`,
//! `sanitizing_filters.c`, `callback_filter.c`, `filter_private.h`,
//! `filter.stub.php` (php-src master, including `FILTER_THROW_ON_FAILURE`
//! and the ext/uri-based `FILTER_VALIDATE_URL`).
//!
//! # Overview
//!
//! * [`constants`] — filter ids, flags, the filter list ([`filter_list`],
//!   [`filter_id`]) and the PHP constants (with the deprecations of
//!   `FILTER_SANITIZE_STRING`/`STRIPPED`).
//! * [`engine`] — the PHP-level functions. They take the already
//!   zpp-coerced arguments (`$options` as [`FilterArgs`]) and return
//!   `RtResult<Value>`: warnings ("`filter_var()`: Unknown filter with ID 999")
//!   go through [`RuntimeContext::diagnostic`](php_runtime::RuntimeContext),
//!   `ValueError`/`TypeError` texts are exactly php-src's, and
//!   `Filter\FilterFailedException` is built by
//!   [`FilterHost::filter_failed_exception`].
//! * [`validate`] / [`email`] — the validation filters (int, bool, float,
//!   regexp, domain, url, email, ip, mac); [`sanitize`] — the sanitizing
//!   filters; [`url_parse`] — the `parse_url()` port used by
//!   `FILTER_VALIDATE_URL`; [`html`] — the `htmlentities()` subset used by
//!   `FILTER_SANITIZE_FULL_SPECIAL_CHARS`.
//! * [`FilterHost`] — what the VM supplies: PCRE matching for
//!   `FILTER_VALIDATE_REGEXP`, callables for `FILTER_CALLBACK`, the
//!   exception class, other ext/uri parsers and (optionally) a different
//!   `default_charset` for `htmlentities()`.
//!
//! # VM integration notes
//!
//! * Argument coercion (`int $filter`, `array|int $options`, `bool
//!   $add_empty`) and its `TypeError`s are the VM's zpp job.
//! * `filter_input*()`/`filter_has_var()` read [`FilterInputs`]: the raw
//!   request variables (before `filter.default` is applied). The values
//!   registered in `$_GET` & co. come from [`apply_default_filter`] with the
//!   ini settings parsed by [`parse_default_filter_ini`] (which also reports
//!   the "The filter.default ini setting is deprecated" condition) and
//!   [`parse_default_flags_ini`].

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod constants;
pub mod email;
pub mod engine;
pub mod host;
pub mod html;
mod html_table;
pub mod sanitize;
mod unicode_table;
pub mod url_parse;
pub mod validate;

pub use constants::{filter_id, filter_list, FilterKind};
pub use engine::{
    apply_default_filter, filter_has_var, filter_input, filter_input_array, filter_var,
    filter_var_array, parse_default_filter_ini, parse_default_flags_ini, FilterArgs,
    FilterFunction, FilterInputs,
};
pub use host::{
    FilterHost, UriComponents, UriParseResult, FILTER_EXCEPTION_CLASS,
    FILTER_FAILED_EXCEPTION_CLASS,
};

#[cfg(test)]
mod test_host;

#[cfg(test)]
mod oracle_tests;

#[cfg(test)]
mod phpt_tests;
