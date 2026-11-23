// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! PHP's `ext/sockets` for Linux (ref: `ext/sockets/sockets.c`,
//! `sockaddr_conv.c`, `multicast.c`, `sendrecvmsg.c`, `conversions.c`,
//! `sockets.stub.php`).
//!
//! * [`Socket`] is the state behind a PHP `Socket` object (descriptor,
//!   family, per-socket error, blocking flag).  Dropping it closes the
//!   descriptor unless a stream owns it (see [`Socket::set_stream_owned`]).
//! * [`AddressInfo`] backs the `AddressInfo` object.
//! * [`Sockets`] holds the module globals (`last_error`) and collects the
//!   warnings a call emits; every PHP function is a method on it.
//!
//! # Diagnostics
//!
//! Warnings are pushed to [`Sockets::take_warnings`] **without** the
//! `"func(): "` prefix — the VM emits each as `E_WARNING` with the prefix,
//! as `php_error_docref()` does — and must be emitted before acting on the
//! call's result.  Thrown errors ([`SocketError`]) carry the **complete**
//! exception message (some Zend errors are not prefixed, so the crate
//! decides).  `Err(SocketError::Failed)` means "return `false`".
//!
//! # Values
//!
//! Options and messages use the [`value`] model: the VM converts PHP values
//! to [`Value`] and results back from [`OutValue`].
//!
//! # Streams
//!
//! `socket_import_stream()`/`socket_export_stream()` are split: the VM
//! casts streams to descriptors and builds stream resources; this crate
//! provides [`Sockets::import_fd`], [`Socket::export_protocol`] and the
//! ownership flag.  `socket_wsaprotocol_info_*()` do not exist on Linux.

#![expect(
    clippy::disallowed_methods,
    reason = "memory_limit ratchet: calls not reviewed yet (roadmap 76)"
)]

mod constants;
mod conversions;
mod options;
mod sys;
pub mod value;

pub use constants::CONSTANTS;
pub use value::{Key, OutValue, Value};

use std::fmt;

/// `PHP_NORMAL_READ`.
pub const PHP_NORMAL_READ: i64 = 1;
/// `PHP_BINARY_READ`.
pub const PHP_BINARY_READ: i64 = 2;
/// `SOMAXCONN` (default backlog of `socket_create_listen()`).
pub const SOMAXCONN: i64 = libc::SOMAXCONN as i64;
/// `FD_SETSIZE`.
pub const FD_SETSIZE: i32 = sys::FD_SETSIZE;

/// A thrown error (complete message) or a silent `false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketError {
    /// Throw `ValueError`.
    ValueError(String),
    /// Throw `TypeError`.
    TypeError(String),
    /// Throw `ArgumentCountError`.
    ArgumentCountError(String),
    /// Throw `Error`.
    Error(String),
    /// Return `false` (any warnings are in [`Sockets::take_warnings`]).
    Failed,
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ValueError(m)
            | Self::TypeError(m)
            | Self::ArgumentCountError(m)
            | Self::Error(m) => f.write_str(m),
            Self::Failed => f.write_str("failed"),
        }
    }
}

impl std::error::Error for SocketError {}

/// Result alias.
pub type SocketResult<T> = Result<T, SocketError>;

pub(crate) fn arg_value_error(func: &str, n: u32, name: &str, msg: &str) -> SocketError {
    SocketError::ValueError(format!("{func}(): Argument #{n} (${name}) {msg}"))
}

pub(crate) fn arg_type_error(func: &str, n: u32, name: &str, msg: &str) -> SocketError {
    SocketError::TypeError(format!("{func}(): Argument #{n} (${name}) {msg}"))
}

/// `sockets_strerror()`: `hstrerror()` for host-lookup codes (below
/// `-10000`), `strerror()` otherwise.
pub fn sockets_strerror(error: i32) -> String {
    if error < -10000 {
        let e = if error == i32::MIN {
            2_147_473_648
        } else {
            -error - 10000
        };
        sys::hstrerror(e)
    } else {
        sys::strerror(error)
    }
}

/// The state behind a PHP `Socket` object.
#[derive(Debug)]
pub struct Socket {
    fd: i32,
    family: i32,
    error: i32,
    blocking: bool,
    stream_owned: bool,
}

impl Drop for Socket {
    fn drop(&mut self) {
        if self.fd >= 0 && !self.stream_owned {
            sys::close(self.fd);
        }
    }
}

impl Socket {
    fn new(fd: i32, family: i32) -> Self {
        Self {
            fd,
            family,
            error: 0,
            blocking: true,
            stream_owned: false,
        }
    }

    /// The descriptor (`-1` once closed).
    pub fn fd(&self) -> i32 {
        self.fd
    }

    /// The address family it was created with (`php_socket->type`).
    pub fn family(&self) -> i32 {
        self.family
    }

    /// The per-socket error (`socket_last_error($socket)`).
    pub fn error(&self) -> i32 {
        self.error
    }

    /// Whether the socket is in blocking mode as far as PHP knows.
    pub fn is_blocking(&self) -> bool {
        self.blocking
    }

    /// Whether `socket_close()` has been called.
    pub fn is_closed(&self) -> bool {
        self.fd < 0
    }

    /// Marks the descriptor as owned by an associated stream (after
    /// `socket_import_stream()` or `socket_export_stream()`): dropping the
    /// socket then leaves it open.
    pub fn set_stream_owned(&mut self, owned: bool) {
        self.stream_owned = owned;
    }

    /// Whether a stream owns the descriptor.
    pub fn is_stream_owned(&self) -> bool {
        self.stream_owned
    }

    /// Records the blocking mode after the VM changed it through the
    /// associated stream (`socket_set_(non)block()` on stream-backed
    /// sockets).
    pub fn set_blocking_flag(&mut self, blocking: bool) {
        self.blocking = blocking;
    }

    /// The transport `socket_export_stream()` asks for: `tcp://`, `udp://`,
    /// `unix://`, `udg://`, or `None` for a generic socket stream.
    pub fn export_protocol(&self) -> Option<&'static str> {
        let int_opt = |name| {
            let mut b = [0u8; 4];
            sys::getsockopt(self.fd, libc::SOL_SOCKET, name, &mut b).ok()?;
            Some(i32::from_ne_bytes(b))
        };
        match self.family {
            libc::AF_INET | libc::AF_INET6 => match int_opt(libc::SO_TYPE)? {
                libc::SOCK_STREAM => {
                    (int_opt(libc::SO_PROTOCOL)? == libc::IPPROTO_TCP).then_some("tcp://")
                }
                libc::SOCK_DGRAM => Some("udp://"),
                _ => None,
            },
            libc::AF_UNIX => match int_opt(libc::SO_TYPE)? {
                libc::SOCK_STREAM => Some("unix://"),
                libc::SOCK_DGRAM => Some("udg://"),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The state behind a PHP `AddressInfo` object.
#[derive(Debug, Clone)]
pub struct AddressInfo {
    entry: sys::AddrInfoEntry,
}

/// What `socket_recv()` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recv {
    /// The value for `$data` (`None` → `null`).
    pub data: Option<Vec<u8>>,
    /// The return value (`None` → `false`).
    pub result: Option<i64>,
}

/// What `socket_recvfrom()` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecvFrom {
    /// `$data`.
    pub data: Vec<u8>,
    /// `$address`.
    pub address: Vec<u8>,
    /// `$port` (assigned only when `Some`).
    pub port: Option<i64>,
    /// The return value.
    pub result: i64,
}

/// An address from `socket_getsockname()`/`socket_getpeername()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketName {
    /// `$address`.
    pub address: Vec<u8>,
    /// `$port` (assigned only when `Some` and the argument was passed).
    pub port: Option<i64>,
}

/// Marks a queued warning from [`Sockets::take_warnings`] raised with
/// `zend_error()` rather than `php_error_docref()`: emit the rest of the
/// message without the `"func(): "` prefix.
pub const PLAIN_WARNING: char = '\u{1}';

/// The module globals plus the warning sink.
#[derive(Debug, Default)]
pub struct Sockets {
    last_error: i32,
    warnings: Vec<String>,
}

fn ensure_open(func: &str, sock: &Socket) -> SocketResult<()> {
    if sock.fd < 0 {
        Err(SocketError::Error(format!(
            "{func}(): Argument #1 ($socket) has already been closed"
        )))
    } else {
        Ok(())
    }
}

const TYPE_ERROR_TEXT: &str = "must be one of SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET, SOCK_RAW, or SOCK_RDM optionally OR'ed with SOCK_CLOEXEC, SOCK_NONBLOCK";

#[allow(clippy::cast_possible_truncation)]
fn c_int(v: i64) -> i32 {
    v as i32
}

impl Sockets {
    /// Fresh module state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Warnings emitted since the last call (without function prefix).
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    pub(crate) fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    /// Queues a `zend_error(E_WARNING, ...)` (no function prefix): the
    /// message starts with [`PLAIN_WARNING`], which the VM strips.
    pub(crate) fn warn_plain(&mut self, msg: &str) {
        self.warnings.push(format!("{PLAIN_WARNING}{msg}"));
    }

    /// `zval_get_long()`: like [`Value::to_long`], plus the engine's
    /// "Object of class X could not be converted to int" warning for
    /// objects (`convert_object_to_type` without a cast handler).
    pub(crate) fn long_of(&mut self, v: &Value) -> i64 {
        match v {
            Value::Object { class, .. } => {
                self.warn_plain(&format!(
                    "Object of class {class} could not be converted to int"
                ));
            }
            Value::Socket { .. } => {
                self.warn_plain("Object of class Socket could not be converted to int");
            }
            _ => {}
        }
        v.to_long()
    }

    /// `PHP_SOCKET_ERROR(sock, msg, errno)`.
    pub(crate) fn sock_error(&mut self, sock: &mut Socket, msg: &str, err: i32) {
        sock.error = err;
        self.last_error = err;
        if err != libc::EAGAIN && err != libc::EINPROGRESS {
            self.warn(format!("{msg} [{err}]: {}", sockets_strerror(err)));
        }
    }

    // ------------------------------------------------------ errors

    /// `socket_last_error(?Socket $socket = null)`.
    pub fn last_error(&self, socket: Option<&Socket>) -> SocketResult<i64> {
        match socket {
            Some(s) => {
                ensure_open("socket_last_error", s)?;
                Ok(s.error.into())
            }
            None => Ok(self.last_error.into()),
        }
    }

    /// `socket_clear_error(?Socket $socket = null)`.
    pub fn clear_error(&mut self, socket: Option<&mut Socket>) -> SocketResult<()> {
        match socket {
            Some(s) => {
                ensure_open("socket_clear_error", s)?;
                s.error = 0;
            }
            None => self.last_error = 0,
        }
        Ok(())
    }

    /// `socket_strerror(int $error_code)`.
    pub fn strerror(&self, error_code: i64) -> SocketResult<String> {
        let code = i32::try_from(error_code).map_err(|_| {
            arg_value_error(
                "socket_strerror",
                1,
                "error_code",
                &format!("must be between {} and {}", i32::MIN, i32::MAX),
            )
        })?;
        Ok(sockets_strerror(code))
    }

    // ------------------------------------------------------ creation

    fn check_type(func: &str, ty: i64) -> SocketResult<()> {
        let check = ty & !i64::from(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK);
        if check > 10 {
            return Err(arg_value_error(func, 2, "type", TYPE_ERROR_TEXT));
        }
        Ok(())
    }

    /// `socket_create(int $domain, int $type, int $protocol)`.
    pub fn create(&mut self, domain: i64, ty: i64, protocol: i64) -> SocketResult<Socket> {
        if ![
            libc::AF_UNIX,
            libc::AF_INET6,
            libc::AF_PACKET,
            libc::AF_INET,
        ]
        .iter()
        .any(|&d| i64::from(d) == domain)
        {
            return Err(arg_value_error(
                "socket_create",
                1,
                "domain",
                "must be one of AF_UNIX, AF_PACKET, AF_INET6, or AF_INET",
            ));
        }
        Self::check_type("socket_create", ty)?;
        let protocol = eth_proto(protocol, domain);
        match sys::socket(c_int(domain), c_int(ty), c_int(protocol)) {
            Ok(fd) => Ok(Socket::new(fd, c_int(domain))),
            Err(e) => {
                self.last_error = e;
                self.warn(format!(
                    "Unable to create socket [{e}]: {}",
                    sockets_strerror(e)
                ));
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_create_listen(int $port, int $backlog = SOMAXCONN)`.
    pub fn create_listen(&mut self, port: i64, backlog: i64) -> SocketResult<Socket> {
        let port = u16::try_from(port).map_err(|_| {
            arg_value_error(
                "socket_create_listen",
                1,
                "port",
                "must be between 0 and 65535",
            )
        })?;
        let mut sock = Socket::new(-1, libc::AF_INET);
        let fd = match sys::socket(libc::AF_INET, libc::SOCK_STREAM, 0) {
            Ok(fd) => fd,
            Err(e) => {
                self.sock_error(&mut sock, "unable to create listening socket", e);
                return Err(SocketError::Failed);
            }
        };
        sock.fd = fd;
        if let Err(e) = sys::bind(fd, &sys::SockAddr::inet([0; 4], port)) {
            self.sock_error(&mut sock, "unable to bind to given address", e);
            return Err(SocketError::Failed);
        }
        if let Err(e) = sys::listen(fd, c_int(backlog)) {
            self.sock_error(&mut sock, "unable to listen on socket", e);
            return Err(SocketError::Failed);
        }
        Ok(sock)
    }

    /// `socket_create_pair(int $domain, int $type, int $protocol, &$pair)`.
    pub fn create_pair(
        &mut self,
        domain: i64,
        ty: i64,
        protocol: i64,
    ) -> SocketResult<(Socket, Socket)> {
        if ![libc::AF_INET, libc::AF_INET6, libc::AF_UNIX]
            .iter()
            .any(|&d| i64::from(d) == domain)
        {
            return Err(arg_value_error(
                "socket_create_pair",
                1,
                "domain",
                "must be one of AF_UNIX, AF_INET6, or AF_INET",
            ));
        }
        Self::check_type("socket_create_pair", ty)?;
        match sys::socketpair(c_int(domain), c_int(ty), c_int(protocol)) {
            Ok((a, b)) => Ok((Socket::new(a, c_int(domain)), Socket::new(b, c_int(domain)))),
            Err(e) => {
                self.last_error = e;
                self.warn(format!(
                    "Unable to create socket pair [{e}]: {}",
                    sockets_strerror(e)
                ));
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_accept(Socket $socket)`.
    pub fn accept(&mut self, socket: &Socket) -> SocketResult<Socket> {
        ensure_open("socket_accept", socket)?;
        let mut flags = libc::SOCK_CLOEXEC;
        if !socket.blocking {
            flags |= libc::SOCK_NONBLOCK;
        }
        match sys::accept4(socket.fd, flags) {
            Ok((fd, addr)) => Ok(Socket::new(fd, addr.family())),
            Err(e) => {
                let mut out = Socket::new(-1, 0);
                self.sock_error(&mut out, "unable to accept incoming connection", e);
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_import_stream()` after the VM cast the stream to `fd`
    /// (`socket_import_file_descriptor()`).  The returned socket is marked
    /// stream-owned and keeps the stream's descriptor.
    pub fn import_fd(&mut self, fd: i32) -> SocketResult<Socket> {
        let mut sock = Socket::new(fd, 0);
        sock.stream_owned = true;
        let mut b = [0u8; 4];
        if sys::getsockopt(fd, libc::SOL_SOCKET, libc::SO_DOMAIN, &mut b).is_ok() {
            sock.family = i32::from_ne_bytes(b);
        } else {
            match sys::getsockname(fd) {
                Ok(a) => sock.family = a.family(),
                Err(e) => {
                    self.sock_error(&mut sock, "Unable to obtain socket family", e);
                    sock.fd = -1;
                    return Err(SocketError::Failed);
                }
            }
        }
        match sys::get_flags(fd) {
            Ok(fl) => sock.blocking = fl & libc::O_NONBLOCK == 0,
            Err(e) => {
                self.sock_error(&mut sock, "Unable to obtain blocking state", e);
                sock.fd = -1;
                return Err(SocketError::Failed);
            }
        }
        Ok(sock)
    }

    // ------------------------------------------------------ modes

    fn set_blocking(&mut self, func: &str, socket: &mut Socket, block: bool) -> SocketResult<()> {
        ensure_open(func, socket)?;
        match sys::set_blocking(socket.fd, block) {
            Ok(()) => {
                socket.blocking = block;
                Ok(())
            }
            Err(e) => {
                let msg = if block {
                    "unable to set blocking mode"
                } else {
                    "unable to set nonblocking mode"
                };
                self.sock_error(socket, msg, e);
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_set_nonblock()` (for stream-backed sockets the VM first
    /// tries the stream, see [`Socket::set_blocking_flag`]).
    pub fn set_nonblock(&mut self, socket: &mut Socket) -> SocketResult<()> {
        self.set_blocking("socket_set_nonblock", socket, false)
    }

    /// `socket_set_block()`.
    pub fn set_block(&mut self, socket: &mut Socket) -> SocketResult<()> {
        self.set_blocking("socket_set_block", socket, true)
    }

    /// `socket_listen(Socket $socket, int $backlog = 0)`.
    pub fn listen(&mut self, socket: &mut Socket, backlog: i64) -> SocketResult<()> {
        ensure_open("socket_listen", socket)?;
        sys::listen(socket.fd, c_int(backlog)).map_err(|e| {
            self.sock_error(socket, "unable to listen on socket", e);
            SocketError::Failed
        })
    }

    /// `socket_close()`.  For stream-backed sockets the VM closes the
    /// stream (which closes the descriptor); this then only forgets it.
    pub fn close(&mut self, socket: &mut Socket) -> SocketResult<()> {
        ensure_open("socket_close", socket)?;
        if !socket.stream_owned {
            sys::close(socket.fd);
        }
        socket.stream_owned = false;
        socket.fd = -1;
        Ok(())
    }

    /// `socket_shutdown(Socket $socket, int $mode = 2)`.
    pub fn shutdown(&mut self, socket: &mut Socket, mode: i64) -> SocketResult<()> {
        ensure_open("socket_shutdown", socket)?;
        if !(0..=2).contains(&mode) {
            return Err(arg_value_error(
                "socket_shutdown",
                2,
                "mode",
                "must be one of SHUT_RD, SHUT_WR or SHUT_RDWR",
            ));
        }
        sys::shutdown(socket.fd, c_int(mode)).map_err(|e| {
            self.sock_error(socket, "Unable to shutdown socket", e);
            SocketError::Failed
        })
    }

    /// `socket_atmark()`.
    pub fn atmark(&mut self, socket: &mut Socket) -> SocketResult<bool> {
        ensure_open("socket_atmark", socket)?;
        sys::atmark(socket.fd).map_err(|e| {
            self.sock_error(socket, "Unable to apply sockmark", e);
            SocketError::Failed
        })
    }

    // ------------------------------------------------------ addresses

    /// `php_set_inet_addr()`.
    pub(crate) fn inet_addr(&mut self, sock: &mut Socket, addr: &[u8]) -> Option<[u8; 4]> {
        let c = sys::cstr(addr);
        if let Some(ip) = sys::pton4(&c) {
            return Some(ip);
        }
        let res = if addr.len() > 255 {
            Err(1)
        } else {
            sys::gethostbyname(&c)
        };
        match res {
            Err(h) => {
                self.sock_error(sock, "Host lookup failed", -10000 - h);
                None
            }
            Ok((ty, _)) if ty != libc::AF_INET => {
                self.warn("Host lookup failed: Non AF_INET domain returned on AF_INET socket");
                None
            }
            Ok((_, a)) => {
                let mut ip = [0u8; 4];
                let n = a.len().min(4);
                ip[..n].copy_from_slice(&a[..n]);
                Some(ip)
            }
        }
    }

    /// `php_set_inet6_addr()`: the address and the scope id when a `%scope`
    /// suffix was given.
    pub(crate) fn inet6_addr(
        &mut self,
        sock: &mut Socket,
        addr: &[u8],
    ) -> Option<([u8; 16], Option<u32>)> {
        let c = sys::cstr(addr);
        let ip = if let Some(ip) = sys::pton6(&c) {
            ip
        } else {
            match sys::getaddrinfo(
                &c,
                None,
                libc::AI_V4MAPPED | libc::AI_ADDRCONFIG,
                libc::AF_INET6,
                0,
                0,
            ) {
                Err((_, h)) => {
                    self.sock_error(sock, "Host lookup failed", -10000 - h);
                    return None;
                }
                Ok(list) => {
                    let Some(first) = list.first() else {
                        self.sock_error(sock, "Host lookup failed", -10000 - sys::h_errno());
                        return None;
                    };
                    if first.family != libc::AF_INET6
                        || first.addr.raw().1 != std::mem::size_of::<libc::sockaddr_in6>()
                    {
                        self.warn(
                            "Host lookup failed: Non AF_INET6 domain returned on AF_INET6 socket",
                        );
                        return None;
                    }
                    first.addr.ip6()
                }
            }
        };
        let cbytes = c.as_bytes();
        let scope = cbytes.iter().position(|&b| b == b'%').map(|p| {
            let s = &cbytes[p + 1..];
            match value::is_numeric(s) {
                Some(value::Numeric::Int(l)) => {
                    u32::try_from(l).ok().filter(|&v| v > 0).unwrap_or(0)
                }
                Some(value::Numeric::Float(_)) | None => self.string_to_if_index(s).unwrap_or(0),
            }
        });
        Some((ip, scope))
    }

    /// `php_string_to_if_index()`.
    pub(crate) fn string_to_if_index(&mut self, name: &[u8]) -> Option<u32> {
        match sys::if_nametoindex(&sys::cstr(name)) {
            0 => {
                self.warn(format!(
                    "No interface with name \"{}\" could be found",
                    String::from_utf8_lossy(&sys::cstr(name).into_bytes())
                ));
                None
            }
            i => Some(i),
        }
    }

    fn unix_addr(func: &str, arg: u32, addr: &[u8]) -> SocketResult<sys::SockAddr> {
        if addr.len() >= sys::SUN_PATH_LEN {
            return Err(arg_value_error(
                func,
                arg,
                "address",
                &format!("must be less than {}", sys::SUN_PATH_LEN),
            ));
        }
        Ok(sys::SockAddr::unix(addr, sys::SUN_PATH_OFFSET + addr.len()))
    }

    /// `socket_connect(Socket $socket, string $address, ?int $port = null)`.
    pub fn connect(
        &mut self,
        socket: &mut Socket,
        address: &[u8],
        port: Option<i64>,
    ) -> SocketResult<()> {
        const F: &str = "socket_connect";
        ensure_open(F, socket)?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let addr = match socket.family {
            libc::AF_INET6 => {
                let p = port.ok_or_else(|| {
                    arg_value_error(
                        F,
                        3,
                        "port",
                        "cannot be null when the socket type is AF_INET6",
                    )
                })?;
                let Some((ip, scope)) = self.inet6_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet6(ip, p as u16, 0, scope.unwrap_or(0))
            }
            libc::AF_INET => {
                let p = port.ok_or_else(|| {
                    arg_value_error(
                        F,
                        3,
                        "port",
                        "cannot be null when the socket type is AF_INET",
                    )
                })?;
                let Some(ip) = self.inet_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet(ip, p as u16)
            }
            libc::AF_UNIX => Self::unix_addr(F, 2, address)?,
            _ => {
                return Err(arg_value_error(
                    F,
                    1,
                    "socket",
                    "must be one of AF_UNIX, AF_INET, or AF_INET6",
                ));
            }
        };
        sys::connect(socket.fd, &addr).map_err(|e| {
            self.sock_error(socket, "unable to connect", e);
            SocketError::Failed
        })
    }

    /// `socket_bind(Socket $socket, string $address, int $port = 0)`.
    pub fn bind(&mut self, socket: &mut Socket, address: &[u8], port: i64) -> SocketResult<()> {
        const F: &str = "socket_bind";
        ensure_open(F, socket)?;
        let port = u16::try_from(port)
            .map_err(|_| arg_value_error(F, 3, "port", "must be between 0 and 65535"))?;
        let addr = match socket.family {
            libc::AF_UNIX => Self::unix_addr(F, 2, address)?,
            libc::AF_INET => {
                let Some(ip) = self.inet_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet(ip, port)
            }
            libc::AF_INET6 => {
                let Some((ip, scope)) = self.inet6_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet6(ip, port, 0, scope.unwrap_or(0))
            }
            libc::AF_PACKET => {
                // getsockname() with a pointer-sized length, as PHP does.
                let Ok(mut a) = sys::getsockname(socket.fd) else {
                    return Err(SocketError::ValueError("invalid AF_PACKET socket".into()));
                };
                #[allow(clippy::cast_possible_wrap)]
                a.set_ll_ifindex(sys::if_nametoindex(&sys::cstr(address)) as i32);
                a.set_len(std::mem::size_of::<libc::sockaddr_ll>());
                a
            }
            _ => {
                return Err(arg_value_error(
                    F,
                    1,
                    "socket",
                    "must be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6",
                ));
            }
        };
        sys::bind(socket.fd, &addr).map_err(|e| {
            self.sock_error(socket, "Unable to bind address", e);
            SocketError::Failed
        })
    }

    fn name_of(&mut self, func: &str, socket: &mut Socket, peer: bool) -> SocketResult<SocketName> {
        ensure_open(func, socket)?;
        let r = if peer {
            sys::getpeername(socket.fd)
        } else {
            sys::getsockname(socket.fd)
        };
        let a = r.map_err(|e| {
            let msg = if peer {
                "unable to retrieve peer name"
            } else {
                "unable to retrieve socket name"
            };
            self.sock_error(socket, msg, e);
            SocketError::Failed
        })?;
        match a.family() {
            libc::AF_INET6 => Ok(SocketName {
                address: sys::ntop6(a.ip6()).into_bytes(),
                port: Some(a.port().into()),
            }),
            libc::AF_INET => Ok(SocketName {
                address: sys::ntop4(a.ip4()).into_bytes(),
                port: Some(a.port().into()),
            }),
            libc::AF_UNIX => {
                let p = a.sun_path();
                let end = p.iter().position(|&b| b == 0).unwrap_or(p.len());
                Ok(SocketName {
                    address: p[..end].to_vec(),
                    port: None,
                })
            }
            #[allow(clippy::cast_sign_loss)]
            libc::AF_PACKET if !peer => {
                let name = sys::if_indextoname(a.ll_ifindex() as u32)
                    .ok_or_else(|| SocketError::Error("invalid interface index".into()))?;
                Ok(SocketName {
                    address: name,
                    port: Some(a.ll_ifindex().into()),
                })
            }
            _ => Err(arg_value_error(
                func,
                1,
                "socket",
                if peer {
                    "must be one of AF_UNIX, AF_INET, or AF_INET6"
                } else {
                    "must be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6"
                },
            )),
        }
    }

    /// `socket_getsockname(Socket $socket, &$address, &$port = null)`.
    pub fn getsockname(&mut self, socket: &mut Socket) -> SocketResult<SocketName> {
        self.name_of("socket_getsockname", socket, false)
    }

    /// `socket_getpeername(Socket $socket, &$address, &$port = null)`.
    pub fn getpeername(&mut self, socket: &mut Socket) -> SocketResult<SocketName> {
        self.name_of("socket_getpeername", socket, true)
    }

    // ------------------------------------------------------ I/O

    /// `socket_write(Socket $socket, string $data, ?int $length = null)`.
    pub fn write(
        &mut self,
        socket: &mut Socket,
        data: &[u8],
        length: Option<i64>,
    ) -> SocketResult<i64> {
        ensure_open("socket_write", socket)?;
        if length.is_some_and(|l| l < 0) {
            return Err(arg_value_error(
                "socket_write",
                3,
                "length",
                "must be greater than or equal to 0",
            ));
        }
        let n = length.map_or(data.len(), |l| {
            usize::try_from(l).unwrap_or(usize::MAX).min(data.len())
        });
        match sys::write(socket.fd, &data[..n]) {
            Ok(w) => Ok(i64::try_from(w).unwrap_or(i64::MAX)),
            Err(e) => {
                self.sock_error(socket, "unable to write to socket", e);
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_read(Socket $socket, int $length, int $mode = PHP_BINARY_READ)`.
    pub fn read(&mut self, socket: &mut Socket, length: i64, mode: i64) -> SocketResult<Vec<u8>> {
        ensure_open("socket_read", socket)?;
        if length <= 0 || length == i64::MAX {
            return Err(SocketError::Failed);
        }
        let len = recv_buffer_len(length);
        let r = if mode == PHP_NORMAL_READ {
            normal_read(socket.fd, len)
        } else {
            let mut buf = vec![0u8; len];
            sys::recv(socket.fd, &mut buf, 0).map(|n| {
                buf.truncate(n);
                buf
            })
        };
        match r {
            Ok(b) => Ok(b),
            Err(e) if e == libc::EAGAIN => {
                socket.error = e;
                self.last_error = e;
                Err(SocketError::Failed)
            }
            Err(e) => {
                self.sock_error(socket, "unable to read from socket", e);
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_recv(Socket $socket, &$data, int $length, int $flags)`.
    pub fn recv(&mut self, socket: &mut Socket, length: i64, flags: i64) -> SocketResult<Recv> {
        ensure_open("socket_recv", socket)?;
        if length <= 0 || length == i64::MAX {
            return Err(SocketError::Failed);
        }
        let mut buf = vec![0u8; recv_buffer_len(length)];
        match sys::recv(socket.fd, &mut buf, c_int(flags)) {
            Ok(0) => Ok(Recv {
                data: None,
                result: Some(0),
            }),
            Ok(n) => {
                buf.truncate(n);
                Ok(Recv {
                    data: Some(buf),
                    result: Some(i64::try_from(n).unwrap_or(i64::MAX)),
                })
            }
            Err(e) => {
                self.sock_error(socket, "Unable to read from socket", e);
                Ok(Recv {
                    data: None,
                    result: None,
                })
            }
        }
    }

    /// `socket_send(Socket $socket, string $data, int $length, int $flags)`.
    pub fn send(
        &mut self,
        socket: &mut Socket,
        data: &[u8],
        length: i64,
        flags: i64,
    ) -> SocketResult<i64> {
        ensure_open("socket_send", socket)?;
        if length < 0 {
            return Err(arg_value_error(
                "socket_send",
                3,
                "length",
                "must be greater than or equal to 0",
            ));
        }
        let n = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(data.len());
        match sys::send(socket.fd, &data[..n], c_int(flags)) {
            Ok(w) => Ok(i64::try_from(w).unwrap_or(i64::MAX)),
            Err(e) => {
                self.sock_error(socket, "Unable to write to socket", e);
                Err(SocketError::Failed)
            }
        }
    }

    /// `socket_recvfrom(Socket $socket, &$data, int $length, int $flags,
    /// &$address, &$port = null)`; `port_given` tells whether `$port` was
    /// passed.
    #[allow(clippy::too_many_lines)]
    pub fn recvfrom(
        &mut self,
        socket: &mut Socket,
        length: i64,
        flags: i64,
        port_given: bool,
    ) -> SocketResult<RecvFrom> {
        const F: &str = "socket_recvfrom";
        ensure_open(F, socket)?;
        if socket.family == libc::AF_PACKET {
            let allowed = libc::MSG_OOB
                | libc::MSG_PEEK
                | libc::MSG_WAITALL
                | libc::MSG_DONTWAIT
                | libc::MSG_ERRQUEUE
                | libc::MSG_CMSG_CLOEXEC;
            if flags & !i64::from(allowed) != 0 {
                return Err(arg_value_error(F, 4, "flags", "must be a combination of MSG_OOB, MSG_PEEK, MSG_WAITALL, MSG_DONTWAIT, MSG_ERRQUEUE, and MSG_CMSG_CLOEXEC for AF_PACKET sockets"));
            }
        }
        if length <= 0 || length > i64::MAX - 1 {
            return Err(SocketError::Failed);
        }
        let need_port = || {
            SocketError::ArgumentCountError(
                "socket_recvfrom() expects exactly 6 arguments when argument #1 ($socket) is of type AF_INET or AF_INET6".into(),
            )
        };
        let (init, init_len, fail_msg) = match socket.family {
            libc::AF_UNIX => (
                sys::SockAddr::unix(&[], 0),
                std::mem::size_of::<libc::sockaddr_un>(),
                "Unable to recvfrom",
            ),
            libc::AF_INET => {
                if !port_given {
                    return Err(need_port());
                }
                (
                    sys::SockAddr::inet([0; 4], 0),
                    std::mem::size_of::<libc::sockaddr_in>(),
                    "Unable to recvfrom",
                )
            }
            libc::AF_INET6 => {
                if !port_given {
                    return Err(need_port());
                }
                (
                    sys::SockAddr::inet6([0; 16], 0, 0, 0),
                    std::mem::size_of::<libc::sockaddr_in6>(),
                    "unable to recvfrom",
                )
            }
            libc::AF_PACKET => (
                sys::SockAddr::from_raw(&[], 0),
                std::mem::size_of::<libc::sockaddr_ll>(),
                "unable to recvfrom",
            ),
            _ => {
                return Err(arg_value_error(
                    F,
                    1,
                    "socket",
                    "must be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6",
                ));
            }
        };
        let mut buf = vec![0u8; recv_buffer_len(length)];
        let (n, addr) = match sys::recvfrom(socket.fd, &mut buf, c_int(flags), init, init_len) {
            Ok(r) => r,
            Err(e) => {
                self.sock_error(socket, fail_msg, e);
                return Err(SocketError::Failed);
            }
        };
        let result = i64::try_from(n).unwrap_or(i64::MAX);
        buf.truncate(n.min(buf.len()));
        let (address, port) = match socket.family {
            libc::AF_UNIX => {
                let p = addr.sun_path();
                let end = p.iter().position(|&b| b == 0).unwrap_or(p.len());
                (p[..end].to_vec(), None)
            }
            libc::AF_INET => (
                sys::ntop4(addr.ip4()).into_bytes(),
                Some(addr.port().into()),
            ),
            libc::AF_INET6 => {
                let s = sys::ntop6(addr.ip6());
                (
                    if s.is_empty() {
                        b"::".to_vec()
                    } else {
                        s.into_bytes()
                    },
                    Some(addr.port().into()),
                )
            }
            _ => {
                #[allow(clippy::cast_sign_loss)]
                let Some(name) = sys::if_indextoname(addr.ll_ifindex() as u32) else {
                    let e = sys::errno();
                    self.sock_error(socket, "unable to get the interface name", e);
                    return Err(SocketError::Failed);
                };
                (name, port_given.then(|| addr.ll_ifindex().into()))
            }
        };
        Ok(RecvFrom {
            data: buf,
            address,
            port,
            result,
        })
    }

    /// `socket_sendto(Socket $socket, string $data, int $length, int $flags,
    /// string $address, ?int $port = null)`.
    pub fn sendto(
        &mut self,
        socket: &mut Socket,
        data: &[u8],
        length: i64,
        flags: i64,
        address: &[u8],
        port: Option<i64>,
    ) -> SocketResult<i64> {
        const F: &str = "socket_sendto";
        ensure_open(F, socket)?;
        if socket.family != libc::AF_PACKET && !(0..=65535).contains(&port.unwrap_or(0)) {
            return Err(arg_value_error(F, 6, "port", "must be between 0 and 65535"));
        }
        if length < 0 {
            return Err(arg_value_error(
                F,
                3,
                "length",
                "must be greater than or equal to 0",
            ));
        }
        let n = usize::try_from(length)
            .unwrap_or(usize::MAX)
            .min(data.len());
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let addr = match socket.family {
            libc::AF_UNIX => {
                if address.len() >= sys::SUN_PATH_LEN {
                    return Err(arg_value_error(
                        F,
                        5,
                        "address",
                        &format!("must be less than {}", sys::SUN_PATH_LEN),
                    ));
                }
                // SUN_LEN: up to the first NUL of the copied path.
                let end = address
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(address.len());
                sys::SockAddr::unix(address, sys::SUN_PATH_OFFSET + end)
            }
            libc::AF_INET => {
                let p = port.ok_or_else(|| {
                    arg_value_error(
                        F,
                        6,
                        "port",
                        "cannot be null when the socket type is AF_INET",
                    )
                })?;
                let Some(ip) = self.inet_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet(ip, p as u16)
            }
            libc::AF_INET6 => {
                let p = port.ok_or_else(|| {
                    arg_value_error(
                        F,
                        6,
                        "port",
                        "cannot be null when the socket type is AF_INET6",
                    )
                })?;
                let Some((ip, scope)) = self.inet6_addr(socket, address) else {
                    return Err(SocketError::Failed);
                };
                sys::SockAddr::inet6(ip, p as u16, 0, scope.unwrap_or(0))
            }
            libc::AF_PACKET => {
                let p = port.ok_or_else(|| {
                    arg_value_error(
                        F,
                        6,
                        "port",
                        "cannot be null when the socket type is AF_PACKET",
                    )
                })?;
                sys::SockAddr::packet(p as i32)
            }
            _ => {
                return Err(arg_value_error(
                    F,
                    1,
                    "socket",
                    "must be one of AF_UNIX, AF_PACKET, AF_INET, or AF_INET6",
                ));
            }
        };
        match sys::sendto(socket.fd, &data[..n], c_int(flags), &addr) {
            Ok(w) => Ok(i64::try_from(w).unwrap_or(i64::MAX)),
            Err(e) => {
                self.sock_error(socket, "Unable to write to socket", e);
                Err(SocketError::Failed)
            }
        }
    }

    // ------------------------------------------------------ select

    /// `socket_select(?array &$read, ?array &$write, ?array &$except, ?int
    /// $seconds, int $microseconds = 0)`.
    ///
    /// Each array is given as its element values (`None` for a `null`
    /// argument).  Returns the count and, per array, the positions of the
    /// ready elements; the VM rebuilds each array from those elements,
    /// keeping their keys.
    pub fn select(
        &mut self,
        arrays: [Option<&[Value]>; 3],
        seconds: Option<i64>,
        microseconds: i64,
    ) -> SocketResult<(i64, [Option<Vec<usize>>; 3])> {
        const F: &str = "socket_select";
        let names = ["read", "write", "except"];
        let mut fds: [Vec<i32>; 3] = Default::default();
        let mut max_fd = 0;
        let mut max_count = 0;
        for (i, arr) in arrays.iter().enumerate() {
            let Some(arr) = arr else { continue };
            for v in *arr {
                let n = u32::try_from(i + 1).unwrap_or(1);
                match v {
                    Value::Socket { fd } if *fd < 0 => {
                        return Err(arg_type_error(F, n, names[i], "contains a closed socket"));
                    }
                    Value::Socket { fd } => {
                        fds[i].push(*fd);
                        max_fd = max_fd.max(*fd);
                    }
                    other => {
                        return Err(arg_type_error(
                            F,
                            n,
                            names[i],
                            &format!(
                                "must only have elements of type Socket, {} given",
                                other.value_name()
                            ),
                        ));
                    }
                }
            }
            max_count = max_count.max(arr.len());
        }
        if max_count == 0 {
            return Err(SocketError::ValueError(
                "socket_select(): At least one array argument must be passed".into(),
            ));
        }
        if max_fd >= FD_SETSIZE {
            self.warn(format!(
                "You MUST recompile PHP with a larger value of FD_SETSIZE.\nIt is set to {FD_SETSIZE}, but you have descriptors numbered at least as high as {max_fd}.\n --enable-fd-setsize={} is recommended, but you may want to set it\nto equal the maximum number of open files supported by your system,\nin order to avoid seeing this error again at a later date.",
                (max_fd + 1024) & !1023
            ));
            return Err(SocketError::Failed);
        }
        let timeout = seconds.map(|s| {
            if microseconds > 999_999 {
                (s + microseconds / 1_000_000, microseconds % 1_000_000)
            } else {
                (s, microseconds)
            }
        });
        match sys::select([&fds[0], &fds[1], &fds[2]], max_fd, timeout) {
            Ok((n, ready)) => {
                let mut out: [Option<Vec<usize>>; 3] = Default::default();
                for i in 0..3 {
                    if arrays[i].is_some() {
                        out[i] = Some(
                            fds[i]
                                .iter()
                                .enumerate()
                                .filter(|(_, fd)| ready[i].contains(fd))
                                .map(|(pos, _)| pos)
                                .collect(),
                        );
                    }
                }
                Ok((n.into(), out))
            }
            Err(e) => {
                self.last_error = e;
                self.warn(format!("Unable to select [{e}]: {}", sockets_strerror(e)));
                Err(SocketError::Failed)
            }
        }
    }

    // ------------------------------------------------------ addrinfo

    /// `socket_addrinfo_lookup(string $host, ?string $service = null, array
    /// $hints = [], &$error_code = null)`.  `Ok(Err(code))` is PHP's
    /// `false` with `$error_code = code`.
    pub fn addrinfo_lookup(
        &mut self,
        host: &[u8],
        service: Option<&[u8]>,
        hints: Option<&Value>,
    ) -> SocketResult<Result<Vec<AddressInfo>, i64>> {
        const F: &str = "socket_addrinfo_lookup";
        let keys_err = || {
            arg_value_error(F, 3, "hints", "must only contain array keys \"ai_flags\", \"ai_socktype\", \"ai_protocol\", or \"ai_family\"")
        };
        let (mut flags, mut socktype, mut protocol, mut family) = (0, 0, 0, 0);
        if let Some(Value::Array(t)) = hints {
            let packed = !t.is_empty()
                && t.iter()
                    .enumerate()
                    .all(|(i, (k, _))| *k == Key::Int(i64::try_from(i).unwrap_or(-1)));
            if packed {
                return Err(keys_err());
            }
            for (k, v) in t {
                let Key::Str(k) = k else {
                    return Err(keys_err());
                };
                let name = String::from_utf8_lossy(k).into_owned();
                if !["ai_flags", "ai_socktype", "ai_protocol", "ai_family"].contains(&name.as_str())
                {
                    return Err(keys_err());
                }
                let val = v.try_long().ok_or_else(|| {
                    arg_type_error(
                        F,
                        3,
                        "hints",
                        &format!(
                            "\"{name}\" key must be of type int, {} given",
                            v.type_name()
                        ),
                    )
                })?;
                if name == "ai_family" {
                    if val != 0
                        && val != i64::from(libc::AF_INET)
                        && val != i64::from(libc::AF_INET6)
                    {
                        return Err(arg_value_error(
                            F,
                            3,
                            "hints",
                            "\"ai_family\" key must be AF_INET or AF_INET6",
                        ));
                    }
                    family = c_int(val);
                    continue;
                }
                if !(0..=i64::from(i32::MAX)).contains(&val) {
                    return Err(arg_value_error(
                        F,
                        3,
                        "hints",
                        &format!("\"{name}\" key must be between 0 and {}", i32::MAX),
                    ));
                }
                match name.as_str() {
                    "ai_flags" => flags = c_int(val),
                    "ai_socktype" => socktype = c_int(val),
                    _ => protocol = c_int(val),
                }
            }
        }
        let svc = service.map(sys::cstr);
        match sys::getaddrinfo(
            &sys::cstr(host),
            svc.as_deref(),
            flags,
            family,
            socktype,
            protocol,
        ) {
            Err((code, _)) => Ok(Err(code.into())),
            Ok(list) => Ok(Ok(list
                .into_iter()
                .filter(|e| e.family == libc::AF_INET || e.family == libc::AF_INET6)
                .map(|entry| AddressInfo { entry })
                .collect())),
        }
    }

    fn addrinfo_socket(&mut self, ai: &AddressInfo, connect: bool) -> SocketResult<Socket> {
        let e = &ai.entry;
        let protocol = eth_proto(e.protocol.into(), e.family.into());
        let fd = match sys::socket(e.family, e.socktype, c_int(protocol)) {
            Ok(fd) => fd,
            Err(err) => {
                self.last_error = err;
                self.warn(format!(
                    "Unable to create socket [{err}]: {}",
                    sockets_strerror(err)
                ));
                return Err(SocketError::Failed);
            }
        };
        let mut sock = Socket::new(fd, e.family);
        let r = if connect {
            sys::connect(fd, &e.addr)
        } else {
            sys::bind(fd, &e.addr)
        };
        if let Err(err) = r {
            let msg = if connect {
                "Unable to connect address"
            } else {
                "Unable to bind address"
            };
            self.sock_error(&mut sock, msg, err);
            return Err(SocketError::Failed);
        }
        Ok(sock)
    }

    /// `socket_addrinfo_bind(AddressInfo $address)`.
    pub fn addrinfo_bind(&mut self, address: &AddressInfo) -> SocketResult<Socket> {
        self.addrinfo_socket(address, false)
    }

    /// `socket_addrinfo_connect(AddressInfo $address)`.
    pub fn addrinfo_connect(&mut self, address: &AddressInfo) -> SocketResult<Socket> {
        self.addrinfo_socket(address, true)
    }

    // ------------------------------------------------------ cmsg

    /// `socket_cmsg_space(int $level, int $type, int $num = 0)`.
    pub fn cmsg_space(&self, level: i64, ty: i64, num: i64) -> SocketResult<i64> {
        const F: &str = "socket_cmsg_space";
        let range = format!("must be between {} and {}", i32::MIN, i32::MAX);
        let lvl = i32::try_from(level).map_err(|_| arg_value_error(F, 1, "level", &range))?;
        let typ = i32::try_from(ty).map_err(|_| arg_value_error(F, 2, "type", &range))?;
        let n = i32::try_from(num).map_err(|_| arg_value_error(F, 3, "num", &range))?;
        if n < 0 {
            return Err(arg_value_error(
                F,
                3,
                "num",
                "must be greater than or equal to 0",
            ));
        }
        let entry = conversions::ancillary(lvl, typ).ok_or_else(|| {
            SocketError::ValueError(format!(
                "Pair level {level} and/or type {ty} is not supported"
            ))
        })?;
        let n = usize::try_from(n).unwrap_or(0);
        let size = entry.size + n * entry.var_el_size;
        if entry.var_el_size > 0 {
            let total = sys::cmsg_space(size);
            if size > u32::MAX as usize || total < size {
                return Err(arg_value_error(F, 3, "num", "is too large"));
            }
        }
        Ok(i64::try_from(sys::cmsg_space(size)).unwrap_or(i64::MAX))
    }
}

impl AddressInfo {
    /// `socket_addrinfo_explain(AddressInfo $address)`.
    pub fn explain(&self) -> OutValue {
        let e = &self.entry;
        let mut pairs = vec![
            ("ai_flags", OutValue::Int(e.flags.into())),
            ("ai_family", OutValue::Int(e.family.into())),
            ("ai_socktype", OutValue::Int(e.socktype.into())),
            ("ai_protocol", OutValue::Int(e.protocol.into())),
        ];
        if let Some(c) = &e.canonname {
            pairs.push(("ai_canonname", OutValue::Str(c.clone())));
        }
        let sockaddr = match e.family {
            libc::AF_INET => OutValue::assoc(vec![
                ("sin_port", OutValue::Int(e.addr.port().into())),
                (
                    "sin_addr",
                    OutValue::Str(sys::ntop4(e.addr.ip4()).into_bytes()),
                ),
            ]),
            libc::AF_INET6 => OutValue::assoc(vec![
                ("sin6_port", OutValue::Int(e.addr.port().into())),
                (
                    "sin6_addr",
                    OutValue::Str(sys::ntop6(e.addr.ip6()).into_bytes()),
                ),
            ]),
            _ => OutValue::Array(Vec::new()),
        };
        pairs.push(("ai_addr", sockaddr));
        OutValue::assoc(pairs)
    }

    /// `ai_family`.
    pub fn family(&self) -> i32 {
        self.entry.family
    }
}

/// `PHP_ETH_PROTO_CHECK`: `AF_PACKET` protocols are passed in network order.
fn eth_proto(protocol: i64, family: i64) -> i64 {
    if protocol <= i64::from(u16::MAX) && family == i64::from(libc::AF_PACKET) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        i64::from((protocol as u16).to_be())
    } else {
        protocol
    }
}

/// PHP allocates `length` bytes up front; huge requests are capped (no
/// single receive can exceed this) to avoid exhausting memory.
fn recv_buffer_len(length: i64) -> usize {
    const CAP: usize = 64 << 20;
    usize::try_from(length).unwrap_or(usize::MAX).min(CAP)
}

/// `php_read()`: byte-at-a-time read stopping after `\n` or `\r`
/// (included), mirroring the C loop exactly (including its EOF and
/// non-blocking rules).
fn normal_read(fd: i32, maxlen: usize) -> Result<Vec<u8>, i32> {
    let nonblock = sys::get_flags(fd)? & libc::O_NONBLOCK != 0;
    let mut buf = vec![0u8; maxlen + 1];
    let mut n = 0usize;
    let mut m: isize = 0;
    let mut no_read = 0;
    while buf[n] != b'\n' && buf[n] != b'\r' && n < maxlen {
        if m > 0 {
            n += 1;
        } else if m == 0 {
            no_read += 1;
            if nonblock && no_read >= 2 {
                buf.truncate(n);
                return Ok(buf);
            }
            if no_read > 200 {
                return Err(libc::ECONNRESET);
            }
        }
        if n < maxlen {
            m = match sys::recv(fd, &mut buf[n..=n], 0) {
                Ok(k) => isize::try_from(k).unwrap_or(1),
                Err(e) if e == libc::EAGAIN || e == libc::ESPIPE => -1,
                Err(e) => return Err(e),
            };
        }
    }
    if n < maxlen {
        n += 1;
    }
    buf.truncate(n);
    Ok(buf)
}

#[cfg(test)]
mod tests;
