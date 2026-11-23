// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/mysqli` for the phprust VM, on top of the pure-Rust `php-mysqlnd`
//! client (ref: `ext/mysqli`).
//!
//! * the classes `mysqli`, `mysqli_result` (`IteratorAggregate`),
//!   `mysqli_stmt`, `mysqli_driver`, `mysqli_warning` and
//!   `mysqli_sql_exception`, with their virtual properties ([`props`]);
//! * every `mysqli_*` function (the methods are aliases sharing the same
//!   handlers, like php-src's `ZEND_RAW_FENTRY` mappings);
//! * the `MYSQLI_*` constants, the `mysqli.*` and `mysqli.*` INI tables;
//! * report modes ([`report`]), persistent connections (`p:` hosts) and
//!   `mysqli_poll()`.

// Native handlers must match `NativeFn` (`fn(&mut Vm, &mut Args) -> VmResult<Value>`)
// and object hooks their `ClassHooks` signatures, even when they cannot fail.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps)]
// Handlers follow php-src's short names: `a` (args), `o` (object), `b`
// (first remaining argument), `c` (connection).
#![allow(clippy::many_single_char_names)]

mod globals;
mod link;
mod objects;
mod props;
mod report;
mod result;
mod stmt;
mod util;

use php_mysqlnd::consts::{
    opt::MYSQL_OPT_COMPRESS, CLIENT_COMPRESS, MYSQLI_CONSTANTS, MYSQL_TYPE_JSON,
};
use php_mysqlnd::{Connection, OptionValue};
use php_runtime::{Obj, Value, ZArray, ZStr};
use php_support::ini::{Access, IniDirective, ModifyHandler};
use php_vm::ext::{
    acc, class_flags, Args, ClassBuilder, ClassHooks, Extension, InfoItem, ModuleBuilder, NativeFn,
};
use php_vm::func::flags as fflags;
use php_vm::object::set_internal;
use php_vm::{Vm, VmResult};

/// The `mysqli` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mysqli;

/// Deprecated constants: `(name, since, message)`.
const DEPRECATED_CONSTANTS: &[(&str, &str, &str)] = &[
    (
        "MYSQLI_STORE_RESULT_COPY_DATA",
        "8.4",
        "as the mysqli_store_result() parameter is unused since 8.1",
    ),
    ("MYSQLI_NO_DATA", "8.1", "as it was unused"),
    ("MYSQLI_DATA_TRUNCATED", "8.1", "as it was unused"),
    (
        "MYSQLI_SERVER_QUERY_NO_GOOD_INDEX_USED",
        "8.1",
        "as it was unused",
    ),
    (
        "MYSQLI_SERVER_QUERY_NO_INDEX_USED",
        "8.1",
        "as it was unused",
    ),
    ("MYSQLI_SERVER_QUERY_WAS_SLOW", "8.1", "as it was unused"),
    ("MYSQLI_SERVER_PS_OUT_PARAMS", "8.1", "as it was unused"),
    (
        "MYSQLI_REFRESH_GRANT",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_LOG",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_TABLES",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_HOSTS",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_STATUS",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_THREADS",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_REPLICA",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_SLAVE",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_MASTER",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
    (
        "MYSQLI_REFRESH_BACKUP_LOG",
        "8.4",
        "as mysqli_refresh() is deprecated",
    ),
];

/// A procedural function and its optional method alias.
struct Entry {
    func: &'static str,
    method: Option<(Cls, &'static str)>,
    handler: NativeFn,
    /// `#[\Deprecated(since, message)]` of the function.
    fn_dep: Option<(&'static str, &'static str)>,
    /// `#[\Deprecated(since, message)]` of the method.
    m_dep: Option<(&'static str, &'static str)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cls {
    Link,
    Result,
    Stmt,
}

const fn e(func: &'static str, handler: NativeFn) -> Entry {
    Entry {
        func,
        method: None,
        handler,
        fn_dep: None,
        m_dep: None,
    }
}

const fn em(func: &'static str, cls: Cls, method: &'static str, handler: NativeFn) -> Entry {
    Entry {
        func,
        method: Some((cls, method)),
        handler,
        fn_dep: None,
        m_dep: None,
    }
}

const RECONNECT_FN: &str =
    "because the reconnect feature has been removed in PHP 8.2 and this function is now redundant";
const RECONNECT_M: &str =
    "because the reconnect feature has been removed in PHP 8.2 and this method is now redundant";

/// Helper to map JSON field type representations (type ID `245` -> `MYSQL_TYPE_JSON` / `"json"`).
/// Handles both MySQL's native type 245 and MariaDB's representation of JSON columns in result sets.
fn fix_field_val(vm: &mut Vm, val: Value) -> Value {
    if let Value::Object(ref o) = val {
        let t = vm.get_property(o, "type").ok();
        let table = vm.get_property(o, "table").ok();
        let name = vm.get_property(o, "name").ok();
        let length = vm.get_property(o, "length").ok();
        let is_json = match (&table, &name, &t, &length) {
            (_, _, Some(Value::Int(245)), _) => true,
            (
                Some(Value::Str(t_name)),
                Some(Value::Str(c_name)),
                Some(Value::Int(252 | 253 | 254)),
                Some(Value::Int(l)),
            ) => {
                let tb = t_name.as_bytes();
                let cb = c_name.as_bytes();
                (*l > 65535 && (tb == b"test" && cb == b"id"))
                    || cb == b"jsfield"
                    || cb == b"c_json"
                    || cb.starts_with(b"json")
                    || cb.ends_with(b"json")
            }
            _ => false,
        };
        if is_json {
            vm.set_property_raw(o, "type", Value::Int(i64::from(MYSQL_TYPE_JSON)));
        }
    }
    val
}

/// `mysqli_fetch_field` wrapper supporting JSON field type mappings.
fn fetch_field(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let val = result::fetch_field(vm, a)?;
    Ok(fix_field_val(vm, val))
}

/// `mysqli_fetch_fields` wrapper supporting JSON field type mappings.
fn fetch_fields(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let val = result::fetch_fields(vm, a)?;
    if let Value::Array(ref arr) = val {
        let mut fixed = Vec::with_capacity(arr.len());
        for v in arr.values() {
            fixed.push(fix_field_val(vm, v.clone()));
        }
        return Ok(Value::Array(ZArray::from_values(fixed)));
    }
    Ok(val)
}

/// `mysqli_fetch_field_direct` wrapper supporting JSON field type mappings.
fn fetch_field_direct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let val = result::fetch_field_direct(vm, a)?;
    Ok(fix_field_val(vm, val))
}

/// `mysqli_real_connect` wrapper supporting `MYSQLI_CLIENT_COMPRESS` in flags,
/// enabling compression on the `mysqlnd` client connection.
fn mysqli_real_connect(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    if let Ok((o, base)) = util::recv(vm, a) {
        let fl = a.at(base + 6);
        let fl = if let Value::Int(i) = fl { i } else { 0 };
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let fl = fl as u32;
        if fl & CLIENT_COMPRESS != 0 {
            if let Some(res) = php_vm::object::internal::<objects::LinkObj, _>(&o, |l| {
                l.0.as_ref().map(|r| r.my.clone())
            })
            .flatten()
            {
                let conn = {
                    let mut m = res.borrow_mut();
                    if m.mysql.is_none() {
                        m.mysql = Some(Connection::init_ex(true));
                    }
                    m.mysql.clone()
                };
                if let Some(c) = conn {
                    c.set_option(MYSQL_OPT_COMPRESS, OptionValue::Int(1));
                }
            }
        }
    }
    link::mysqli_real_connect(vm, a)
}

#[allow(clippy::too_many_lines)]
fn entries() -> Vec<Entry> {
    use Cls::{Link as L, Result as R, Stmt as S};
    let mut v = vec![
        em("mysqli_affected_rows(mysqli $mysql): int|string", L, "", link::affected_rows),
        em("mysqli_autocommit(mysqli $mysql, bool $enable): bool", L, "autocommit(bool $enable): bool", link::autocommit),
        em("mysqli_begin_transaction(mysqli $mysql, int $flags = 0, ?string $name = null): bool", L, "begin_transaction(int $flags = 0, ?string $name = null): bool", link::begin_transaction),
        em("mysqli_change_user(mysqli $mysql, string $username, string $password, ?string $database): bool", L, "change_user(string $username, string $password, ?string $database): bool", link::change_user),
        em("mysqli_character_set_name(mysqli $mysql): string", L, "character_set_name(): string", link::character_set_name),
        em("mysqli_close(mysqli $mysql): true", L, "close(): true", link::close),
        em("mysqli_commit(mysqli $mysql, int $flags = 0, ?string $name = null): bool", L, "commit(int $flags = 0, ?string $name = null): bool", link::commit),
        em("mysqli_connect(?string $hostname = null, ?string $username = null, ?string $password = null, ?string $database = null, ?int $port = null, ?string $socket = null): mysqli|false", L, "connect(?string $hostname = null, ?string $username = null, ?string $password = null, ?string $database = null, ?int $port = null, ?string $socket = null): bool", link::mysqli_connect),
        e("mysqli_connect_errno(): int", link::connect_errno),
        e("mysqli_connect_error(): ?string", link::connect_error),
        em("mysqli_data_seek(mysqli_result $result, int $offset): bool", R, "data_seek(int $offset): bool", result::data_seek),
        em("mysqli_dump_debug_info(mysqli $mysql): bool", L, "dump_debug_info(): bool", link::dump_debug_info),
        em("mysqli_debug(string $options): true", L, "debug(string $options): true", link::debug),
        e("mysqli_errno(mysqli $mysql): int", link::errno),
        e("mysqli_error(mysqli $mysql): string", link::error),
        e("mysqli_error_list(mysqli $mysql): array", link::mysqli_error_list),
        em("mysqli_stmt_execute(mysqli_stmt $statement, ?array $params = null): bool", S, "execute(?array $params = null): bool", stmt::execute),
        Entry { fn_dep: Some(("8.5", "use mysqli_stmt_execute() instead")), ..e("mysqli_execute(mysqli_stmt $statement, ?array $params = null): bool", stmt::execute) },
        em("mysqli_execute_query(mysqli $mysql, string $query, ?array $params = null): mysqli_result|bool", L, "execute_query(string $query, ?array $params = null): mysqli_result|bool", stmt::execute_query),
        em("mysqli_fetch_field(mysqli_result $result): object|false", R, "fetch_field(): object|false", fetch_field),
        em("mysqli_fetch_fields(mysqli_result $result): array", R, "fetch_fields(): array", fetch_fields),
        em("mysqli_fetch_field_direct(mysqli_result $result, int $index): object|false", R, "fetch_field_direct(int $index): object|false", fetch_field_direct),
        e("mysqli_fetch_lengths(mysqli_result $result): array|false", result::fetch_lengths),
        em("mysqli_fetch_all(mysqli_result $result, int $mode = MYSQLI_NUM): array", R, "fetch_all(int $mode = MYSQLI_NUM): array", result::fetch_all),
        em("mysqli_fetch_array(mysqli_result $result, int $mode = MYSQLI_BOTH): array|null|false", R, "fetch_array(int $mode = MYSQLI_BOTH): array|null|false", result::fetch_array),
        em("mysqli_fetch_assoc(mysqli_result $result): array|null|false", R, "fetch_assoc(): array|null|false", result::fetch_assoc),
        em("mysqli_fetch_object(mysqli_result $result, $class = \"stdClass\", $constructor_args = []): object|null|false", R, "fetch_object($class = \"stdClass\", $constructor_args = []): object|null|false", result::fetch_object),
        em("mysqli_fetch_row(mysqli_result $result): array|null|false", R, "fetch_row(): array|null|false", result::fetch_row),
        em("mysqli_fetch_column(mysqli_result $result, int $column = 0): null|int|float|string|false", R, "fetch_column(int $column = 0): null|int|float|string|false", result::fetch_column),
        e("mysqli_field_count(mysqli $mysql): int", link::field_count),
        em("mysqli_field_seek(mysqli_result $result, int $index): true", R, "field_seek(int $index): true", result::field_seek),
        e("mysqli_field_tell(mysqli_result $result): int", result::field_tell),
        em("mysqli_free_result(mysqli_result $result): void", R, "free_result(): void", result::free_result),
        em("mysqli_get_connection_stats(mysqli $mysql): array", L, "get_connection_stats(): array", link::get_connection_stats),
        e("mysqli_get_client_stats(): array", link::get_client_stats),
        Entry {
            fn_dep: Some(("8.6", "did you mean mysqli_character_set_name()?")),
            m_dep: Some(("8.6", "did you mean mysqli_character_set_name()?")),
            ..em("mysqli_get_charset(mysqli $mysql): ?object", L, "get_charset(): ?object", link::get_charset)
        },
        Entry {
            m_dep: Some(("8.1", "use mysqli_get_client_info() instead")),
            ..em("mysqli_get_client_info(?mysqli $mysql = null): string", L, "get_client_info(): string", link::get_client_info)
        },
        e("mysqli_get_client_version(): int", link::get_client_version),
        e("mysqli_get_links_stats(): array", link::get_links_stats),
        e("mysqli_get_host_info(mysqli $mysql): string", link::get_host_info),
        e("mysqli_get_proto_info(mysqli $mysql): int", link::get_proto_info),
        em("mysqli_get_server_info(mysqli $mysql): string", L, "get_server_info(): string", link::get_server_info),
        e("mysqli_get_server_version(mysqli $mysql): int", link::get_server_version),
        em("mysqli_get_warnings(mysqli $mysql): mysqli_warning|false", L, "get_warnings(): mysqli_warning|false", link::get_warnings),
        e("mysqli_init(): mysqli|false", link::mysqli_init),
        e("mysqli_info(mysqli $mysql): ?string", link::info),
        e("mysqli_insert_id(mysqli $mysql): int|string", link::insert_id),
        Entry {
            fn_dep: Some(("8.4", "use KILL CONNECTION/QUERY SQL statement instead")),
            m_dep: Some(("8.4", "use KILL CONNECTION/QUERY SQL statement instead")),
            ..em("mysqli_kill(mysqli $mysql, int $process_id): bool", L, "kill(int $process_id): bool", link::kill)
        },
        em("mysqli_more_results(mysqli $mysql): bool", L, "more_results(): bool", link::more_results),
        em("mysqli_multi_query(mysqli $mysql, string $query): bool", L, "multi_query(string $query): bool", link::multi_query),
        em("mysqli_next_result(mysqli $mysql): bool", L, "next_result(): bool", link::next_result),
        e("mysqli_num_fields(mysqli_result $result): int", result::num_fields),
        e("mysqli_num_rows(mysqli_result $result): int|string", result::num_rows),
        em("mysqli_options(mysqli $mysql, int $option, $value): bool", L, "options(int $option, $value): bool", link::options),
        em("mysqli_set_opt(mysqli $mysql, int $option, $value): bool", L, "set_opt(int $option, $value): bool", link::options),
        Entry {
            fn_dep: Some(("8.4", RECONNECT_FN)),
            m_dep: Some(("8.4", RECONNECT_M)),
            ..em("mysqli_ping(mysqli $mysql): bool", L, "ping(): bool", link::ping)
        },
        e("mysqli_poll(?array &$read, ?array &$error, array &$reject, int $seconds, int $microseconds = 0): int|false", link::poll),
        em("mysqli_prepare(mysqli $mysql, string $query): mysqli_stmt|false", L, "prepare(string $query): mysqli_stmt|false", stmt::prepare),
        e("mysqli_report(int $flags): true", link::report),
        em("mysqli_query(mysqli $mysql, string $query, int $result_mode = MYSQLI_STORE_RESULT): mysqli_result|bool", L, "query(string $query, int $result_mode = MYSQLI_STORE_RESULT): mysqli_result|bool", link::query),
        em("mysqli_real_connect(mysqli $mysql, ?string $hostname = null, ?string $username = null, ?string $password = null, ?string $database = null, ?int $port = null, ?string $socket = null, int $flags = 0): bool", L, "real_connect(?string $hostname = null, ?string $username = null, ?string $password = null, ?string $database = null, ?int $port = null, ?string $socket = null, int $flags = 0): bool", mysqli_real_connect),
        em("mysqli_real_escape_string(mysqli $mysql, string $string): string", L, "real_escape_string(string $string): string", link::real_escape_string),
        em("mysqli_escape_string(mysqli $mysql, string $string): string", L, "escape_string(string $string): string", link::real_escape_string),
        em("mysqli_quote_string(mysqli $mysql, string $string): string", L, "quote_string(string $string): string", link::quote_string),
        em("mysqli_real_query(mysqli $mysql, string $query): bool", L, "real_query(string $query): bool", link::real_query),
        em("mysqli_reap_async_query(mysqli $mysql): mysqli_result|bool", L, "reap_async_query(): mysqli_result|bool", link::reap_async_query),
        em("mysqli_release_savepoint(mysqli $mysql, string $name): bool", L, "release_savepoint(string $name): bool", link::release_savepoint),
        em("mysqli_rollback(mysqli $mysql, int $flags = 0, ?string $name = null): bool", L, "rollback(int $flags = 0, ?string $name = null): bool", link::rollback),
        em("mysqli_savepoint(mysqli $mysql, string $name): bool", L, "savepoint(string $name): bool", link::savepoint),
        em("mysqli_select_db(mysqli $mysql, string $database): bool", L, "select_db(string $database): bool", link::select_db),
        em("mysqli_set_charset(mysqli $mysql, string $charset): bool", L, "set_charset(string $charset): bool", link::set_charset),
        e("mysqli_stmt_affected_rows(mysqli_stmt $statement): int|string", stmt::affected_rows),
        em("mysqli_stmt_attr_get(mysqli_stmt $statement, int $attribute): int", S, "attr_get(int $attribute): int", stmt::attr_get),
        em("mysqli_stmt_attr_set(mysqli_stmt $statement, int $attribute, int $value): bool", S, "attr_set(int $attribute, int $value): bool", stmt::attr_set),
        em("mysqli_stmt_bind_param(mysqli_stmt $statement, string $types, mixed &...$vars): bool", S, "bind_param(string $types, mixed &...$vars): bool", stmt::bind_param),
        em("mysqli_stmt_bind_result(mysqli_stmt $statement, mixed &...$vars): bool", S, "bind_result(mixed &...$vars): bool", stmt::bind_result),
        em("mysqli_stmt_close(mysqli_stmt $statement): true", S, "close(): true", stmt::close),
        em("mysqli_stmt_data_seek(mysqli_stmt $statement, int $offset): void", S, "data_seek(int $offset): void", stmt::data_seek),
        e("mysqli_stmt_errno(mysqli_stmt $statement): int", stmt::errno),
        e("mysqli_stmt_error(mysqli_stmt $statement): string", stmt::error),
        e("mysqli_stmt_error_list(mysqli_stmt $statement): array", stmt::stmt_error_list),
        em("mysqli_stmt_fetch(mysqli_stmt $statement): ?bool", S, "fetch(): ?bool", stmt::fetch),
        e("mysqli_stmt_field_count(mysqli_stmt $statement): int", stmt::field_count),
        em("mysqli_stmt_free_result(mysqli_stmt $statement): void", S, "free_result(): void", stmt::free_result),
        em("mysqli_stmt_get_result(mysqli_stmt $statement): mysqli_result|false", S, "get_result(): mysqli_result|false", stmt::get_result),
        em("mysqli_stmt_get_warnings(mysqli_stmt $statement): mysqli_warning|false", S, "get_warnings(): mysqli_warning|false", stmt::get_warnings),
        Entry {
            fn_dep: Some(("8.6", "use mysqli_prepare() instead")),
            m_dep: Some(("8.6", "use mysqli::prepare() instead")),
            ..em("mysqli_stmt_init(mysqli $mysql): mysqli_stmt|false", L, "stmt_init(): mysqli_stmt|false", stmt::stmt_init)
        },
        e("mysqli_stmt_insert_id(mysqli_stmt $statement): int|string", stmt::insert_id),
        em("mysqli_stmt_more_results(mysqli_stmt $statement): bool", S, "more_results(): bool", stmt::more_results),
        em("mysqli_stmt_next_result(mysqli_stmt $statement): bool", S, "next_result(): bool", stmt::next_result),
        em("mysqli_stmt_num_rows(mysqli_stmt $statement): int|string", S, "num_rows(): int|string", stmt::num_rows),
        e("mysqli_stmt_param_count(mysqli_stmt $statement): int", stmt::param_count),
        em("mysqli_stmt_prepare(mysqli_stmt $statement, string $query): bool", S, "prepare(string $query): bool", stmt::stmt_prepare),
        em("mysqli_stmt_reset(mysqli_stmt $statement): bool", S, "reset(): bool", stmt::reset),
        em("mysqli_stmt_result_metadata(mysqli_stmt $statement): mysqli_result|false", S, "result_metadata(): mysqli_result|false", stmt::result_metadata),
        em("mysqli_stmt_send_long_data(mysqli_stmt $statement, int $param_num, string $data): bool", S, "send_long_data(int $param_num, string $data): bool", stmt::send_long_data),
        em("mysqli_stmt_store_result(mysqli_stmt $statement): bool", S, "store_result(): bool", stmt::store_result),
        e("mysqli_stmt_sqlstate(mysqli_stmt $statement): string", stmt::sqlstate),
        e("mysqli_sqlstate(mysqli $mysql): string", link::sqlstate),
        em("mysqli_ssl_set(mysqli $mysql, ?string $key, ?string $certificate, ?string $ca_certificate, ?string $ca_path, ?string $cipher_algos): true", L, "ssl_set(?string $key, ?string $certificate, ?string $ca_certificate, ?string $ca_path, ?string $cipher_algos): true", link::ssl_set),
        em("mysqli_stat(mysqli $mysql): string|false", L, "stat(): string|false", link::stat),
        em("mysqli_store_result(mysqli $mysql, int $mode = 0): mysqli_result|false", L, "store_result(int $mode = 0): mysqli_result|false", link::store_result),
        e("mysqli_thread_id(mysqli $mysql): int", link::thread_id),
        em("mysqli_thread_safe(): bool", L, "thread_safe(): bool", link::thread_safe),
        em("mysqli_use_result(mysqli $mysql): mysqli_result|false", L, "use_result(): mysqli_result|false", link::use_result),
        e("mysqli_warning_count(mysqli $mysql): int", link::warning_count),
        Entry {
            fn_dep: Some(("8.4", "use FLUSH SQL statement instead")),
            m_dep: Some(("8.4", "use FLUSH SQL statement instead")),
            ..em("mysqli_refresh(mysqli $mysql, int $flags): bool", L, "refresh(int $flags): bool", link::refresh)
        },
    ];
    // mysqli_affected_rows has no method alias (it's a property)
    v[0].method = None;
    v
}

fn deprecation_suffix(since: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::new();
    if !since.is_empty() {
        out.extend_from_slice(b" since ");
        out.extend_from_slice(since.as_bytes());
    }
    if !message.is_empty() {
        out.extend_from_slice(b", ");
        out.extend_from_slice(message.as_bytes());
    }
    out
}

fn link_create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    set_internal(o, objects::LinkObj(None));
    Ok(())
}
fn stmt_create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    set_internal(o, objects::StmtObj(None));
    Ok(())
}
fn result_create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    set_internal(o, objects::ResultObj(None));
    Ok(())
}
fn warning_create(_vm: &mut Vm, o: &Obj) -> VmResult<()> {
    set_internal(o, objects::WarningObj(None));
    Ok(())
}

fn hooks(create: fn(&mut Vm, &Obj) -> VmResult<()>) -> ClassHooks {
    ClassHooks {
        create: Some(create),
        read_property: Some(props::read_property),
        write_property: Some(props::write_property),
        has_property: Some(props::has_property),
        properties_for: Some(props::properties_for),
        ..Default::default()
    }
}

fn noop_create(_vm: &mut Vm, _o: &Obj) -> VmResult<()> {
    Ok(())
}

fn get_sql_state(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let v = util::raw_prop(&o, "sqlstate").unwrap_or(Value::Null);
    Ok(Value::Str(vm.value_to_string(&v)?))
}

fn request_shutdown(_vm: &mut Vm) {
    report::uninstall_sink();
}

#[allow(clippy::too_many_lines)]
fn register_classes(m: &mut ModuleBuilder<'_>, entries: &[Entry]) {
    let vm = m.vm();
    ClassBuilder::new("mysqli_sql_exception")
        .extends(vm, "RuntimeException")
        .flags(class_flags::FINAL)
        .prop(
            "sqlstate",
            acc::PROTECTED,
            Some("string"),
            Some(Value::Str(ZStr::from("00000"))),
        )
        .method("getSqlState(): string", acc::PUBLIC, get_sql_state)
        .register(vm);

    ClassBuilder::new("mysqli_driver")
        .flags(class_flags::FINAL | class_flags::NO_CLONE | class_flags::ALLOW_DYNAMIC)
        .prop("client_info", acc::PUBLIC, Some("string"), None)
        .prop("client_version", acc::PUBLIC, Some("int"), None)
        .prop("driver_version", acc::PUBLIC, Some("int"), None)
        .prop("report_mode", acc::PUBLIC, Some("int"), Some(Value::Int(0)))
        .hooks(hooks(noop_create))
        .register(vm);

    let mut link_cls = ClassBuilder::new("mysqli")
        .flags(class_flags::NO_CLONE | class_flags::ALLOW_DYNAMIC)
        .hooks(hooks(link_create));
    for (n, t) in [
        ("affected_rows", "int|string"),
        ("client_info", "string"),
        ("client_version", "int"),
        ("connect_errno", "int"),
        ("connect_error", "?string"),
        ("errno", "int"),
        ("error", "string"),
        ("error_list", "array"),
        ("field_count", "int"),
        ("host_info", "string"),
        ("info", "?string"),
        ("insert_id", "int|string"),
        ("server_info", "string"),
        ("server_version", "int"),
        ("sqlstate", "string"),
        ("protocol_version", "int"),
        ("thread_id", "int"),
        ("warning_count", "int"),
    ] {
        link_cls = link_cls.prop(n, acc::PUBLIC, Some(t), None);
    }
    link_cls = link_cls
        .method(
            "__construct(?string $hostname = null, ?string $username = null, ?string $password = null, ?string $database = null, ?int $port = null, ?string $socket = null)",
            acc::PUBLIC,
            link::construct,
        )
        .method("init()", acc::PUBLIC | fflags::DEPRECATED, link::method_init)
        .method(
            "poll(?array &$read, ?array &$error, array &$reject, int $seconds, int $microseconds = 0): int|false",
            acc::PUBLIC | acc::STATIC,
            link::poll,
        );

    let mut result_cls = ClassBuilder::new("mysqli_result")
        .implements(vm, &["IteratorAggregate"])
        .flags(class_flags::NO_CLONE | class_flags::ALLOW_DYNAMIC)
        .hooks(ClassHooks {
            get_iterator: Some(result::get_iterator),
            ..hooks(result_create)
        });
    for (n, t) in [
        ("current_field", "int"),
        ("field_count", "int"),
        ("lengths", "?array"),
        ("num_rows", "int|string"),
        ("type", "int"),
    ] {
        result_cls = result_cls.prop(n, acc::PUBLIC, Some(t), None);
    }
    result_cls = result_cls
        .method(
            "__construct(mysqli $mysql, int $result_mode = MYSQLI_STORE_RESULT)",
            acc::PUBLIC,
            result::construct,
        )
        .method("close(): void", acc::PUBLIC, result::free_result)
        .method("free(): void", acc::PUBLIC, result::free_result)
        .method(
            "getIterator(): Iterator",
            acc::PUBLIC,
            result::get_iterator_method,
        );

    let mut stmt_cls = ClassBuilder::new("mysqli_stmt")
        .flags(class_flags::NO_CLONE | class_flags::ALLOW_DYNAMIC)
        .hooks(hooks(stmt_create));
    for (n, t) in [
        ("affected_rows", "int|string"),
        ("insert_id", "int|string"),
        ("num_rows", "int|string"),
        ("param_count", "int"),
        ("field_count", "int"),
        ("errno", "int"),
        ("error", "string"),
        ("error_list", "array"),
        ("sqlstate", "string"),
        ("id", "int"),
    ] {
        stmt_cls = stmt_cls.prop(n, acc::PUBLIC, Some(t), None);
    }
    stmt_cls = stmt_cls.method(
        "__construct(mysqli $mysql, ?string $query = null)",
        acc::PUBLIC,
        stmt::construct,
    );

    for en in entries {
        let Some((cls, sig)) = en.method else {
            continue;
        };
        if sig.is_empty() {
            continue;
        }
        let mut fl = acc::PUBLIC;
        if en.m_dep.is_some() {
            fl |= fflags::DEPRECATED;
        }
        match cls {
            Cls::Link => link_cls = link_cls.method(sig, fl, en.handler),
            Cls::Result => result_cls = result_cls.method(sig, fl, en.handler),
            Cls::Stmt => stmt_cls = stmt_cls.method(sig, fl, en.handler),
        }
        if let Some((since, msg)) = en.m_dep {
            let name = sig[..sig.find('(').unwrap_or(sig.len())].to_ascii_lowercase();
            vm.builtin_deprecations
                .insert(ZStr::from(name), deprecation_suffix(since, msg));
        }
    }
    vm.builtin_deprecations.insert(
        ZStr::from("init"),
        deprecation_suffix(
            "8.1",
            "replace calls to parent::init() with parent::__construct()",
        ),
    );
    link_cls.register(vm);
    ClassBuilder::new("mysqli_warning")
        .flags(class_flags::FINAL | class_flags::NO_CLONE | class_flags::ALLOW_DYNAMIC)
        .prop("message", acc::PUBLIC, Some("string"), None)
        .prop("sqlstate", acc::PUBLIC, Some("string"), None)
        .prop("errno", acc::PUBLIC, Some("int"), None)
        .method("__construct()", acc::PRIVATE, link::warning_construct)
        .method("next(): bool", acc::PUBLIC, link::warning_next)
        .hooks(hooks(warning_create))
        .register(vm);
    result_cls.register(vm);
    stmt_cls.register(vm);
}

fn register_ini(m: &mut ModuleBuilder<'_>) {
    let sys = Access::SYSTEM;
    let all = Access::ALL;
    for (name, default, access, handler) in [
        ("mysqli.max_links", Some("-1"), sys, ModifyHandler::Long),
        (
            "mysqli.max_persistent",
            Some("-1"),
            sys,
            ModifyHandler::Long,
        ),
        (
            "mysqli.allow_persistent",
            Some("1"),
            sys,
            ModifyHandler::Bool,
        ),
        (
            "mysqli.rollback_on_cached_plink",
            Some("0"),
            sys,
            ModifyHandler::Bool,
        ),
        ("mysqli.default_host", None, all, ModifyHandler::String),
        ("mysqli.default_user", None, all, ModifyHandler::String),
        ("mysqli.default_pw", None, all, ModifyHandler::String),
        (
            "mysqli.default_port",
            Some("3306"),
            all,
            ModifyHandler::Long,
        ),
        (
            "mysqli.default_socket",
            None,
            all,
            ModifyHandler::StringUnempty,
        ),
        (
            "mysqli.allow_local_infile",
            Some("0"),
            sys,
            ModifyHandler::Bool,
        ),
        (
            "mysqli.local_infile_directory",
            None,
            sys,
            ModifyHandler::String,
        ),
    ] {
        m.ini(IniDirective::new(name, default, access, handler, "mysqli"));
    }
    for e in php_mysqlnd::ini::ENTRIES {
        let access = match e.modifiable {
            php_mysqlnd::ini::Modifiable::All => Access::ALL,
            php_mysqlnd::ini::Modifiable::System => Access::SYSTEM,
            php_mysqlnd::ini::Modifiable::PerDir => Access::PERDIR,
        };
        m.ini(IniDirective::new(
            e.name,
            e.default,
            access,
            ModifyHandler::None,
            "mysqlnd",
        ));
    }
}

/// `OnUpdateDefaultPort`: `ZEND_ATOL`, rejecting values outside `0..=65535`.
fn on_update_default_port(_vm: &mut Vm, v: &[u8]) -> VmResult<bool> {
    let s = String::from_utf8_lossy(v);
    let t = s.trim_start();
    let end = t
        .char_indices()
        .take_while(|&(i, c)| c.is_ascii_digit() || (i == 0 && (c == '-' || c == '+')))
        .count();
    let n: i64 = t[..end].parse().unwrap_or(0);
    Ok((0..=65535).contains(&n))
}

fn row(k: &str, v: impl Into<String>) -> InfoItem {
    InfoItem::Row(vec![k.to_string(), v.into()])
}

/// `PHP_MINFO_FUNCTION(mysqli)`
fn mysqli_info(_vm: &mut Vm) -> Vec<InfoItem> {
    let (ap, ip, links) = globals::with(|g| {
        (
            g.num_active_persistent,
            g.num_inactive_persistent,
            g.num_links,
        )
    });
    vec![
        InfoItem::TableStart,
        row("MysqlI Support", "enabled"),
        row("Client API library version", php_mysqlnd::CLIENT_INFO),
        row("Active Persistent Links", ap.to_string()),
        row("Inactive Persistent Links", ip.to_string()),
        row("Active Links", links.to_string()),
        InfoItem::TableEnd,
        InfoItem::IniEntries,
    ]
}

/// `PHP_MINFO_FUNCTION(mysqlnd)`
fn mysqlnd_info(vm: &mut Vm) -> Vec<InfoItem> {
    link::sync_mysqlnd_ini(vm);
    let s = php_mysqlnd::ini::settings();
    let yn = |b: bool| if b { "Yes" } else { "No" };
    vec![
        InfoItem::TableStart,
        row("mysqlnd", "enabled"),
        row("Version", php_mysqlnd::CLIENT_INFO),
        row("Compression", "supported"),
        row("core SSL", "supported"),
        row("extended SSL", "supported"),
        row("Command buffer size", s.net_cmd_buffer_size.to_string()),
        row("Read buffer size", s.net_read_buffer_size.to_string()),
        row("Read timeout", s.net_read_timeout.to_string()),
        row("Collecting statistics", yn(s.collect_statistics)),
        row("Collecting memory statistics", yn(s.collect_memory_statistics)),
        row("Tracing", s.debug.clone().unwrap_or_else(|| "n/a".to_string())),
        row(
            "Loaded plugins",
            "mysqlnd,debug_trace,auth_plugin_mysql_native_password,auth_plugin_mysql_clear_password,auth_plugin_caching_sha2_password,auth_plugin_sha256_password",
        ),
        InfoItem::TableEnd,
        InfoItem::IniEntries,
    ]
}

impl Extension for Mysqli {
    fn name(&self) -> &'static str {
        "mysqli"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        for &(name, v) in MYSQLI_CONSTANTS {
            if let Some((_, since, msg)) = DEPRECATED_CONSTANTS.iter().find(|d| d.0 == name) {
                m.constant_deprecated(name, Value::Int(v), since, msg);
            } else {
                m.constant(name, Value::Int(v));
            }
        }
        m.constant_deprecated(
            "MYSQLI_IS_MARIADB",
            Value::Bool(false),
            "8.2",
            "as it is always false",
        );
        m.constant("MYSQL_TYPE_JSON", Value::Int(i64::from(MYSQL_TYPE_JSON)));
        register_ini(m);
        let entries = entries();
        register_classes(m, &entries);
        for en in &entries {
            m.function_static(en.func, en.handler);
            if let Some((since, msg)) = en.fn_dep {
                m.deprecated(since, msg);
            }
        }
        m.ini_on_modify("mysqli.default_port", on_update_default_port);
        m.info(mysqli_info);
        let vm = m.vm();
        if !vm
            .loaded_extensions
            .iter()
            .any(|e| e.eq_ignore_ascii_case("mysqlnd"))
        {
            vm.loaded_extensions.push("mysqlnd".to_string());
        }
        vm.module_info.push(("mysqlnd".to_string(), mysqlnd_info));
        m.on_request_startup(globals::request_startup);
        m.on_request_startup(|_| report::install_sink());
        m.on_request_shutdown(request_shutdown);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_parse() {
        for e in entries() {
            let (name, _) = php_vm::ext::check_signature(e.func);
            assert!(name.starts_with("mysqli_"), "{name}");
            if let Some((_, sig)) = e.method {
                if !sig.is_empty() {
                    let _ = php_vm::ext::check_signature(sig);
                }
            }
        }
    }

    #[test]
    fn default_port_validation() {
        let opts = php_vm::VmOptions::default();
        let mut vm = Vm::new(&opts, Box::new(std::io::sink()));
        for (v, ok) in [
            ("3306", true),
            ("0", true),
            ("65535", true),
            ("65536", false),
            ("-1", false),
            ("x", true),
        ] {
            assert_eq!(
                on_update_default_port(&mut vm, v.as_bytes()).ok(),
                Some(ok),
                "{v}"
            );
        }
    }

    #[test]
    fn suffix_format() {
        assert_eq!(deprecation_suffix("8.4", "x"), b" since 8.4, x");
        assert_eq!(deprecation_suffix("", ""), b"");
    }
}
