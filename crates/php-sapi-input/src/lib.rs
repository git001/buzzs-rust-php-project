// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The request input every SAPI shares, in one place: the built-in
//! server, CGI, Apache and the coming `FastCGI` and HTTP servers parse
//! request bodies the same way, so that the limits PHP enforces on them
//! (`max_input_vars`, `max_file_uploads`, `max_multipart_body_parts`,
//! `post_max_size`, `upload_max_filesize`) hold for all of them.
//!
//! * [`rfc1867`]: `multipart/form-data` bodies and file uploads
//!   (`main/rfc1867.c`).

pub mod rfc1867;
