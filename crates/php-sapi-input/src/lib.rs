// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The request input every SAPI shares, in one place: the built-in
//! server, CGI, Apache and the coming `FastCGI` and HTTP servers parse
//! request bodies the same way, so that the limits PHP enforces on them
//! (`max_input_vars`, `max_file_uploads`, `max_multipart_body_parts`,
//! `post_max_size`, `upload_max_filesize`) hold for all of them.
//!
//! * [`body`]: the request body as the SAPI delivers it
//!   (`sapi_read_post_block()`, the request body buffer, `php://input`).
//! * [`vars`]: the superglobals and `request_parse_body()` (the SAPI side
//!   of `main/php_variables.c` and `main/SAPI.c`).
//! * [`rfc1867`]: `multipart/form-data` bodies and file uploads
//!   (`main/rfc1867.c`).
//!
//! A SAPI implements [`body::SapiInput`] and [`vars::SapiVariables`] for
//! its request, fills `vm.request_info`, and calls [`vars::populate`].

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

pub mod body;
pub mod rfc1867;
pub mod vars;

/// The SAPI side of the request body (`sapi_module.read_post`), which
/// [`body::SapiInput::post`] returns.
pub use php_sapi::SapiPost;
