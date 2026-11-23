// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/ftp` client engine (`ext/ftp/ftp.c`) without the VM.
//!
//! [`Ftp`] is `ftpbuf_t`: a control connection with the exact reply
//! reader of `ftp_readline`/`ftp_getresp` (4096-byte buffer, CR / LF /
//! CRLF line ends, `"ddd "` final lines of multi-line replies), the
//! commands behind every `ftp_*()` function with their expected reply
//! codes, active (`PORT`/`EPRT`) and passive (`EPSV`/`PASV`, honouring
//! `FTP_USEPASVADDRESS`) data connections, blocking transfers with the
//! ASCII CRLF conversions in both directions, the non-blocking transfer
//! state machine (`FTP_MOREDATA`/`FTP_FINISHED`/`FTP_FAILED`), FTPS
//! (`AUTH TLS`, `PBSZ 0`, `PROT P`, TLS session reuse on data
//! connections) and `MLSD` fact parsing.
//!
//! PHP streams are abstracted as [`Sink`] / [`Source`]; diagnostics are
//! returned as warning texts for the binding to emit with the
//! `ftp_xxx(): ` prefix.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod channel;
mod ftp;
mod list;
mod text;

pub use ftp::{Ftp, NbStatus, Sink, SliceSource, Source, TransferType, BUFSIZE, DEFAULT_TIMEOUT};
pub use list::{
    ascii_get_chunk, ascii_nb_chunk, parse_mlsd_line, split_listing, MlsdEntry, MlsdError,
};

/// `PHP_FTP_OPT_TIMEOUT_SEC`.
pub const OPT_TIMEOUT_SEC: i64 = 0;
/// `PHP_FTP_OPT_AUTOSEEK`.
pub const OPT_AUTOSEEK: i64 = 1;
/// `PHP_FTP_OPT_USEPASVADDRESS`.
pub const OPT_USEPASVADDRESS: i64 = 2;
/// `PHP_FTP_AUTORESUME`.
pub const AUTORESUME: i64 = -1;
/// `PHP_FTP_TIMEOUT_SEC_MAX`: `(uint64_t)((double) ULLONG_MAX / 1000000.0)`.
pub const TIMEOUT_SEC_MAX: u64 = 18_446_744_073_709;

#[cfg(test)]
mod tests;
