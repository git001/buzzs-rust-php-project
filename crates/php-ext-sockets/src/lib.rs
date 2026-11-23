// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/sockets` for the phprust VM: binds the `php-sockets` library
//! through the `php_vm::ext` extension API (ref: `ext/sockets/sockets.c`).
//!
//! The library owns the module globals ([`Sockets`]) and all libc work;
//! this crate converts PHP values to the library's value model
//! ([`conv`]), keeps the `Socket`/`AddressInfo` objects ([`objects`]),
//! emits the queued warnings with the function prefix, and glues sockets
//! to stream resources (`socket_import_stream()`,
//! `socket_export_stream()`, stream-backed `socket_close()` and blocking
//! changes) through `php-ext-streams`.

// Native handlers share one signature (`fn(&mut Vm, &mut Args) ->
// VmResult<Value>`), so infallible ones still return `VmResult`.
#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]
#![allow(clippy::unnecessary_wraps)]

mod conv;
mod objects;
mod stream;

use std::cell::RefCell;

use php_ext_streams::res;
use php_runtime::{ArrayKey, Value, ZArray, ZStr};
use php_sockets::{Socket, SocketError, SocketResult, Sockets};
use php_streams::stream::{BufferMode, CastAs, StreamOption};
use php_vm::ext::{Args, Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

use crate::objects::{with_addrinfo, with_sock};

thread_local! {
    /// The per-request module globals.
    static STATE: RefCell<Sockets> = RefCell::new(Sockets::new());
}

/// Runs `f` on the module state.
fn with<R>(f: impl FnOnce(&mut Sockets) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// The `sockets` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct SocketsExt;

/// The running function's name (`socket_getopt` for the alias).
fn fname(a: &Args) -> String {
    a.func
        .as_ref()
        .map_or_else(String::new, |f| f.name.to_string_lossy())
}

/// A PHP string from bytes.
fn bytes(b: &[u8]) -> Value {
    Value::Str(ZStr::from(b))
}

/// The exception for a library error (messages are complete).
fn throw(vm: &mut Vm, e: SocketError) -> Throw {
    match e {
        SocketError::ValueError(m) => vm.throw("ValueError", &m),
        SocketError::TypeError(m) => vm.throw("TypeError", &m),
        SocketError::ArgumentCountError(m) => vm.throw("ArgumentCountError", &m),
        SocketError::Error(m) => vm.throw("Error", &m),
        SocketError::Failed => vm.throw("Error", "socket operation failed"),
    }
}

/// Emits the warnings the last library call queued.
fn warn(vm: &mut Vm) -> VmResult<()> {
    for w in with(Sockets::take_warnings) {
        match w.strip_prefix(php_sockets::PLAIN_WARNING) {
            // `zend_error(E_WARNING, ...)`: no function prefix.
            Some(plain) => vm.raise(E_WARNING, plain)?,
            None => vm.php_warning(&w)?,
        }
    }
    Ok(())
}

/// `E_WARNING`.
const E_WARNING: i64 = 2;

/// Emits the warnings, then maps the result: `ok` on success, `false` for
/// [`SocketError::Failed`], an exception otherwise.
fn done<T>(
    vm: &mut Vm,
    r: SocketResult<T>,
    ok: impl FnOnce(&mut Vm, T) -> VmResult<Value>,
) -> VmResult<Value> {
    warn(vm)?;
    match r {
        Ok(v) => ok(vm, v),
        Err(SocketError::Failed) => Ok(Value::Bool(false)),
        Err(e) => Err(throw(vm, e)),
    }
}

/// `true` on success.
fn ok_true<T>(_vm: &mut Vm, _v: T) -> VmResult<Value> {
    Ok(Value::Bool(true))
}

/// An int result.
#[allow(clippy::needless_pass_by_value)]
fn ok_int(_vm: &mut Vm, n: i64) -> VmResult<Value> {
    Ok(Value::Int(n))
}

/// Runs `f` on the module state and the socket of argument `n`.
fn on_sock<T>(
    a: &Args,
    n: usize,
    f: impl FnOnce(&mut Sockets, &mut Socket) -> SocketResult<T>,
) -> SocketResult<T> {
    with_sock(a, n, |s| with(|st| f(st, &mut s.sock))).unwrap_or(Err(SocketError::Failed))
}

/// The integer argument `n` or `default` when omitted (defaults spelled
/// as constants in the stubs).
fn int_or(a: &Args, n: usize, default: i64) -> i64 {
    if a.has(n) {
        a.int(n)
    } else {
        default
    }
}

/// `?int` argument `n`.
fn opt_int(a: &Args, n: usize) -> Option<i64> {
    a.opt(n).map(|_| a.int(n))
}

/// A new `Socket` object from a result.
fn ok_socket(vm: &mut Vm, s: Socket) -> VmResult<Value> {
    objects::new_socket(vm, s)
}

// ---------------------------------------------------------------------------
// Creation and lifecycle
// ---------------------------------------------------------------------------

fn socket_create(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|s| s.create(a.int(0), a.int(1), a.int(2)));
    done(vm, r, ok_socket)
}

fn socket_create_listen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let backlog = int_or(a, 1, php_sockets::SOMAXCONN);
    let r = with(|s| s.create_listen(a.int(0), backlog));
    done(vm, r, ok_socket)
}

fn socket_create_pair(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|s| s.create_pair(a.int(0), a.int(1), a.int(2)));
    warn(vm)?;
    match r {
        Ok((s0, s1)) => {
            let v0 = objects::new_socket(vm, s0)?;
            let v1 = objects::new_socket(vm, s1)?;
            a.set_ref(3, Value::Array(ZArray::from_values([v0, v1])));
            Ok(Value::Bool(true))
        }
        Err(SocketError::Failed) => Ok(Value::Bool(false)),
        Err(e) => Err(throw(vm, e)),
    }
}

fn socket_accept(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with_sock(a, 0, |s| with(|st| st.accept(&s.sock))).unwrap_or(Err(SocketError::Failed));
    done(vm, r, ok_socket)
}

fn socket_listen(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let backlog = a.int(1);
    let r = on_sock(a, 0, |st, s| st.listen(s, backlog));
    done(vm, r, ok_true)
}

fn socket_close(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // A stream-backed socket closes its stream, which owns the descriptor.
    let stream = with_sock(a, 0, |s| {
        if s.sock.is_closed() {
            None
        } else {
            s.stream.clone()
        }
    })
    .flatten();
    if let Some(Value::Resource(r)) = &stream {
        if let Some(cell) = res::cell_of(r) {
            if let Some(mut st) = cell.take() {
                r.close();
                php_ext_streams::bridge::run(vm, || st.close())?;
            }
        }
    }
    let r = on_sock(a, 0, Sockets::close);
    done(vm, r, |_, ()| Ok(Value::Null))
}

fn socket_shutdown(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mode = a.int(1);
    let r = on_sock(a, 0, |st, s| st.shutdown(s, mode));
    done(vm, r, ok_true)
}

fn socket_atmark(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = on_sock(a, 0, Sockets::atmark);
    done(vm, r, |_, b| Ok(Value::Bool(b)))
}

/// `socket_set_block()`/`socket_set_nonblock()`: a stream-backed socket
/// changes the mode through its stream first.
fn set_blocking(vm: &mut Vm, a: &Args, blocking: bool) -> VmResult<Value> {
    let state = with_sock(a, 0, |s| (s.sock.is_closed(), s.stream.clone()));
    if let Some((false, Some(Value::Resource(r)))) = state {
        if let Some(cell) = res::cell_of(&r).filter(|c| !r.is_closed() && !c.is_closed()) {
            let rc = res::with_stream(vm, &cell, |s| {
                s.set_option(&mut StreamOption::Blocking(blocking))
            })?;
            if rc.is_some_and(|rc| rc != -1) {
                with_sock(a, 0, |s| s.sock.set_blocking_flag(blocking));
                return Ok(Value::Bool(true));
            }
        }
    }
    let r = on_sock(a, 0, |st, s| {
        if blocking {
            st.set_block(s)
        } else {
            st.set_nonblock(s)
        }
    });
    done(vm, r, ok_true)
}

fn socket_set_block(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    set_blocking(vm, a, true)
}

fn socket_set_nonblock(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    set_blocking(vm, a, false)
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

fn socket_bind(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (addr, port) = (a.str(1), a.int(2));
    let r = on_sock(a, 0, |st, s| st.bind(s, addr.as_bytes(), port));
    done(vm, r, ok_true)
}

fn socket_connect(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (addr, port) = (a.str(1), opt_int(a, 2));
    let r = on_sock(a, 0, |st, s| st.connect(s, addr.as_bytes(), port));
    done(vm, r, ok_true)
}

fn name_result(vm: &mut Vm, a: &Args, r: SocketResult<php_sockets::SocketName>) -> VmResult<Value> {
    done(vm, r, |_, n| {
        a.set_ref(1, bytes(&n.address));
        if let Some(p) = n.port {
            if a.has(2) {
                a.set_ref(2, Value::Int(p));
            }
        }
        Ok(Value::Bool(true))
    })
}

fn socket_getsockname(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = on_sock(a, 0, Sockets::getsockname);
    name_result(vm, a, r)
}

fn socket_getpeername(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = on_sock(a, 0, Sockets::getpeername);
    name_result(vm, a, r)
}

// ---------------------------------------------------------------------------
// Data transfer
// ---------------------------------------------------------------------------

fn socket_write(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (data, len) = (a.str(1), opt_int(a, 2));
    let r = on_sock(a, 0, |st, s| st.write(s, data.as_bytes(), len));
    done(vm, r, ok_int)
}

fn socket_read(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (len, mode) = (a.int(1), int_or(a, 2, php_sockets::PHP_BINARY_READ));
    let r = on_sock(a, 0, |st, s| st.read(s, len, mode));
    done(vm, r, |_, d| Ok(bytes(&d)))
}

fn socket_recv(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (len, flags) = (a.int(2), a.int(3));
    let r = on_sock(a, 0, |st, s| st.recv(s, len, flags));
    done(vm, r, |_, rv| {
        a.set_ref(1, rv.data.as_deref().map_or(Value::Null, bytes));
        Ok(rv.result.map_or(Value::Bool(false), Value::Int))
    })
}

fn socket_send(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (data, len, flags) = (a.str(1), a.int(2), a.int(3));
    let r = on_sock(a, 0, |st, s| st.send(s, data.as_bytes(), len, flags));
    done(vm, r, ok_int)
}

fn socket_recvfrom(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (len, flags, port_given) = (a.int(2), a.int(3), a.has(5));
    let r = on_sock(a, 0, |st, s| st.recvfrom(s, len, flags, port_given));
    done(vm, r, |_, rv| {
        a.set_ref(1, bytes(&rv.data));
        a.set_ref(4, bytes(&rv.address));
        if let Some(p) = rv.port {
            a.set_ref(5, Value::Int(p));
        }
        Ok(Value::Int(rv.result))
    })
}

fn socket_sendto(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (data, len, flags, addr, port) = (a.str(1), a.int(2), a.int(3), a.str(4), opt_int(a, 5));
    let r = on_sock(a, 0, |st, s| {
        st.sendto(s, data.as_bytes(), len, flags, addr.as_bytes(), port)
    });
    done(vm, r, ok_int)
}

fn socket_sendmsg(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let message = conv::to_svalue(vm, &a.at(1))?;
    let flags = a.int(2);
    let r = on_sock(a, 0, |st, s| st.sendmsg(s, &message, flags));
    done(vm, r, ok_int)
}

fn socket_recvmsg(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let message = conv::to_svalue(vm, &a.at(1))?;
    let flags = a.int(2);
    let r = on_sock(a, 0, |st, s| st.recvmsg(s, &message, flags));
    done(vm, r, |vm, (n, out)| {
        let v = conv::from_out(vm, out)?;
        a.set_ref(1, v);
        Ok(Value::Int(n))
    })
}

fn socket_cmsg_space(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|s| s.cmsg_space(a.int(0), a.int(1), a.int(2)));
    done(vm, r, ok_int)
}

// ---------------------------------------------------------------------------
// select
// ---------------------------------------------------------------------------

fn socket_select(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut originals: [Option<ZArray>; 3] = [None, None, None];
    let mut converted: [Option<Vec<php_sockets::Value>>; 3] = [None, None, None];
    for i in 0..3 {
        if let Value::Array(arr) = a.at(i) {
            let mut vals = Vec::with_capacity(arr.len());
            for (_, v) in arr.iter() {
                vals.push(conv::to_svalue(vm, &v.deref())?);
            }
            converted[i] = Some(vals);
            originals[i] = Some(arr);
        }
    }
    let views = [
        converted[0].as_deref(),
        converted[1].as_deref(),
        converted[2].as_deref(),
    ];
    let seconds = opt_int(a, 3);
    let micro = a.int(4);
    let r = with(|s| s.select(views, seconds, micro));
    done(vm, r, |_, (n, ready)| {
        for (i, positions) in ready.into_iter().enumerate() {
            let (Some(positions), Some(orig)) = (positions, &originals[i]) else {
                continue;
            };
            let entries: Vec<_> = orig.iter().collect();
            let mut out = ZArray::with_capacity(positions.len());
            for p in positions {
                if let Some((k, v)) = entries.get(p) {
                    out.make_mut().insert(ArrayKey::clone(k), Value::clone(v));
                }
            }
            a.set_ref(i, Value::Array(out));
        }
        Ok(Value::Int(n))
    })
}

// ---------------------------------------------------------------------------
// Options and errors
// ---------------------------------------------------------------------------

fn socket_get_option(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let func = fname(a);
    let (level, option) = (a.int(1), a.int(2));
    let r = on_sock(a, 0, |st, s| st.get_option(&func, s, level, option));
    done(vm, r, conv::from_out)
}

fn socket_set_option(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let func = fname(a);
    let value = conv::to_svalue(vm, &a.at(3))?;
    let (level, option) = (a.int(1), a.int(2));
    let r = on_sock(a, 0, |st, s| st.set_option(&func, s, level, option, &value));
    done(vm, r, ok_true)
}

fn socket_last_error(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = if a.opt(0).is_some() {
        with_sock(a, 0, |s| with(|st| st.last_error(Some(&s.sock)))).unwrap_or(Ok(0))
    } else {
        with(|st| st.last_error(None))
    };
    done(vm, r, ok_int)
}

fn socket_clear_error(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = if a.opt(0).is_some() {
        with_sock(a, 0, |s| with(|st| st.clear_error(Some(&mut s.sock)))).unwrap_or(Ok(()))
    } else {
        with(|st| st.clear_error(None))
    };
    done(vm, r, |_, ()| Ok(Value::Null))
}

fn socket_strerror(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with(|s| s.strerror(a.int(0)));
    done(vm, r, |_, m| Ok(bytes(m.as_bytes())))
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

fn socket_import_stream(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // Z_PARAM_RESOURCE + php_stream_from_zval().
    let r = res::resource_arg(vm, a, 0)?;
    let cell = res::stream_of_resource(vm, a, &r)?;
    let cast = res::with_stream(vm, &cell, |s| {
        s.cast(CastAs::SocketFd, false).ok_or_else(|| s.ops_label())
    })?;
    let fd = match cast {
        Some(Ok(fd)) => fd,
        Some(Err(label)) => {
            vm.php_warning(&format!(
                "Cannot represent a stream of type {label} as a Socket Descriptor"
            ))?;
            return Ok(Value::Bool(false));
        }
        None => return Ok(Value::Bool(false)),
    };
    let r = with(|s| s.import_fd(fd));
    warn(vm)?;
    let sock = match r {
        Ok(s) => s,
        Err(SocketError::Failed) => return Ok(Value::Bool(false)),
        Err(e) => return Err(throw(vm, e)),
    };
    let v = objects::new_socket_with_stream(vm, sock, a.at(0))?;
    res::with_stream(vm, &cell, |s| {
        s.set_option(&mut StreamOption::ReadBuffer(BufferMode::None, None))
    })?;
    Ok(v)
}

fn socket_export_stream(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let timeout = vm.ini_long("default_socket_timeout");
    let r = with_sock(a, 0, |s| {
        if s.sock.is_closed() {
            return Err(());
        }
        if let Some(v) = &s.stream {
            return Ok(Ok(v.clone()));
        }
        let stream = stream::socket_stream(
            s.sock.fd(),
            s.sock.export_protocol(),
            s.sock.is_blocking(),
            timeout,
        );
        Ok(Err(stream))
    });
    match r {
        Some(Err(())) => Err(vm.throw(
            "Error",
            "socket_export_stream(): Argument #1 ($socket) has already been closed",
        )),
        Some(Ok(Ok(v))) => Ok(v),
        Some(Ok(Err(stream))) => {
            let v = res::new_stream(stream, None);
            with_sock(a, 0, |s| {
                s.sock.set_stream_owned(true);
                s.stream = Some(v.clone());
            });
            Ok(v)
        }
        None => Ok(Value::Bool(false)),
    }
}

// ---------------------------------------------------------------------------
// addrinfo
// ---------------------------------------------------------------------------

fn socket_addrinfo_lookup(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let host = a.str(0);
    let service = a.opt(1).map(|_| a.str(1));
    let hints = if a.has(2) {
        Some(conv::to_svalue(vm, &a.at(2))?)
    } else {
        None
    };
    let r = with(|s| {
        s.addrinfo_lookup(
            host.as_bytes(),
            service.as_ref().map(ZStr::as_bytes),
            hints.as_ref(),
        )
    });
    done(vm, r, |vm, res| match res {
        Ok(list) => {
            let mut out = Vec::with_capacity(list.len());
            for info in list {
                out.push(objects::new_addrinfo(vm, info)?);
            }
            Ok(Value::Array(ZArray::from_values(out)))
        }
        Err(code) => {
            if a.has(3) {
                a.set_ref(3, Value::Int(code));
            }
            Ok(Value::Bool(false))
        }
    })
}

fn socket_addrinfo_bind(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r =
        with_addrinfo(a, 0, |ai| with(|s| s.addrinfo_bind(ai))).unwrap_or(Err(SocketError::Failed));
    done(vm, r, ok_socket)
}

fn socket_addrinfo_connect(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = with_addrinfo(a, 0, |ai| with(|s| s.addrinfo_connect(ai)))
        .unwrap_or(Err(SocketError::Failed));
    done(vm, r, ok_socket)
}

fn socket_addrinfo_explain(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match with_addrinfo(a, 0, |ai| ai.explain()) {
        Some(out) => conv::from_out(vm, out),
        None => Ok(Value::Array(ZArray::new())),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

fn request_startup(_vm: &mut Vm) {
    with(|s| *s = Sockets::new());
}

impl Extension for SocketsExt {
    fn name(&self) -> &'static str {
        "sockets"
    }

    #[allow(clippy::too_many_lines)]
    fn register(&self, m: &mut ModuleBuilder<'_>) {
        m.info(module_info);
        objects::register(m);
        for (name, value) in php_sockets::CONSTANTS {
            m.constant(name, Value::Int(*value));
        }
        m.on_request_startup(request_startup);
        m.function_static(
            "socket_select(?array &$read, ?array &$write, ?array &$except, ?int $seconds, int $microseconds = 0): int|false",
            socket_select,
        )
        .function_static(
            "socket_create_listen(int $port, int $backlog = SOMAXCONN): Socket|false",
            socket_create_listen,
        )
        .function_static("socket_accept(Socket $socket): Socket|false", socket_accept)
        .function_static("socket_set_nonblock(Socket $socket): bool", socket_set_nonblock)
        .function_static("socket_set_block(Socket $socket): bool", socket_set_block)
        .function_static("socket_listen(Socket $socket, int $backlog = 0): bool", socket_listen)
        .function_static("socket_close(Socket $socket): void", socket_close)
        .function_static(
            "socket_write(Socket $socket, string $data, ?int $length = null): int|false",
            socket_write,
        )
        .function_static(
            "socket_read(Socket $socket, int $length, int $mode = PHP_BINARY_READ): string|false",
            socket_read,
        )
        .function_static(
            "socket_getsockname(Socket $socket, &$address, &$port = null): bool",
            socket_getsockname,
        )
        .function_static(
            "socket_getpeername(Socket $socket, &$address, &$port = null): bool",
            socket_getpeername,
        )
        .function_static(
            "socket_create(int $domain, int $type, int $protocol): Socket|false",
            socket_create,
        )
        .function_static(
            "socket_connect(Socket $socket, string $address, ?int $port = null): bool",
            socket_connect,
        )
        .function_static("socket_strerror(int $error_code): string", socket_strerror)
        .function_static(
            "socket_bind(Socket $socket, string $address, int $port = 0): bool",
            socket_bind,
        )
        .function_static(
            "socket_recv(Socket $socket, &$data, int $length, int $flags): int|false",
            socket_recv,
        )
        .function_static(
            "socket_send(Socket $socket, string $data, int $length, int $flags): int|false",
            socket_send,
        )
        .function_static(
            "socket_recvfrom(Socket $socket, &$data, int $length, int $flags, &$address, &$port = null): int|false",
            socket_recvfrom,
        )
        .function_static(
            "socket_sendto(Socket $socket, string $data, int $length, int $flags, string $address, ?int $port = null): int|false",
            socket_sendto,
        )
        .function_static(
            "socket_get_option(Socket $socket, int $level, int $option): array|int|false",
            socket_get_option,
        )
        .function_static(
            "socket_getopt(Socket $socket, int $level, int $option): array|int|false",
            socket_get_option,
        )
        .function_static(
            "socket_set_option(Socket $socket, int $level, int $option, $value): bool",
            socket_set_option,
        )
        .function_static(
            "socket_setopt(Socket $socket, int $level, int $option, $value): bool",
            socket_set_option,
        )
        .function_static(
            "socket_create_pair(int $domain, int $type, int $protocol, &$pair): bool",
            socket_create_pair,
        )
        .function_static("socket_shutdown(Socket $socket, int $mode = 2): bool", socket_shutdown)
        .function_static("socket_atmark(Socket $socket): bool", socket_atmark)
        .function_static("socket_last_error(?Socket $socket = null): int", socket_last_error)
        .function_static("socket_clear_error(?Socket $socket = null): void", socket_clear_error)
        .function_static("socket_import_stream($stream): Socket|false", socket_import_stream)
        .function_static("socket_export_stream(Socket $socket)", socket_export_stream)
        .function_static(
            "socket_sendmsg(Socket $socket, array $message, int $flags = 0): int|false",
            socket_sendmsg,
        )
        .function_static(
            "socket_recvmsg(Socket $socket, array &$message, int $flags = 0): int|false",
            socket_recvmsg,
        )
        .function_static(
            "socket_cmsg_space(int $level, int $type, int $num = 0): int",
            socket_cmsg_space,
        )
        .function_static(
            "socket_addrinfo_lookup(string $host, ?string $service = null, array $hints = [], &$error_code = null): array|false",
            socket_addrinfo_lookup,
        )
        .function_static(
            "socket_addrinfo_connect(AddressInfo $address): Socket|false",
            socket_addrinfo_connect,
        )
        .function_static(
            "socket_addrinfo_bind(AddressInfo $address): Socket|false",
            socket_addrinfo_bind,
        )
        .function_static(
            "socket_addrinfo_explain(AddressInfo $address): array",
            socket_addrinfo_explain,
        );
    }
}

/// `PHP_MINFO_FUNCTION`.
fn module_info(_vm: &mut Vm) -> Vec<php_vm::ext::InfoItem> {
    vec![
        php_vm::ext::InfoItem::TableStart,
        php_vm::ext::InfoItem::Row(vec!["Sockets Support".into(), "enabled".into()]),
        php_vm::ext::InfoItem::TableEnd,
    ]
}
