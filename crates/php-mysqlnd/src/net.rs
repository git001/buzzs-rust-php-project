// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Transport (`mysqlnd_vio.c`) and packet framing
//! (`mysqlnd_protocol_frame_codec.c`): TCP / Unix-socket streams with
//! optional TLS, read timeouts, 16 MiB packet splitting, sequence-number
//! checking and the compressed protocol.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use openssl::ssl::{SslConnector, SslFiletype, SslMethod, SslStream, SslVerifyMode};

use crate::consts::{
    Command, CR_CONNECTION_ERROR, CR_SERVER_GONE_ERROR, MAX_PACKET_SIZE, MSG_SERVER_GONE,
};
use crate::diag;
use crate::error::{ErrorInfo, ServerError};
use crate::inflate;
use crate::stats::{inc_conn, Stat, Stats};

/// Size of the packet header.
pub const HEADER_SIZE: usize = 4;
/// Size of the extra compressed-protocol header.
pub const COMPRESSED_HEADER_SIZE: usize = 3;

/// TLS peer verification mode (`enum mysqlnd_ssl_peer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslPeer {
    /// Decide based on whether any SSL option was set.
    #[default]
    Default,
    /// Verify the peer.
    Verify,
    /// Don't verify.
    DontVerify,
}

/// `MYSQLND_VIO_OPTIONS`
#[derive(Debug, Clone, Default)]
pub struct VioOptions {
    /// Connect timeout in seconds (0 = `default_socket_timeout`).
    pub timeout_connect: u32,
    /// Read timeout in seconds (0 = none).
    pub timeout_read: u32,
    /// Stream chunk size.
    pub net_read_buffer_size: usize,
    /// Client private key file.
    pub ssl_key: Option<String>,
    /// Client certificate file.
    pub ssl_cert: Option<String>,
    /// CA file.
    pub ssl_ca: Option<String>,
    /// CA directory.
    pub ssl_capath: Option<String>,
    /// Cipher list.
    pub ssl_cipher: Option<String>,
    /// Key passphrase.
    pub ssl_passphrase: Option<String>,
    /// Peer verification mode.
    pub ssl_verify_peer: SslPeer,
}

#[derive(Debug)]
enum Plain {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Plain {
    fn set_read_timeout(&self, d: Option<Duration>) {
        let _ = match self {
            Plain::Tcp(s) => s.set_read_timeout(d),
            Plain::Unix(s) => s.set_read_timeout(d),
        };
    }
}

impl Read for Plain {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Plain::Tcp(s) => s.read(buf),
            Plain::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Plain {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Plain::Tcp(s) => s.write(buf),
            Plain::Unix(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Plain::Tcp(s) => s.flush(),
            Plain::Unix(s) => s.flush(),
        }
    }
}

#[derive(Debug)]
enum Stream {
    Plain(Plain),
    Tls(Box<SslStream<Plain>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(p) => p.read(buf),
            Stream::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(p) => p.write(buf),
            Stream::Tls(t) => t.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(p) => p.flush(),
            Stream::Tls(t) => t.flush(),
        }
    }
}

/// Text of an OS error the way `strerror()` prints it.
pub fn strerror(e: &io::Error) -> String {
    if e.kind() == io::ErrorKind::TimedOut {
        return "Connection timed out".to_string();
    }
    let s = e.to_string();
    match s.find(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

/// Splits `host:port` / `[v6]:port` (`php_network_parse_network_address_with_port`).
fn parse_host_port(addr: &str) -> Option<(String, u16)> {
    if let Some(rest) = addr.strip_prefix('[') {
        let close = rest.find(']')?;
        let host = &rest[..close];
        let port = rest[close + 1..].strip_prefix(':')?;
        return Some((host.to_string(), atoi_port(port)));
    }
    let colon = addr.rfind(':')?;
    Some((addr[..colon].to_string(), atoi_port(&addr[colon + 1..])))
}

fn atoi_port(s: &str) -> u16 {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let p = digits.parse::<i64>().map_or(0, |v| v as u16);
    p
}

/// The transport + framing state of a connection (`MYSQLND_VIO` + `MYSQLND_PFC`).
#[derive(Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Net {
    stream: Option<Stream>,
    /// Expected next sequence number.
    pub packet_no: u8,
    /// Sequence number of the compressed envelope.
    pub compressed_envelope_packet_no: u8,
    /// Compressed protocol active.
    pub compressed: bool,
    /// `MYSQLND_PROTOCOL_FLAG_USE_COMPRESSION` was requested via options.
    pub use_compression: bool,
    unc: Vec<u8>,
    unc_pos: usize,
    prefetch: Vec<u8>,
    pre_pos: usize,
    /// TLS is active on the stream.
    pub ssl: bool,
    /// The stream is a Unix socket.
    pub unix: bool,
    /// `mysqlnd.net_cmd_buffer_size` for this connection.
    pub cmd_buffer_size: usize,
    /// Transport options.
    pub opts: VioOptions,
    /// `MYSQL_SERVER_PUBLIC_KEY` (PEM file path).
    pub server_public_key: Option<String>,
    /// Peer name for TLS verification.
    peer_name: String,
}

impl Net {
    /// Whether a stream is open.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// `mysqlnd_vio::close_stream`
    pub fn close(&mut self) {
        if let Some(Stream::Tls(mut t)) = self.stream.take() {
            let _ = t.shutdown();
        }
        self.ssl = false;
        self.prefetch.clear();
        self.pre_pos = 0;
        self.unc.clear();
        self.unc_pos = 0;
    }

    /// `mysqlnd_pfc::reset`
    pub fn reset(&mut self) {
        self.packet_no = 0;
        self.compressed_envelope_packet_no = 0;
    }

    /// `mysqlnd_vio::connect` for a `tcp://` or `unix://` transport string.
    pub fn connect(
        &mut self,
        scheme: &str,
        default_timeout: Duration,
        err: &mut ErrorInfo,
    ) -> bool {
        self.close();
        let timeout = if self.opts.timeout_connect > 0 {
            Duration::from_secs(u64::from(self.opts.timeout_connect))
        } else {
            default_timeout
        };
        let plain = if let Some(path) = scheme.strip_prefix("unix://").filter(|p| !p.is_empty()) {
            match UnixStream::connect(path) {
                Ok(s) => {
                    self.unix = true;
                    Plain::Unix(s)
                }
                Err(e) => {
                    err.set_client(CR_CONNECTION_ERROR, &strerror(&e));
                    return false;
                }
            }
        } else if let Some(target) = scheme.strip_prefix("tcp://").filter(|p| !p.is_empty()) {
            let Some((host, port)) = parse_host_port(target) else {
                err.set_client(CR_CONNECTION_ERROR, "Failed to parse address");
                return false;
            };
            self.peer_name.clone_from(&host);
            let addrs: Vec<_> = match (host.as_str(), port).to_socket_addrs() {
                Ok(a) => a.collect(),
                Err(e) => {
                    let msg = e.to_string();
                    let msg = msg
                        .strip_prefix("failed to lookup address information: ")
                        .unwrap_or(&msg);
                    err.set_client(
                        CR_CONNECTION_ERROR,
                        &format!("php_network_getaddresses: getaddrinfo for {host} failed: {msg}"),
                    );
                    return false;
                }
            };
            let mut last_err = None;
            let mut conn = None;
            for a in addrs {
                match TcpStream::connect_timeout(&a, timeout) {
                    Ok(s) => {
                        conn = Some(s);
                        break;
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            if let Some(s) = conn {
                let _ = s.set_nodelay(true);
                self.unix = false;
                Plain::Tcp(s)
            } else {
                let msg = last_err.map_or_else(
                    || "Unknown error while connecting".to_string(),
                    |e| strerror(&e),
                );
                err.set_client(CR_CONNECTION_ERROR, &msg);
                return false;
            }
        } else {
            err.set_client(CR_CONNECTION_ERROR, "No handler for this scheme");
            return false;
        };
        if self.opts.timeout_read > 0 {
            plain.set_read_timeout(Some(Duration::from_secs(u64::from(self.opts.timeout_read))));
        }
        self.stream = Some(Stream::Plain(plain));
        true
    }

    /// `mysqlnd_vio::enable_ssl`
    pub fn enable_ssl(&mut self) -> bool {
        let Some(Stream::Plain(plain)) = self.stream.take() else {
            return false;
        };
        match self.build_tls(plain) {
            Some(tls) => {
                self.stream = Some(Stream::Tls(Box::new(tls)));
                self.ssl = true;
                true
            }
            None => false,
        }
    }

    fn build_tls(&mut self, plain: Plain) -> Option<SslStream<Plain>> {
        let o = &mut self.opts;
        let any_flag = o.ssl_key.is_some()
            || o.ssl_cert.is_some()
            || o.ssl_ca.is_some()
            || o.ssl_capath.is_some()
            || o.ssl_passphrase.is_some()
            || o.ssl_cipher.is_some();
        if o.ssl_verify_peer == SslPeer::Default {
            o.ssl_verify_peer = if any_flag {
                SslPeer::Verify
            } else {
                SslPeer::DontVerify
            };
        }
        let verify = o.ssl_verify_peer == SslPeer::Verify;
        let mut b = SslConnector::builder(SslMethod::tls_client()).ok()?;
        if let Some(ca) = &o.ssl_ca {
            b.set_ca_file(ca).ok()?;
        }
        if let Some(capath) = &o.ssl_capath {
            let store = b.cert_store_mut();
            for entry in std::fs::read_dir(capath).ok()?.flatten() {
                if let Ok(pem) = std::fs::read(entry.path()) {
                    if let Ok(certs) = openssl::x509::X509::stack_from_pem(&pem) {
                        for c in certs {
                            let _ = store.add_cert(c);
                        }
                    }
                }
            }
        }
        let key_file = o.ssl_key.clone().or_else(|| o.ssl_cert.clone());
        if let Some(cert) = &o.ssl_cert {
            b.set_certificate_chain_file(cert).ok()?;
        }
        if let Some(key) = key_file {
            if let Some(pass) = &o.ssl_passphrase {
                let pem = std::fs::read(&key).ok()?;
                let pk =
                    openssl::pkey::PKey::private_key_from_pem_passphrase(&pem, pass.as_bytes())
                        .ok()?;
                b.set_private_key(&pk).ok()?;
            } else {
                b.set_private_key_file(&key, SslFiletype::PEM).ok()?;
            }
        }
        if let Some(c) = &o.ssl_cipher {
            b.set_cipher_list(c).ok()?;
        }
        if !verify {
            b.set_verify(SslVerifyMode::NONE);
        }
        let connector = b.build();
        let mut cfg = connector.configure().ok()?;
        cfg.set_verify_hostname(verify);
        cfg.set_use_server_name_indication(
            !self.peer_name.is_empty() && self.peer_name.parse::<std::net::IpAddr>().is_err(),
        );
        let name = if self.peer_name.is_empty() {
            "localhost".to_string()
        } else {
            self.peer_name.clone()
        };
        cfg.connect(&name, plain).ok()
    }

    /// Whether data (or EOF) can be read without blocking (`select()`
    /// readiness used by `mysqlnd_poll`).
    pub fn readable(&mut self) -> bool {
        if self.unc_pos < self.unc.len() || self.pre_pos < self.prefetch.len() {
            return true;
        }
        let (plain, pending) = match self.stream.as_ref() {
            None => return false,
            Some(Stream::Plain(p)) => (p, 0),
            Some(Stream::Tls(t)) => (t.get_ref(), t.ssl().pending()),
        };
        if pending > 0 {
            return true;
        }
        let mut b = [0u8; 1];
        let r = match plain {
            Plain::Tcp(s) => {
                let _ = s.set_nonblocking(true);
                let r = s.peek(&mut b);
                let _ = s.set_nonblocking(false);
                r
            }
            Plain::Unix(s) => {
                // no stable peek for Unix sockets: read one byte non-blocking
                // and keep it in the prefetch buffer
                let _ = s.set_nonblocking(true);
                let mut sref = s;
                let r = sref.read(&mut b);
                let _ = s.set_nonblocking(false);
                if let Ok(1) = r {
                    self.prefetch.drain(..self.pre_pos);
                    self.pre_pos = 0;
                    self.prefetch.push(b[0]);
                }
                r
            }
        };
        match r {
            Ok(_) => true,
            Err(e) => e.kind() != io::ErrorKind::WouldBlock,
        }
    }

    /// `mysqlnd_vio::network_read`: reads exactly `buf.len()` bytes.
    ///
    /// Like PHP's stream layer, data is read ahead in chunks of
    /// `net_read_buffer_size` bytes into a client-side buffer (so e.g. a
    /// forked child sees what the parent had already buffered).
    pub fn network_read(&mut self, buf: &mut [u8], stats: &mut Stats) -> bool {
        let mut done = 0;
        let mut ok = true;
        while done < buf.len() {
            if self.pre_pos < self.prefetch.len() {
                let n = (self.prefetch.len() - self.pre_pos).min(buf.len() - done);
                buf[done..done + n].copy_from_slice(&self.prefetch[self.pre_pos..self.pre_pos + n]);
                self.pre_pos += n;
                done += n;
                continue;
            }
            let Some(s) = self.stream.as_mut() else {
                ok = false;
                break;
            };
            let chunk = if self.opts.net_read_buffer_size == 0 {
                8192
            } else {
                self.opts.net_read_buffer_size
            };
            // Reads of a chunk or more go straight into the caller's buffer:
            // php-src grows its read buffer one chunk at a time
            // (`_php_stream_fill_read_buffer`), so a 16 MiB packet does not
            // leave a second 16 MiB buffer on the connection.
            if buf.len() - done >= chunk {
                match s.read(&mut buf[done..]) {
                    Ok(0) => {
                        ok = false;
                        break;
                    }
                    Ok(n) => done += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
                continue;
            }
            self.prefetch.resize(chunk, 0);
            self.pre_pos = 0;
            match s.read(&mut self.prefetch) {
                Ok(0) => {
                    self.prefetch.clear();
                    ok = false;
                    break;
                }
                Ok(n) => self.prefetch.truncate(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => self.prefetch.clear(),
                Err(_) => {
                    self.prefetch.clear();
                    ok = false;
                    break;
                }
            }
        }
        inc_conn(stats, Stat::BytesReceived, done as u64);
        ok
    }

    /// `mysqlnd_vio::network_write`
    fn network_write(&mut self, buf: &[u8]) -> bool {
        let Some(s) = self.stream.as_mut() else {
            return false;
        };
        s.write_all(buf).and_then(|()| s.flush()).is_ok()
    }

    /// Reads and drops `len` payload bytes through a small buffer (the rest
    /// of a row past `memory_limit`), so the connection stays in step.
    pub fn skip(&mut self, mut len: usize, stats: &mut Stats) -> bool {
        let mut buf = [0u8; 8192];
        while len > 0 {
            let n = len.min(buf.len());
            if !self.receive(&mut buf[..n], stats) {
                return false;
            }
            len -= n;
        }
        true
    }

    /// `mysqlnd_pfc::receive`: reads `buf.len()` payload bytes, transparently
    /// decompressing when the compressed protocol is active.
    pub fn receive(&mut self, buf: &mut [u8], stats: &mut Stats) -> bool {
        if !self.compressed {
            return self.network_read(buf, stats);
        }
        let mut done = 0;
        while done < buf.len() {
            if self.unc_pos < self.unc.len() {
                let n = (self.unc.len() - self.unc_pos).min(buf.len() - done);
                buf[done..done + n].copy_from_slice(&self.unc[self.unc_pos..self.unc_pos + n]);
                self.unc_pos += n;
                done += n;
                continue;
            }
            let mut hdr = [0u8; HEADER_SIZE];
            if !self.network_read(&mut hdr, stats) {
                return false;
            }
            let payload = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], 0]) as usize;
            let no = hdr[3];
            if no != self.compressed_envelope_packet_no {
                diag::warn_plain(format!(
                    "Packets out of order. Expected {} received {no}. Packet size={payload}",
                    self.compressed_envelope_packet_no
                ));
                return false;
            }
            self.compressed_envelope_packet_no = self.compressed_envelope_packet_no.wrapping_add(1);
            let mut ch = [0u8; COMPRESSED_HEADER_SIZE];
            if !self.network_read(&mut ch, stats) {
                return false;
            }
            let decompressed = u32::from_le_bytes([ch[0], ch[1], ch[2], 0]) as usize;
            let mut data = vec![0u8; payload];
            if !self.network_read(&mut data, stats) {
                return false;
            }
            self.unc = if decompressed == 0 {
                data
            } else {
                match inflate::uncompress(&data, decompressed) {
                    Ok(d) => d,
                    Err(_) => return false,
                }
            };
            self.unc_pos = 0;
        }
        true
    }

    /// `mysqlnd_pfc::send`: frames `payload` into packets (splitting at
    /// 16 MiB - 1, with a trailing empty packet when needed). Returns the
    /// number of bytes written (0 on failure, with `CR_SERVER_GONE_ERROR` set).
    pub fn send(&mut self, payload: &[u8], stats: &mut Stats, err: &mut ErrorInfo) -> usize {
        let mut left = payload.len();
        let mut off = 0;
        let mut packets_sent: u64 = 1;
        let mut ok;
        let mut sent = 0usize;
        loop {
            let to_be_sent = left.min(MAX_PACKET_SIZE);
            #[allow(clippy::cast_possible_truncation)]
            let len_bytes = (to_be_sent as u32).to_le_bytes();
            let mut frame = Vec::with_capacity(HEADER_SIZE + to_be_sent);
            frame.extend_from_slice(&len_bytes[..3]);
            frame.push(self.packet_no);
            frame.extend_from_slice(&payload[off..off + to_be_sent]);
            if self.compressed {
                ok = self.write_compressed(&frame);
            } else {
                ok = self.network_write(&frame);
                self.compressed_envelope_packet_no =
                    self.compressed_envelope_packet_no.wrapping_add(1);
            }
            if ok {
                sent += frame.len();
            }
            self.packet_no = self.packet_no.wrapping_add(1);
            off += to_be_sent;
            left -= to_be_sent;
            packets_sent += 1;
            if !(ok && (left > 0 || to_be_sent == MAX_PACKET_SIZE)) {
                break;
            }
        }
        let hdr = packets_sent * HEADER_SIZE as u64;
        inc_conn(stats, Stat::BytesSent, payload.len() as u64 + hdr);
        inc_conn(stats, Stat::ProtocolOverheadOut, hdr);
        inc_conn(stats, Stat::PacketsSent, packets_sent);
        if ok {
            sent.max(1)
        } else {
            err.set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            0
        }
    }

    /// `write_compressed_packet`: wraps an already framed packet into a
    /// compressed envelope. The payload is deflated when that fits in its
    /// own size (`mysqlnd_pfc::encode` into a `to_be_sent`-byte buffer);
    /// otherwise it goes verbatim with an uncompressed length of 0.
    fn write_compressed(&mut self, frame: &[u8]) -> bool {
        let mut chunks: Vec<&[u8]> = Vec::new();
        if frame.len() > MAX_PACKET_SIZE {
            let split = 8192;
            chunks.push(&frame[..split]);
            chunks.push(&frame[split..]);
        } else {
            chunks.push(frame);
        }
        for c in chunks {
            let z = inflate::compress(c);
            let (payload, unc_len) = if z.len() <= c.len() {
                (z.as_slice(), c.len())
            } else {
                (c, 0)
            };
            #[allow(clippy::cast_possible_truncation)]
            let len = (payload.len() as u32).to_le_bytes();
            #[allow(clippy::cast_possible_truncation)]
            let unc = (unc_len as u32).to_le_bytes();
            let mut out = Vec::with_capacity(payload.len() + 7);
            out.extend_from_slice(&len[..3]);
            out.push(self.compressed_envelope_packet_no);
            out.extend_from_slice(&unc[..3]);
            out.extend_from_slice(payload);
            if !self.network_write(&out) {
                return false;
            }
            self.compressed_envelope_packet_no = self.compressed_envelope_packet_no.wrapping_add(1);
        }
        true
    }

    /// `mysqlnd_read_header`: reads a packet header and checks its sequence
    /// number. Returns the payload size.
    pub fn read_header(&mut self, stats: &mut Stats, err: &mut ErrorInfo) -> Option<usize> {
        let mut h = [0u8; HEADER_SIZE];
        if !self.receive(&mut h, stats) {
            return None;
        }
        let size = u32::from_le_bytes([h[0], h[1], h[2], 0]) as usize;
        let no = h[3];
        inc_conn(stats, Stat::ProtocolOverheadIn, HEADER_SIZE as u64);
        inc_conn(stats, Stat::PacketsReceived, 1);
        if self.compressed || self.packet_no == no {
            self.packet_no = self.packet_no.wrapping_add(1);
            return Some(size);
        }
        if size > 0 {
            let mut body = vec![0u8; size];
            if self.receive(&mut body, stats) && body[0] == 0xFF {
                ServerError::parse(&body[1..]).apply(err);
                return None;
            }
        }
        diag::warn_plain(format!(
            "Packets out of order. Expected {} received {no}. Packet size={size}",
            self.packet_no
        ));
        None
    }

    /// Sends a command packet (`php_mysqlnd_cmd_write`): resets the
    /// sequence, counts `packets_sent_command`, frames `cmd + arg`.
    pub fn send_command(
        &mut self,
        cmd: Command,
        arg: &[u8],
        stats: &mut Stats,
        err: &mut ErrorInfo,
    ) -> usize {
        self.reset();
        inc_conn(stats, Stat::PacketsSentCmd, 1);
        let mut payload = Vec::with_capacity(arg.len() + 1);
        payload.push(cmd as u8);
        payload.extend_from_slice(arg);
        if HEADER_SIZE + payload.len() > self.cmd_buffer_size {
            inc_conn(stats, Stat::CmdBufferTooSmall, 1);
        }
        self.send(&payload, stats, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_parsing() {
        assert_eq!(
            parse_host_port("127.0.0.1:3306"),
            Some(("127.0.0.1".into(), 3306))
        );
        assert_eq!(parse_host_port("[::1]:3307"), Some(("::1".into(), 3307)));
        assert_eq!(parse_host_port("host:abc"), Some(("host".into(), 0)));
        assert_eq!(parse_host_port("nohost"), None);
    }

    #[test]
    fn strerror_strips_os_suffix() {
        let e = io::Error::from_raw_os_error(111);
        assert_eq!(strerror(&e), "Connection refused");
        let e = io::Error::from_raw_os_error(2);
        assert_eq!(strerror(&e), "No such file or directory");
    }

    #[test]
    fn refused_connection_sets_error() {
        let mut n = Net::default();
        let mut err = ErrorInfo::default();
        // port 1 on localhost is essentially never open
        assert!(!n.connect("tcp://127.0.0.1:1", Duration::from_secs(2), &mut err));
        assert_eq!(err.errno, CR_CONNECTION_ERROR);
        assert_eq!(err.error, b"Connection refused");
        assert!(!n.connect(
            "unix:///nonexistent/mysql.sock",
            Duration::from_secs(2),
            &mut err
        ));
        assert_eq!(err.error, b"No such file or directory");
        assert!(!n.connect("pipe://x", Duration::from_secs(2), &mut err));
        assert_eq!(err.error, b"No handler for this scheme");
    }
}
