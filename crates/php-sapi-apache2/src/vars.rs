// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! apache2handler's part of the request variables
//! (`php_apache_sapi_register_variables()`, `php_apache_sapi_get_request_time()`);
//! the rest is [`php_sapi_input::vars`].

use crate::backend::{IntField, StrField, Table};
use crate::request::RequestCtx;
use php_runtime::{Value, ZArray, ZStr};
use php_sapi_input::vars::{register_variable_safe, SapiVariables};
use php_vm::ext::ParseBodyOptions;
use php_vm::{Vm, VmResult};

impl SapiVariables for RequestCtx {
    /// The `subprocess_env` table (`ap_add_common_vars()`,
    /// `ap_add_cgi_vars()`), then `PHP_SELF` = `r->uri`.
    fn register_server_variables(&self, vm: &mut Vm, server: &mut ZArray) {
        let b = &*self.backend;
        for (k, v) in b.table_entries(Table::SubprocessEnv) {
            register_variable_safe(vm, &k, Value::Str(ZStr::from(v)), server);
        }
        let uri = b.str_field(StrField::Uri).unwrap_or_default();
        register_variable_safe(vm, b"PHP_SELF", Value::Str(ZStr::from(uri)), server);
    }

    /// `r->request_time` (microseconds).
    fn request_time(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let t = self.backend.int_field(IntField::RequestTime) as f64;
        t / 1_000_000.0
    }
}

/// [`php_vm::ext::ParseBodyFn`]: `request_parse_body()` for the current
/// request.
pub fn parse_body(vm: &mut Vm, options: &ParseBodyOptions) -> VmResult<(ZArray, ZArray)> {
    let ctx = crate::request::current();
    php_sapi_input::vars::parse_body(vm, options, ctx.as_deref())
}
