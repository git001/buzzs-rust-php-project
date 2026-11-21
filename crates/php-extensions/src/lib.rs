// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The native extensions linked into the `php`, `php-cgi` and `php -S`
//! binaries and the servers, in registration order: one list for every
//! SAPI that loads the full set.
//!
//! Binding crates (`crates/php-ext-*`) add one line here and a dependency
//! in `crates/php-extensions/Cargo.toml`; see `crates/php-vm/EXTENSIONS.md`.
//! The extensions are stateless unit structs (their state lives in the
//! `Vm`), so one static set serves every VM on every thread.

use php_vm::ext::Extension;

/// All linked extensions, in registration order.
pub const ALL: &[&dyn Extension] = &[
    &php_ext_stdstr::StdStr,
    &php_ext_streams::Streams,
    &php_ext_date::Date,
    &php_ext_ctype::Ctype,
    &php_ext_json::Json,
    &php_ext_hash::Hash,
    &php_ext_random::Random,
    &php_ext_filter::Filter,
    &php_ext_pcre::Pcre,
    &php_ext_stdarr::StdArray,
    &php_ext_stdmisc::StdMisc,
    &php_ext_spl::Spl,
    &php_ext_mbstring::Mbstring,
    &php_ext_pdo::Pdo,
    &php_ext_session::Session,
    &php_ext_openssl::OpenSsl,
    &php_ext_sodium::Sodium,
    &php_ext_fileinfo::Fileinfo,
    &php_ext_posix::PosixExt,
    &php_ext_iconv::IconvExt,
    &php_ext_intl::Intl,
    &php_ext_exif::Exif,
    &php_ext_uri::Uri,
    &php_ext_zlib::Zlib,
    &php_ext_zip::Zip,
    &php_ext_phar::Phar,
    &php_ext_gettext::Gettext,
    &php_ext_sysv::Sysvmsg,
    &php_ext_sysv::Sysvsem,
    &php_ext_sysv::Sysvshm,
    &php_ext_sysv::Shmop,
    &php_ext_pcntl::PcntlExt,
    &php_ext_sockets::SocketsExt,
    &php_ext_calendar::Calendar,
    &php_ext_bcmath::Bcmath,
    &php_ext_mysqli::Mysqli,
    &php_ext_gmp::Gmp,
    &php_ext_xml::Libxml,
    &php_ext_xml::Xml,
    &php_ext_xml::XmlWriter,
    &php_ext_soap::Soap,
    &php_ext_xmlreader::XmlReaderExt,
    &php_ext_simplexml::SimpleXml,
    &php_ext_dom::Dom,
    &php_ext_xsl::Xsl,
    &php_ext_tokenizer::Tokenizer,
    &php_ext_ftp::Ftp,
    &php_ext_curl::Curl,
    &php_ext_gd::Gd,
    &php_ext_zendtest::ZendTest,
    &php_ext_opcache::Opcache,
];

/// [`ALL`], for the SAPIs that take the set as a function.
#[must_use]
pub fn all() -> &'static [&'static dyn Extension] {
    ALL
}
