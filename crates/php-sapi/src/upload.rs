// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The upload error codes (`PHP_UPLOAD_ERROR_*`, `main/rfc1867.h`) and
//! the `UPLOAD_ERR_*` constants: main's own constants (`main.stub.php`),
//! which every SAPI has under `Core`.

/// `UPLOAD_ERR_*` codes (`PHP_UPLOAD_ERROR_*`).
pub mod upload_err {
    /// `UPLOAD_ERR_OK`.
    pub const OK: i64 = 0;
    /// `UPLOAD_ERR_INI_SIZE` (`upload_max_filesize`).
    pub const INI_SIZE: i64 = 1;
    /// `UPLOAD_ERR_FORM_SIZE` (`MAX_FILE_SIZE`).
    pub const FORM_SIZE: i64 = 2;
    /// `UPLOAD_ERR_PARTIAL`.
    pub const PARTIAL: i64 = 3;
    /// `UPLOAD_ERR_NO_FILE`.
    pub const NO_FILE: i64 = 4;
    /// `UPLOAD_ERR_NO_TMP_DIR`.
    pub const NO_TMP_DIR: i64 = 6;
    /// `UPLOAD_ERR_CANT_WRITE`.
    pub const CANT_WRITE: i64 = 7;
    /// `UPLOAD_ERR_EXTENSION` (the upload observer cancelled it).
    pub const EXTENSION: i64 = 8;
}

/// The `UPLOAD_ERR_*` constants in the order of `main.stub.php`.
pub const UPLOAD_ERR_CONSTANTS: &[(&str, i64)] = &[
    ("UPLOAD_ERR_OK", upload_err::OK),
    ("UPLOAD_ERR_INI_SIZE", upload_err::INI_SIZE),
    ("UPLOAD_ERR_FORM_SIZE", upload_err::FORM_SIZE),
    ("UPLOAD_ERR_PARTIAL", upload_err::PARTIAL),
    ("UPLOAD_ERR_NO_FILE", upload_err::NO_FILE),
    ("UPLOAD_ERR_NO_TMP_DIR", upload_err::NO_TMP_DIR),
    ("UPLOAD_ERR_CANT_WRITE", upload_err::CANT_WRITE),
    ("UPLOAD_ERR_EXTENSION", upload_err::EXTENSION),
];
