// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! URI parsing for PHP's `ext/uri`: RFC 3986 (`Uri\Rfc3986\Uri`, modelled on
//! uriparser) and the WHATWG URL Standard (`Uri\WhatWg\Url`, modelled on
//! lexbor's URL module).
//!
//! The crate is independent of the VM: [`api`] exposes one function per PHP
//! method with plain Rust types, and failures are described by
//! [`UriError`] values carrying the PHP class name and message.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod api;
pub mod error;
pub mod rfc3986;
pub mod whatwg;

pub use error::{ErrorClass, ErrorContext, UriError, UrlValidationError, UrlValidationErrorType};

/// `Uri\UriComparisonMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UriComparisonMode {
    /// `Uri\UriComparisonMode::IncludeFragment`.
    IncludeFragment,
    /// `Uri\UriComparisonMode::ExcludeFragment` (the default).
    #[default]
    ExcludeFragment,
}

impl UriComparisonMode {
    /// The enum case name.
    pub fn name(self) -> &'static str {
        match self {
            UriComparisonMode::IncludeFragment => "IncludeFragment",
            UriComparisonMode::ExcludeFragment => "ExcludeFragment",
        }
    }
}
