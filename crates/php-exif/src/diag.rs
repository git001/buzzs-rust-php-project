// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Diagnostics emitted by the exif / image functions.
//!
//! The functions in this crate never print anything: every warning or notice
//! PHP would raise through `php_error_docref()` is returned as a
//! [`Diagnostic`] in emission order.  The message never contains the
//! `"exif_read_data(): "` prefix — the VM adds the name of the PHP function
//! that is running, exactly like `php_error_docref(NULL, ...)` does.

use std::fmt;

/// The PHP error level of a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// `E_WARNING`.
    Warning,
    /// `E_NOTICE`.
    Notice,
}

impl Level {
    /// The `E_*` constant value.
    #[must_use]
    pub fn code(self) -> i64 {
        match self {
            Level::Warning => 2,
            Level::Notice => 8,
        }
    }

    /// The label PHP prints (`"Warning"` / `"Notice"`).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Level::Warning => "Warning",
            Level::Notice => "Notice",
        }
    }
}

/// One warning or notice.  The message is raw bytes because a few messages
/// embed user data (e.g. `getimagesizefromstring()`'s "Error reading from %s!"
/// embeds the image data itself).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Diagnostic {
    /// The error level.
    pub level: Level,
    /// The message without the `function(): ` prefix.
    pub message: Vec<u8>,
}

impl Diagnostic {
    /// Creates a warning.
    pub fn warning(message: impl Into<Vec<u8>>) -> Self {
        Self {
            level: Level::Warning,
            message: message.into(),
        }
    }

    /// Creates a notice.
    pub fn notice(message: impl Into<Vec<u8>>) -> Self {
        Self {
            level: Level::Notice,
            message: message.into(),
        }
    }

    /// The message as (lossy) UTF-8.
    #[must_use]
    pub fn message_lossy(&self) -> String {
        String::from_utf8_lossy(&self.message).into_owned()
    }
}

impl fmt::Debug for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.level.label(), self.message_lossy())
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.level.label(), self.message_lossy())
    }
}
