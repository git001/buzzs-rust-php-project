// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/gd` for the phprust VM: binds the `php-gd` library (`GdImage`,
//! `GdFont`, the `image*` functions and the GD constants) through the
//! `php_vm::ext` extension API. Ref: ext/gd/gd.c, ext/gd/gd.stub.php.

// `r`, `g`, `b`, `a`, `x`, `y`, `w`, `h` mirror gd.c.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::many_single_char_names)]

mod create;
mod draw;
mod io;
mod objects;
mod transform;
mod util;

use php_gd::info::{self, ConstValue};
use php_runtime::Value;
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{Extension, InfoItem, ModuleBuilder};
use php_vm::Vm;

/// The `gd` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Gd;

impl Extension for Gd {
    fn name(&self) -> &'static str {
        "gd"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        m.ini(IniDirective::new(
            "gd.jpeg_ignore_warning",
            Some("1"),
            Access::ALL,
            ModifyHandler::Bool,
            "gd",
        ));
        for (name, v) in info::constants() {
            m.constant(
                name,
                match v {
                    ConstValue::Int(i) => Value::Int(i),
                    ConstValue::Str(s) => Value::string(s),
                },
            );
        }
        objects::register(m);
        create::register(m);
        draw::register(m);
        transform::register(m);
        m.alias("imagettfbbox", "imageftbbox");
        m.alias("imagettftext", "imagefttext");
        m.info(module_info);
        m.on_request_shutdown(|_vm| php_gd::ffi::font_cache_shutdown());
    }
}

/// `PHP_MINFO_FUNCTION(gd)`.
fn module_info(_vm: &mut Vm) -> Vec<InfoItem> {
    let mut out = vec![InfoItem::TableStart];
    for (k, v) in info::minfo_rows() {
        out.push(InfoItem::Row(vec![k.to_string(), v.to_string()]));
    }
    out.push(InfoItem::TableEnd);
    out.push(InfoItem::IniEntries);
    out
}
