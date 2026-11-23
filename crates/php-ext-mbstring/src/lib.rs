// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/mbstring` for the phprust VM: binds the `php-mbstring` library
//! (libmbfl's encodings and the `mb_*` algorithms) through the
//! `php_vm::ext` extension API.
//!
//! This crate is the glue of `ext/mbstring/mbstring.c` and
//! `php_mbregex.c`: argument handling, array walking
//! (`mb_convert_encoding`/`mb_check_encoding` on arrays, by-reference
//! `mb_convert_variables`), the `mbstring.*` INI table with its on-modify
//! handlers, `mb_parse_str`, `mb_send_mail`, `mb_output_handler`, and the
//! `mb_ereg*` family, which runs on the system Oniguruma (see [`regex`]).

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod arrays;
mod funcs;
mod gpc;
mod ini;
mod mail;
mod mem;
mod multibyte;
mod output;
pub mod regex;
mod state;

use php_runtime::Value;
use php_vm::ext::{Extension, InfoItem, ModuleBuilder};
use php_vm::Vm;

/// The `mbstring` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mbstring;

impl Extension for Mbstring {
    fn name(&self) -> &'static str {
        "mbstring"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        ini::register(m);
        register_constants(m);
        funcs::register(m);
        arrays::register(m);
        mail::register(m);
        gpc::register(m);
        output::register(m);
        regex::register(m);
        ini::register_hooks(m);
        multibyte::register_hooks(m);
        ini::startup(m.vm());
        multibyte::install(m.vm());
        m.info(module_info);
        m.on_request_startup(output::request_startup);
        m.on_request_shutdown(|_| {
            state::with(php_mbstring::MbState::request_shutdown);
            regex::reset();
        });
    }
}

fn register_constants(m: &mut ModuleBuilder<'_>) {
    let cases = [
        ("MB_CASE_UPPER", 0),
        ("MB_CASE_LOWER", 1),
        ("MB_CASE_TITLE", 2),
        ("MB_CASE_FOLD", 3),
        ("MB_CASE_UPPER_SIMPLE", 4),
        ("MB_CASE_LOWER_SIMPLE", 5),
        ("MB_CASE_TITLE_SIMPLE", 6),
        ("MB_CASE_FOLD_SIMPLE", 7),
    ];
    for (name, v) in cases {
        m.constant(name, Value::Int(v));
    }
    m.constant(
        "MB_ONIGURUMA_VERSION",
        Value::from(regex::oniguruma_version()),
    );
}

/// `PHP_MINFO_FUNCTION(mbstring)` (with `PHP_MINFO(mb_regex)`).
fn module_info(vm: &mut Vm) -> Vec<InfoItem> {
    let row = |a: &str, b: &str| InfoItem::Row(vec![a.to_string(), b.to_string()]);
    let translation = if vm.ini_bool("mbstring.encoding_translation") {
        "enabled"
    } else {
        "disabled"
    };
    vec![
        InfoItem::TableStart,
        row("Multibyte Support", "enabled"),
        row("Multibyte string engine", "libmbfl"),
        row("HTTP input encoding translation", translation),
        row("libmbfl version", "1.3.2"),
        InfoItem::TableEnd,
        InfoItem::TableStart,
        InfoItem::Header(vec![
            "mbstring extension makes use of \"streamable kanji code filter and converter\", which is distributed under the GNU Lesser General Public License version 2.1."
                .to_string(),
        ]),
        InfoItem::TableEnd,
        InfoItem::TableStart,
        row("Multibyte (japanese) regex support", "enabled"),
        row(
            "Multibyte regex (oniguruma) version",
            &regex::oniguruma_version(),
        ),
        InfoItem::TableEnd,
        InfoItem::IniEntries,
    ]
}
