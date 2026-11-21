// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Network functions: address conversion, host/service/protocol lookups,
//! DNS, interfaces and syslog.

use crate::ctx::RtExt;
use php_runtime::{Value, ZStr};
use php_stdmisc::dns::{self, SystemResolver};
use php_stdmisc::net;
use php_vm::ext::{Args, ModuleBuilder, VmResult};
use php_vm::Vm;

/// `DNS_ANY`, the default `$type` of `dns_get_record()`.
const DNS_ANY: i64 = 0x1000_0000;
#[allow(clippy::unnecessary_wraps)] // native handler signature
fn inet_ntop(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(net::inet_ntop(a.str(0).as_bytes()))
}

fn inet_pton(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::inet_pton(a.str(0).as_bytes()).vm(vm)
}

fn ip2long(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::ip2long(a.str(0).as_bytes()).vm(vm)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn long2ip(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::from(net::long2ip(a.int(0))))
}

fn gethostname(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    net::gethostname(vm).vm(vm)
}

fn gethostbyname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::gethostbyname(a.str(0).as_bytes(), vm)
        .vm(vm)
        .map(Value::Str)
}

fn gethostbynamel(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::gethostbynamel(a.str(0).as_bytes(), vm).vm(vm)
}

fn gethostbyaddr(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::gethostbyaddr(a.str(0).as_bytes(), vm).vm(vm)
}

fn getservbyname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::getservbyname(a.str(0).as_bytes(), a.str(1).as_bytes()).vm(vm)
}

fn getservbyport(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::getservbyport(a.int(0), a.str(1).as_bytes()).vm(vm)
}

fn getprotobyname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    net::getprotobyname(a.str(0).as_bytes()).vm(vm)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn getprotobynumber(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(net::getprotobynumber(a.int(0)))
}

fn net_get_interfaces(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    net::net_get_interfaces(vm).vm(vm)
}

fn dns_check_record(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let func = a.func_name();
    let host = a.str(0);
    let ty = a.has(1).then(|| a.str(1));
    dns::dns_check_record(
        &mut SystemResolver,
        &func,
        host.as_bytes(),
        ty.as_ref().map(ZStr::as_bytes),
    )
    .vm(vm)
    .map(Value::Bool)
}

fn dns_get_mx(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let func = a.func_name();
    let (ok, hosts, weights) =
        dns::dns_get_mx(&mut SystemResolver, &func, a.str(0).as_bytes()).vm(vm)?;
    a.set_ref(1, Value::Array(hosts));
    if a.has(2) {
        a.set_ref(2, Value::Array(weights));
    }
    Ok(Value::Bool(ok))
}

fn dns_get_record(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let host = a.str(0);
    let ty = if a.has(1) { a.int(1) } else { DNS_ANY };
    let raw = a.has(4) && a.bool(4);
    let r = dns::dns_get_record(
        &mut SystemResolver,
        host.as_bytes(),
        ty,
        a.has(2),
        a.has(3),
        raw,
        vm,
    )
    .vm(vm)?;
    if let Some(ns) = r.authns {
        a.set_ref(2, Value::Array(ns));
    }
    if let Some(ad) = r.addtl {
        a.set_ref(3, Value::Array(ad));
    }
    Ok(r.value)
}

fn openlog(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let prefix = a.str(0);
    net::openlog(prefix.as_bytes(), a.int(1), a.int(2))
        .vm(vm)
        .map(Value::Bool)
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn closelog(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(net::closelog()))
}

#[allow(clippy::unnecessary_wraps)] // native handler signature
fn syslog(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let msg = a.str(1);
    let settings = vm.syslog_settings();
    Ok(Value::Bool(net::syslog(
        a.int(0),
        msg.as_bytes(),
        &settings,
    )))
}

/// Registers the functions and constants.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, v) in dns::CONSTANTS
        .iter()
        .chain(php_log::syslog::SYSLOG_CONSTANTS)
    {
        m.constant(name, Value::Int(*v));
    }
    m.function_static("inet_ntop(string $ip): string|false", inet_ntop)
        .function_static("inet_pton(string $ip): string|false", inet_pton)
        .function_static("ip2long(string $ip): int|false", ip2long)
        .function_static("long2ip(int $ip): string", long2ip)
        .function_static("gethostname(): string|false", gethostname)
        .function_static("gethostbyname(string $hostname): string", gethostbyname)
        .function_static("gethostbynamel(string $hostname): array|false", gethostbynamel)
        .function_static("gethostbyaddr(string $ip): string|false", gethostbyaddr)
        .function_static(
            "getservbyname(string $service, string $protocol): int|false",
            getservbyname,
        )
        .function_static(
            "getservbyport(int $port, string $protocol): string|false",
            getservbyport,
        )
        .function_static("getprotobyname(string $protocol): int|false", getprotobyname)
        .function_static("getprotobynumber(int $protocol): string|false", getprotobynumber)
        .function_static("net_get_interfaces(): array|false", net_get_interfaces)
        .function_static(
            "dns_check_record(string $hostname, string $type = \"MX\"): bool",
            dns_check_record,
        )
        .function_static(
            "checkdnsrr(string $hostname, string $type = \"MX\"): bool",
            dns_check_record,
        )
        .function_static(
            "dns_get_record(string $hostname, int $type = DNS_ANY, &$authoritative_name_servers = null, &$additional_records = null, bool $raw = false): array|false",
            dns_get_record,
        )
        .function_static(
            "dns_get_mx(string $hostname, &$hosts, &$weights = null): bool",
            dns_get_mx,
        )
        .function_static(
            "getmxrr(string $hostname, &$hosts, &$weights = null): bool",
            dns_get_mx,
        )
        .function_static("openlog(string $prefix, int $flags, int $facility): true", openlog)
        .function_static("closelog(): true", closelog)
        .function_static("syslog(int $priority, string $message): true", syslog)
        .on_request_shutdown(|_| php_log::syslog::request_shutdown());
}
