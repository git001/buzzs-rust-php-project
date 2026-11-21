// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The native extensions linked into the module: a subset of the `php`
//! CLI's (`crates/php-extensions/src/lib.rs`) in the same order, including
//! the stream/file functions (needed for `php://input`) and ext/session
//! (whose `php_rfc1867_callback` the multipart parser reports to), plus
//! the SAPI's own `apache2handler` module, registered last so its
//! `header()`, `flush()`, `php_sapi_name()`, ... replace the CLI stubs.

use php_vm::ext::Extension;

/// All linked extensions, in registration order.
pub fn all() -> Vec<Box<dyn Extension>> {
    vec![
        Box::new(php_ext_stdstr::StdStr),
        Box::new(php_ext_streams::Streams),
        Box::new(php_ext_date::Date),
        Box::new(php_ext_json::Json),
        Box::new(php_ext_hash::Hash),
        Box::new(php_ext_random::Random),
        Box::new(php_ext_filter::Filter),
        Box::new(php_ext_pcre::Pcre),
        Box::new(php_ext_stdarr::StdArray),
        Box::new(php_ext_stdmisc::StdMisc),
        Box::new(php_ext_spl::Spl),
        Box::new(php_ext_pdo::Pdo),
        Box::new(php_ext_session::Session),
        Box::new(crate::functions::Apache2Handler),
    ]
}
