// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Decoders and encoders for the individual protocol packets
//! (ref: `mysqlnd_wireprotocol.c`). Decoders work on a complete payload and
//! report malformed input the way mysqlnd does (a queued PHP warning plus a
//! failure), so they can be unit-tested from captured packets.

use crate::consts::{
    CLIENT_PLUGIN_AUTH, CLIENT_SECURE_CONNECTION, MAX_ALLOWED_DB_LEN, MAX_ALLOWED_USER_LEN,
    MYSQL_TYPE_INT24, MYSQL_TYPE_TIMESTAMP, MYSQL_TYPE_YEAR, NUM_FLAG, SCRAMBLE_LENGTH,
    SCRAMBLE_LENGTH_323,
};
use crate::diag;
use crate::error::ServerError;
use crate::wire::{put_lenenc, FieldLen, Reader};

/// Marker byte of an error packet.
pub const ERROR_MARKER: u8 = 0xFF;
/// Marker byte of an EOF packet.
pub const EODATA_MARKER: u8 = 0xFE;

const MARIADB_RPL_VERSION_HACK: &[u8] = b"5.5.5-";

fn premature(line: u32) {
    diag::warn(format!(
        "Premature end of data (mysqlnd_wireprotocol.c:{line})"
    ));
}

/// A decoded server greeting (protocol v10 handshake).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Greeting {
    /// Protocol version (10), or 0xFF when the server sent an error.
    pub protocol_version: u8,
    /// Server version string (`MariaDB`'s `5.5.5-` prefix removed).
    pub server_version: String,
    /// Connection id.
    pub thread_id: u32,
    /// Auth plugin data (scramble), normally 20 bytes.
    pub auth_plugin_data: Vec<u8>,
    /// Capability flags (lower and upper halves combined).
    pub server_capabilities: u32,
    /// Server default collation.
    pub charset_no: u8,
    /// Server status.
    pub server_status: u16,
    /// Auth plugin name, if `CLIENT_PLUGIN_AUTH`.
    pub auth_protocol: Option<String>,
    /// Pre-4.1 server (unsupported).
    pub pre41: bool,
    /// The server refused the connection with an error.
    pub error: Option<ServerError>,
}

/// `php_mysqlnd_greet_read`
pub fn parse_greeting(buf: &[u8]) -> Option<Greeting> {
    let r = parse_greeting_inner(buf);
    if r.is_none() {
        diag::warn("GREET packet shorter than expected");
    }
    r
}

fn parse_greeting_inner(buf: &[u8]) -> Option<Greeting> {
    let mut g = Greeting::default();
    let size = buf.len();
    if size < 1 {
        premature(400);
        return None;
    }
    let mut p = 0usize;
    g.protocol_version = buf[0];
    p += 1;
    if p >= size {
        premature(407);
        return None;
    }
    if g.protocol_version == ERROR_MARKER {
        let mut e = ServerError::parse(&buf[1..]);
        if e.errno == 1040 {
            e.sqlstate = "08004".to_string();
        }
        g.error = Some(e);
        return Some(g);
    }
    if size - p >= MARIADB_RPL_VERSION_HACK.len() && buf[p..].starts_with(MARIADB_RPL_VERSION_HACK)
    {
        p += MARIADB_RPL_VERSION_HACK.len();
    }
    let Some(nul) = buf[p..].iter().position(|&b| b == 0) else {
        premature(434);
        return None;
    };
    g.server_version = String::from_utf8_lossy(&buf[p..p + nul]).into_owned();
    p += nul + 1;
    if p + 4 + SCRAMBLE_LENGTH_323 + 1 + 2 + 1 + 2 + 13 > size {
        premature(439);
        return None;
    }
    g.thread_id = u32::from_le_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
    p += 4;
    let mut scramble = buf[p..p + SCRAMBLE_LENGTH_323].to_vec();
    p += SCRAMBLE_LENGTH_323;
    p += 1;
    g.server_capabilities = u32::from(u16::from_le_bytes([buf[p], buf[p + 1]]));
    p += 2;
    g.charset_no = buf[p];
    p += 1;
    g.server_status = u16::from_le_bytes([buf[p], buf[p + 1]]);
    p += 2;
    let pad_start = p;
    p += 13;
    let mut scramble_len = SCRAMBLE_LENGTH;
    if p < size {
        let end = (p + SCRAMBLE_LENGTH - SCRAMBLE_LENGTH_323).min(size);
        scramble.extend_from_slice(&buf[p..end]);
        scramble.resize(SCRAMBLE_LENGTH, 0);
        p += SCRAMBLE_LENGTH - SCRAMBLE_LENGTH_323;
        p += 1;
    } else {
        g.pre41 = true;
    }
    if p < size {
        p -= 1;
        g.server_capabilities |=
            u32::from(u16::from_le_bytes([buf[pad_start], buf[pad_start + 1]])) << 16;
        scramble_len = usize::from(buf[pad_start + 2]);
        if scramble_len > SCRAMBLE_LENGTH {
            let extra = scramble_len - SCRAMBLE_LENGTH;
            if p + extra > size {
                premature(485);
                return None;
            }
            scramble.extend_from_slice(&buf[p..p + extra]);
            p += extra;
        }
    }
    // A scramble length below 20 keeps the 20-byte buffer but reports the
    // shorter length (`authentication_plugin_data.l`).
    if scramble_len < scramble.len() {
        scramble.truncate(scramble_len);
    }
    g.auth_plugin_data = scramble;
    if g.server_capabilities & CLIENT_PLUGIN_AUTH != 0 {
        if p >= size {
            premature(502);
            return None;
        }
        let rest = &buf[p..];
        let name = match rest.iter().position(|&b| b == 0) {
            Some(n) => &rest[..n],
            None => rest,
        };
        g.auth_protocol = Some(String::from_utf8_lossy(name).into_owned());
    }
    Some(g)
}

/// Parameters of the client handshake response / `COM_CHANGE_USER` payload.
#[derive(Debug, Clone, Default)]
pub struct AuthPacket<'a> {
    /// Building a `COM_CHANGE_USER` payload instead of a handshake response.
    pub is_change_user: bool,
    /// Client capability flags.
    pub client_flags: u32,
    /// `max_packet_size`
    pub max_packet_size: u32,
    /// Client collation.
    pub charset_no: u32,
    /// Include user/auth data (false for the SSL request packet).
    pub send_auth_data: bool,
    /// User name.
    pub user: &'a [u8],
    /// Scrambled auth data.
    pub auth_data: &'a [u8],
    /// Database (empty = none).
    pub db: &'a [u8],
    /// Plugin name.
    pub auth_plugin_name: Option<&'a str>,
    /// Connection attributes (sent when non-empty).
    pub connect_attrs: &'a [(String, String)],
}

/// Error from [`encode_auth_packet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPacketError {
    /// Auth data longer than 255 bytes.
    AuthDataTooLong,
}

/// `php_mysqlnd_auth_write` (payload only, without the 4-byte header).
pub fn encode_auth_packet(a: &AuthPacket<'_>) -> Result<Vec<u8>, AuthPacketError> {
    // AUTH_WRITE_BUFFER_LEN in the C code: header + user + scramble + db + 1 + 4096
    const BUFFER_LEN: usize =
        4 + MAX_ALLOWED_USER_LEN + SCRAMBLE_LENGTH + MAX_ALLOWED_DB_LEN + 1 + 4096;
    let mut out = Vec::with_capacity(128);
    if !a.is_change_user {
        out.extend_from_slice(&a.client_flags.to_le_bytes());
        out.extend_from_slice(&a.max_packet_size.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        out.push(a.charset_no as u8);
        out.extend_from_slice(&[0u8; 23]);
    }
    if a.send_auth_data || a.is_change_user {
        let user = &a.user[..a.user.len().min(MAX_ALLOWED_USER_LEN)];
        let user = match user.iter().position(|&b| b == 0) {
            Some(n) => &user[..n],
            None => user,
        };
        out.extend_from_slice(user);
        out.push(0);
        if a.auth_data.len() > 0xFF {
            let msg = "Authentication data too long. Won't fit into the buffer and will be truncated. Authentication will thus fail";
            diag::warn(msg);
            return Err(AuthPacketError::AuthDataTooLong);
        }
        #[allow(clippy::cast_possible_truncation)]
        out.push(a.auth_data.len() as u8);
        out.extend_from_slice(a.auth_data);
        if !a.db.is_empty() {
            out.extend_from_slice(&a.db[..a.db.len().min(MAX_ALLOWED_DB_LEN)]);
            out.push(0);
        } else if a.is_change_user {
            out.push(0);
        }
        if a.is_change_user && a.charset_no != 0 {
            #[allow(clippy::cast_possible_truncation)]
            out.extend_from_slice(&(a.charset_no as u16).to_le_bytes());
        }
        if let Some(p) = a.auth_plugin_name {
            out.extend_from_slice(p.as_bytes());
            out.push(0);
        }
        if !a.connect_attrs.is_empty() {
            let mut ca = Vec::new();
            for (k, v) in a.connect_attrs {
                put_lenenc(&mut ca, k.len() as u64);
                ca.extend_from_slice(k.as_bytes());
                put_lenenc(&mut ca, v.len() as u64);
                ca.extend_from_slice(v.as_bytes());
            }
            if BUFFER_LEN >= ca.len() + crate::wire::lenenc_size(ca.len() as u64) + out.len() + 4 {
                put_lenenc(&mut out, ca.len() as u64);
                out.extend_from_slice(&ca);
            }
        }
    }
    Ok(out)
}

/// An OK packet (`MYSQLND_PACKET_OK`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OkPacket {
    /// Affected rows.
    pub affected_rows: u64,
    /// Last insert id.
    pub last_insert_id: u64,
    /// Server status.
    pub server_status: u16,
    /// Warning count.
    pub warning_count: u16,
    /// Info message.
    pub message: Vec<u8>,
    /// Raw session state changes (only with `CLIENT_SESSION_TRACK`).
    pub session_track: Vec<(u8, Vec<u8>)>,
}

/// Outcome of reading a generic response packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    /// 0x00 OK.
    Ok(OkPacket),
    /// 0xFF error.
    Err(ServerError),
    /// 0xFE auth switch request: plugin name and data.
    AuthSwitch(String, Vec<u8>),
    /// 0xFE with no plugin name (old-password request).
    OldAuthSwitch,
}

/// Why an OK body failed to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OkFail {
    /// `BAIL_PREMATURE_END` (the caller adds its "... shorter than expected").
    Premature,
    /// The auth response's message length points past the packet (its own warning).
    MessagePastEnd,
}

/// Reads a length-encoded 64-bit value (`php_mysqlnd_net_field_length_ll`):
/// NULL (0xFB) yields `MYSQLND_NULL_LENGTH`.
fn lenenc_ll(r: &mut Reader<'_>) -> Option<u64> {
    match r.field_len_ll() {
        FieldLen::Len(v) => Some(v),
        FieldLen::Null => Some(u64::MAX),
        FieldLen::Invalid => None,
    }
}

#[allow(clippy::many_single_char_names)]
fn parse_ok_body(
    r: &mut Reader<'_>,
    session_track: bool,
    lines: (u32, u32, u32),
    auth: bool,
) -> Result<OkPacket, OkFail> {
    let mut ok = OkPacket::default();
    let Some(a) = lenenc_ll(r) else {
        premature(lines.0);
        return Err(OkFail::Premature);
    };
    ok.affected_rows = a;
    let Some(i) = lenenc_ll(r) else {
        premature(lines.0);
        return Err(OkFail::Premature);
    };
    ok.last_insert_id = i;
    if r.remaining() < 4 {
        premature(lines.1);
        return Err(OkFail::Premature);
    }
    ok.server_status = r.u16().unwrap_or(0);
    ok.warning_count = r.u16().unwrap_or(0);
    if r.remaining() > 0 {
        match r.field_len() {
            FieldLen::Len(0) => {}
            FieldLen::Invalid if !auth => {
                premature(lines.2);
                return Err(OkFail::Premature);
            }
            other => {
                let n = match other {
                    FieldLen::Len(n) => usize::try_from(n).unwrap_or(usize::MAX),
                    _ => usize::MAX,
                };
                if auth && n > r.remaining() {
                    diag::warn("OK packet message length is past the packet size");
                    return Err(OkFail::MessagePastEnd);
                }
                let n = n.min(r.remaining());
                ok.message = r.take(n).unwrap_or_default().to_vec();
            }
        }
        if session_track && ok.server_status & crate::consts::SERVER_SESSION_STATE_CHANGED != 0 {
            if let FieldLen::Len(total) = r.field_len() {
                let total = usize::try_from(total).unwrap_or(0).min(r.remaining());
                let mut t = Reader::new(r.take(total).unwrap_or_default());
                while t.remaining() > 0 {
                    let Some(kind) = t.u8() else { break };
                    let FieldLen::Len(l) = t.field_len() else {
                        break;
                    };
                    let Some(data) = t.take(usize::try_from(l).unwrap_or(usize::MAX)) else {
                        break;
                    };
                    ok.session_track.push((kind, data.to_vec()));
                }
            }
        }
    }
    Ok(ok)
}

/// `php_mysqlnd_ok_read`
pub fn parse_ok(buf: &[u8], session_track: bool) -> Option<Response> {
    let r = parse_ok_inner(buf, session_track);
    if r.is_none() {
        diag::warn("OK packet shorter than expected");
    }
    r
}

fn parse_ok_inner(buf: &[u8], session_track: bool) -> Option<Response> {
    if buf.is_empty() {
        premature(910);
        return None;
    }
    if buf.len() < 2 {
        premature(915);
        return None;
    }
    if buf[0] == ERROR_MARKER {
        return Some(Response::Err(ServerError::parse(&buf[1..])));
    }
    let mut r = Reader::new(&buf[1..]);
    parse_ok_body(&mut r, session_track, (929, 939, 950), false)
        .ok()
        .map(Response::Ok)
}

/// `php_mysqlnd_auth_response_read`. Returns the response code byte (set
/// even when decoding fails, as mysqlnd's `packet->response_code` is) and
/// the decoded response (`None` = malformed, warnings raised).
pub fn parse_auth_response(buf: &[u8], session_track: bool) -> (u8, Option<Response>) {
    let code = buf.first().copied().unwrap_or(0);
    let r = parse_auth_response_inner(buf, session_track);
    match r {
        Ok(r) => (code, Some(r)),
        Err(OkFail::Premature) => {
            diag::warn("AUTH_RESPONSE packet shorter than expected");
            (code, None)
        }
        Err(OkFail::MessagePastEnd) => (code, None),
    }
}

fn parse_auth_switch(rest: &[u8], line: u32) -> Option<Response> {
    if rest.is_empty() {
        return Some(Response::OldAuthSwitch);
    }
    let Some(nul) = rest.iter().position(|&b| b == 0) else {
        premature(line);
        return None;
    };
    Some(Response::AuthSwitch(
        String::from_utf8_lossy(&rest[..nul]).into_owned(),
        rest[nul + 1..].to_vec(),
    ))
}

fn parse_auth_response_inner(buf: &[u8], session_track: bool) -> Result<Response, OkFail> {
    if buf.is_empty() {
        premature(736);
        return Err(OkFail::Premature);
    }
    if buf.len() < 2 {
        premature(741);
        return Err(OkFail::Premature);
    }
    match buf[0] {
        ERROR_MARKER => Ok(Response::Err(ServerError::parse(&buf[1..]))),
        0xFE => parse_auth_switch(&buf[1..], 756).ok_or(OkFail::Premature),
        _ => {
            let mut r = Reader::new(&buf[1..]);
            parse_ok_body(&mut r, session_track, (777, 787, 787), true).map(Response::Ok)
        }
    }
}

/// Decoded `COM_CHANGE_USER` reply (`php_mysqlnd_chg_user_read`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChgUserResponse {
    /// `packet->response_code` (0 when the packet was empty).
    pub code: u8,
    /// Whether the read succeeded (mysqlnd's `PACKET_READ` result).
    pub ok: bool,
    /// The server asked for pre-4.1 (3.23) authentication.
    pub asked_323: bool,
    /// Error from an error packet.
    pub error: Option<ServerError>,
    /// Auth switch request (plugin, data).
    pub switch: Option<(String, Vec<u8>)>,
}

/// `php_mysqlnd_chg_user_read`
pub fn parse_change_user_response(buf: &[u8], server_caps: u32) -> ChgUserResponse {
    let mut out = ChgUserResponse {
        code: 0,
        ok: false,
        asked_323: false,
        error: None,
        switch: None,
    };
    let fail = |out: ChgUserResponse, line: u32| {
        premature(line);
        diag::warn("CHANGE_USER packet shorter than expected");
        out
    };
    if buf.is_empty() {
        return fail(out, 2004);
    }
    out.code = buf[0];
    if buf.len() == 1 && buf[0] == EODATA_MARKER && server_caps & CLIENT_SECURE_CONNECTION != 0 {
        out.asked_323 = true;
        return out;
    }
    if buf[0] == ERROR_MARKER {
        out.error = Some(ServerError::parse(&buf[1..]));
    }
    if buf.len() < 2 {
        return fail(out, 2029);
    }
    if buf[0] == 0xFE {
        match parse_auth_switch(&buf[1..], 2034) {
            Some(Response::AuthSwitch(n, d)) => out.switch = Some((n, d)),
            Some(_) => {}
            None => {
                diag::warn("CHANGE_USER packet shorter than expected");
                return out;
            }
        }
    }
    out.ok = true;
    out
}

/// The reply to a `caching_sha2_password` exchange (`php_mysqlnd_cached_sha2_result_read`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sha2Result {
    /// Error packet.
    Err(ServerError),
    /// Auth switch request.
    Switch(String, Vec<u8>),
    /// `0x01 <code>`: 3 = fast auth success, 4 = full auth required.
    Code(u8),
}

/// `php_mysqlnd_cached_sha2_result_read`
pub fn parse_sha2_result(buf: &[u8]) -> Option<Sha2Result> {
    let fail = || {
        diag::warn("SHA256_PK_REQUEST_RESPONSE packet shorter than expected");
        None
    };
    if buf.len() < 2 {
        premature(if buf.is_empty() { 2199 } else { 2203 });
        return fail();
    }
    match buf[0] {
        ERROR_MARKER => Some(Sha2Result::Err(ServerError::parse(&buf[1..]))),
        0xFE => match parse_auth_switch(&buf[1..], 2218) {
            Some(Response::AuthSwitch(n, d)) => Some(Sha2Result::Switch(n, d)),
            Some(_) => Some(Sha2Result::Switch(String::new(), Vec::new())),
            None => fail(),
        },
        _ => Some(Sha2Result::Code(buf[1])),
    }
}

/// An EOF packet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EofPacket {
    /// First byte (0xFE expected).
    pub field_count: u8,
    /// Warning count.
    pub warning_count: u16,
    /// Server status.
    pub server_status: u16,
}

/// `php_mysqlnd_eof_read`: `Err` carries a server error packet.
pub fn parse_eof(buf: &[u8]) -> Option<Result<EofPacket, ServerError>> {
    let fail = |line| {
        premature(line);
        diag::warn("EOF packet shorter than expected");
        None
    };
    if buf.is_empty() {
        return fail(1011);
    }
    if buf.len() < 2 && buf[0] == ERROR_MARKER {
        return fail(1016);
    }
    if buf[0] == ERROR_MARKER {
        return Some(Err(ServerError::parse(&buf[1..])));
    }
    let mut e = EofPacket {
        field_count: buf[0],
        ..EofPacket::default()
    };
    if buf.len() > 1 {
        if buf.len() < 5 {
            return fail(1032);
        }
        e.warning_count = u16::from_le_bytes([buf[1], buf[2]]);
        e.server_status = u16::from_le_bytes([buf[3], buf[4]]);
    }
    Some(Ok(e))
}

/// The first response to `COM_QUERY` / `COM_STMT_EXECUTE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RsetHeader {
    /// Server error.
    Err(ServerError),
    /// `LOAD DATA LOCAL INFILE` request with the file name.
    LocalInfile(Vec<u8>),
    /// Non-result-set statement (affected rows etc.).
    Upsert(OkPacket),
    /// A result set with this many columns follows.
    ResultSet(u64),
}

/// `php_mysqlnd_rset_header_read`
#[allow(clippy::many_single_char_names)]
pub fn parse_rset_header(buf: &[u8], session_track: bool) -> Option<RsetHeader> {
    let fail = |line: Option<u32>| {
        if let Some(l) = line {
            premature(l);
        }
        diag::warn("RSET_HEADER packet shorter than expected");
        None
    };
    if buf.is_empty() {
        return fail(Some(1138));
    }
    if buf[0] == ERROR_MARKER {
        if buf.len() < 2 {
            return fail(Some(1147));
        }
        return Some(RsetHeader::Err(ServerError::parse(&buf[1..])));
    }
    let mut r = Reader::new(buf);
    match r.field_len() {
        FieldLen::Invalid => fail(Some(1157)),
        FieldLen::Null => Some(RsetHeader::LocalInfile(buf[1..].to_vec())),
        FieldLen::Len(0) => {
            let mut ok = OkPacket::default();
            let Some(a) = lenenc_ll(&mut r) else {
                return fail(Some(1182));
            };
            let Some(i) = lenenc_ll(&mut r) else {
                return fail(Some(1188));
            };
            ok.affected_rows = a;
            ok.last_insert_id = i;
            if r.remaining() < 4 {
                return fail(Some(1192));
            }
            ok.server_status = r.u16().unwrap_or(0);
            ok.warning_count = r.u16().unwrap_or(0);
            if let FieldLen::Len(n) = r.field_len() {
                if n > 0 {
                    let n = usize::try_from(n).unwrap_or(usize::MAX);
                    if r.remaining() < n {
                        diag::warn(format!(
                            "RSET_HEADER packet additional data length is past {} bytes the packet size",
                            n - r.remaining()
                        ));
                        return None;
                    }
                    ok.message = r.take(n).unwrap_or_default().to_vec();
                }
            }
            if session_track && ok.server_status & crate::consts::SERVER_SESSION_STATE_CHANGED != 0
            {
                if let FieldLen::Len(total) = r.field_len() {
                    let total = usize::try_from(total).unwrap_or(0).min(r.remaining());
                    let mut t = Reader::new(r.take(total).unwrap_or_default());
                    while let Some(kind) = t.u8() {
                        let FieldLen::Len(l) = t.field_len() else {
                            break;
                        };
                        let Some(data) = t.take(usize::try_from(l).unwrap_or(usize::MAX)) else {
                            break;
                        };
                        ok.session_track.push((kind, data.to_vec()));
                    }
                }
            }
            Some(RsetHeader::Upsert(ok))
        }
        FieldLen::Len(n) => Some(RsetHeader::ResultSet(n)),
    }
}

/// Column metadata (`MYSQLND_FIELD`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Field {
    /// Column alias.
    pub name: Vec<u8>,
    /// Original column name.
    pub org_name: Vec<u8>,
    /// Table alias.
    pub table: Vec<u8>,
    /// Original table name.
    pub org_table: Vec<u8>,
    /// Database.
    pub db: Vec<u8>,
    /// Catalog (always "def").
    pub catalog: Vec<u8>,
    /// Default value (never sent by modern servers).
    pub def: Vec<u8>,
    /// Display length.
    pub length: u32,
    /// Maximum value length (mysqli always reports 0).
    pub max_length: u64,
    /// Collation number.
    pub charsetnr: u16,
    /// Field flags (with `NUM_FLAG` added like libmysql).
    pub flags: u32,
    /// `MYSQL_TYPE_*`
    pub type_: u8,
    /// Decimals.
    pub decimals: u8,
}

/// `php_mysqlnd_rset_field_read`. `Err` carries a server error packet.
pub fn parse_field(buf: &[u8]) -> Option<Result<Field, ServerError>> {
    if buf.is_empty() {
        premature(1296);
        diag::warn("Result set field packet shorter than expected");
        return None;
    }
    if buf[0] == ERROR_MARKER {
        if buf.len() < 2 {
            premature(1300);
            diag::warn("Result set field packet shorter than expected");
            return None;
        }
        return Some(Err(ServerError::parse(&buf[1..])));
    }
    if buf[0] == EODATA_MARKER && buf.len() < 8 {
        diag::warn("Premature EOF in result field metadata");
        return None;
    }
    let mut r = Reader::new(buf);
    let mut strs: [Vec<u8>; 6] = Default::default();
    for s in &mut strs {
        let len = match r.field_len() {
            FieldLen::Len(l) if usize::try_from(l).is_ok_and(|l| l <= r.remaining()) => l,
            _ => {
                diag::warn("Protocol error. Server sent NULL_LENGTH. The server is faulty");
                return None;
            }
        };
        *s = r
            .take(usize::try_from(len).unwrap_or(0))
            .unwrap_or_default()
            .to_vec();
    }
    let [catalog, db, table, org_table, name, org_name] = strs;
    if r.peek() != Some(12) {
        diag::warn("Protocol error. Server sent false length. Expected 12");
    }
    if r.pos() + 12 > buf.len() {
        diag::warn("Premature end of data (mysqlnd_wireprotocol.c:1330)");
        diag::warn("Result set field packet shorter than expected");
        return None;
    }
    r.skip(1);
    let mut f = Field {
        name,
        org_name,
        table,
        org_table,
        db,
        catalog,
        ..Field::default()
    };
    f.charsetnr = r.u16().unwrap_or(0);
    f.length = r.u32().unwrap_or(0);
    f.type_ = r.u8().unwrap_or(0);
    f.flags = u32::from(r.u16().unwrap_or(0));
    f.decimals = r.u8().unwrap_or(0);
    r.skip(2);
    if (f.type_ <= MYSQL_TYPE_INT24
        && (f.type_ != MYSQL_TYPE_TIMESTAMP || f.length == 14 || f.length == 8))
        || f.type_ == MYSQL_TYPE_YEAR
    {
        f.flags |= NUM_FLAG;
    }
    if let FieldLen::Len(_) | FieldLen::Null = r.field_len() {
        diag::warn("Protocol error. Server sent default for unsupported field list (mysqlnd_wireprotocol.c:1369)");
        return None;
    }
    Some(Ok(f))
}

/// A row packet as classified by `php_mysqlnd_rowp_read`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowPacket {
    /// A data row (raw payload).
    Row(Vec<u8>),
    /// End of rows.
    Eof(EofPacket),
    /// Error in the middle of the result set.
    Err(ServerError),
    /// A row that would pass `memory_limit`: read and dropped, the
    /// violation pending for the VM (php-src: `SET_OOM_ERROR` when the
    /// row buffer cannot grow).
    OverLimit,
}

/// Classifies a (fully assembled) row payload.
#[must_use]
pub fn classify_row(buf: Vec<u8>) -> RowPacket {
    match buf.first() {
        Some(&ERROR_MARKER) => RowPacket::Err(ServerError::parse(&buf[1..])),
        Some(&EODATA_MARKER) if buf.len() < 8 => {
            let mut e = EofPacket {
                field_count: EODATA_MARKER,
                ..EofPacket::default()
            };
            if buf.len() >= 5 {
                e.warning_count = u16::from_le_bytes([buf[1], buf[2]]);
                e.server_status = u16::from_le_bytes([buf[3], buf[4]]);
            }
            RowPacket::Eof(e)
        }
        _ => RowPacket::Row(buf),
    }
}

/// `COM_STMT_PREPARE` OK response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrepareResponse {
    /// Statement id.
    pub stmt_id: u32,
    /// Result columns.
    pub field_count: u16,
    /// Parameters.
    pub param_count: u16,
    /// Warnings.
    pub warning_count: u16,
}

/// `php_mysqlnd_prepare_read`
pub fn parse_prepare_response(buf: &[u8]) -> Option<Result<PrepareResponse, ServerError>> {
    let fail = |line: u32| {
        premature(line);
        diag::warn("PREPARE packet shorter than expected");
        None
    };
    if buf.is_empty() {
        return fail(1925);
    }
    if buf.len() < 2 {
        return fail(1930);
    }
    if buf[0] == ERROR_MARKER {
        return Some(Err(ServerError::parse(&buf[1..])));
    }
    let data_size = buf.len();
    if data_size != 9 && data_size < 12 {
        diag::warn_plain(format!(
            "Wrong COM_STMT_PREPARE response size. Received {data_size}"
        ));
        return None;
    }
    let mut r = Reader::new(&buf[1..]);
    let mut p = PrepareResponse {
        stmt_id: r.u32().unwrap_or(0),
        field_count: r.u16().unwrap_or(0),
        param_count: r.u16().unwrap_or(0),
        warning_count: 0,
    };
    if data_size > 9 {
        r.skip(1);
        p.warning_count = r.u16().unwrap_or(0);
    }
    Some(Ok(p))
}

#[cfg(test)]
#[allow(clippy::cast_possible_truncation, clippy::doc_markdown)]
mod tests {
    use super::*;
    use crate::consts::*;

    /// A MariaDB 10.11 greeting captured from the wire.
    fn mariadb_greeting() -> Vec<u8> {
        let mut g = vec![10u8];
        g.extend_from_slice(b"5.5.5-10.11.14-MariaDB-0ubuntu0.24.04.1\0");
        g.extend_from_slice(&42u32.to_le_bytes());
        g.extend_from_slice(b"abcdefgh");
        g.push(0);
        let caps: u32 = 0xF7FE | (0x81BF << 16);
        g.extend_from_slice(&(caps as u16).to_le_bytes());
        g.push(45);
        g.extend_from_slice(&2u16.to_le_bytes());
        g.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
        g.push(21);
        g.extend_from_slice(&[0u8; 10]);
        g.extend_from_slice(b"ijklmnopqrst\0");
        g.extend_from_slice(b"mysql_native_password\0");
        g
    }

    #[test]
    fn greeting_parses() {
        let g = parse_greeting(&mariadb_greeting()).unwrap();
        assert_eq!(g.server_version, "10.11.14-MariaDB-0ubuntu0.24.04.1");
        assert_eq!(g.thread_id, 42);
        // mysqlnd keeps the scramble's terminating NUL (length 21 reported by 5.5+ servers)
        assert_eq!(g.auth_plugin_data, b"abcdefghijklmnopqrst\0");
        assert_eq!(g.charset_no, 45);
        assert_eq!(g.server_status, 2);
        assert_eq!(g.auth_protocol.as_deref(), Some("mysql_native_password"));
        assert!(g.server_capabilities & CLIENT_PLUGIN_AUTH != 0);
        assert!(!g.pre41);
    }

    #[test]
    fn greeting_error() {
        let g = parse_greeting(b"\xff\x10\x04Too many connections").unwrap();
        let e = g.error.unwrap();
        assert_eq!(e.errno, 1040);
        assert_eq!(e.sqlstate, "08004");
    }

    #[test]
    fn greeting_truncated_warns() {
        let _ = diag::take();
        assert!(parse_greeting(b"\x0a5.5\0\x01\x02").is_none());
        let d = diag::take();
        assert_eq!(
            d[0].message,
            "Premature end of data (mysqlnd_wireprotocol.c:439)"
        );
        assert_eq!(d[1].message, "GREET packet shorter than expected");
    }

    #[test]
    fn auth_packet_layout() {
        let attrs = vec![("_client_name".to_string(), "mysqlnd".to_string())];
        let p = AuthPacket {
            client_flags: MYSQLND_CAPABILITIES | CLIENT_CONNECT_WITH_DB | CLIENT_CONNECT_ATTRS,
            max_packet_size: 0xC000_0000,
            charset_no: 45,
            send_auth_data: true,
            user: b"root",
            auth_data: &[1, 2, 3],
            db: b"test",
            auth_plugin_name: Some("mysql_native_password"),
            connect_attrs: &attrs,
            ..AuthPacket::default()
        };
        let b = encode_auth_packet(&p).unwrap();
        assert_eq!(&b[4..8], &0xC000_0000u32.to_le_bytes());
        assert_eq!(b[8], 45);
        assert_eq!(&b[32..37], b"root\0");
        assert_eq!(&b[37..41], &[3, 1, 2, 3]);
        assert_eq!(&b[41..46], b"test\0");
        assert!(b.ends_with(b"\x0c_client_name\x07mysqlnd"));
        let cu = AuthPacket {
            is_change_user: true,
            user: b"u",
            charset_no: 45,
            ..AuthPacket::default()
        };
        assert_eq!(encode_auth_packet(&cu).unwrap(), b"u\0\0\0\x2d\0".to_vec());
    }

    #[test]
    fn ok_and_error_packets() {
        let ok = parse_ok(b"\x00\x01\x05\x02\x00\x01\x00\x05hello", false).unwrap();
        let Response::Ok(ok) = ok else { panic!() };
        assert_eq!(
            (
                ok.affected_rows,
                ok.last_insert_id,
                ok.server_status,
                ok.warning_count
            ),
            (1, 5, 2, 1)
        );
        assert_eq!(ok.message, b"hello");
        let Response::Err(e) = parse_ok(b"\xff\x28\x04#42000You have an error", false).unwrap()
        else {
            panic!()
        };
        assert_eq!((e.errno, e.sqlstate.as_str()), (1064, "42000"));
    }

    #[test]
    fn session_track_parsing() {
        let mut p = vec![0u8, 0, 0];
        p.extend_from_slice(&(SERVER_SESSION_STATE_CHANGED | 2).to_le_bytes());
        p.extend_from_slice(&[0, 0, 0]); // warnings, empty info
        let state = b"\x00\x0a\x0aautocommit";
        let mut track = Vec::new();
        track.push(0u8); // SESSION_TRACK_SYSTEM_VARIABLES
        track.push(state.len() as u8);
        track.extend_from_slice(state);
        p.push(track.len() as u8);
        p.extend_from_slice(&track);
        let Response::Ok(ok) = parse_ok(&p, true).unwrap() else {
            panic!()
        };
        assert_eq!(ok.session_track, vec![(0u8, state.to_vec())]);
    }

    #[test]
    fn rset_headers() {
        assert_eq!(
            parse_rset_header(b"\x03", false),
            Some(RsetHeader::ResultSet(3))
        );
        assert_eq!(
            parse_rset_header(b"\xfb/tmp/x.csv", false),
            Some(RsetHeader::LocalInfile(b"/tmp/x.csv".to_vec()))
        );
        let Some(RsetHeader::Upsert(ok)) =
            parse_rset_header(b"\x00\x02\x00\x02\x00\x00\x00\x03abc", false)
        else {
            panic!()
        };
        assert_eq!(ok.affected_rows, 2);
        assert_eq!(ok.message, b"abc");
    }

    #[test]
    fn field_packet() {
        let mut b = Vec::new();
        for s in [&b"def"[..], b"test", b"t", b"t", b"id", b"id"] {
            crate::wire::put_lenenc_str(&mut b, s);
        }
        b.push(12);
        b.extend_from_slice(&63u16.to_le_bytes());
        b.extend_from_slice(&11u32.to_le_bytes());
        b.push(MYSQL_TYPE_LONG);
        b.extend_from_slice(&((NOT_NULL_FLAG | PRI_KEY_FLAG) as u16).to_le_bytes());
        b.push(0);
        b.extend_from_slice(&[0, 0]);
        let f = parse_field(&b).unwrap().unwrap();
        assert_eq!(f.name, b"id");
        assert_eq!(f.db, b"test");
        assert_eq!(f.length, 11);
        assert_eq!(f.flags, NOT_NULL_FLAG | PRI_KEY_FLAG | NUM_FLAG);
        // a trailing default value is a protocol error for mysqlnd
        b.push(0);
        let _ = diag::take();
        assert!(parse_field(&b).is_none());
        assert!(diag::take()[0]
            .message
            .starts_with("Protocol error. Server sent default"));
    }

    #[test]
    fn prepare_response() {
        let b = [0u8, 7, 0, 0, 0, 2, 0, 1, 0, 0, 3, 0];
        let p = parse_prepare_response(&b).unwrap().unwrap();
        assert_eq!(
            (p.stmt_id, p.field_count, p.param_count, p.warning_count),
            (7, 2, 1, 3)
        );
        let _ = diag::take();
        assert!(parse_prepare_response(&[0u8, 1, 0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        assert_eq!(
            diag::take()[0].message,
            "Wrong COM_STMT_PREPARE response size. Received 10"
        );
    }

    #[test]
    fn rows_and_eof() {
        assert_eq!(
            classify_row(vec![0xfe, 0, 0, 2, 0]),
            RowPacket::Eof(EofPacket {
                field_count: 0xfe,
                warning_count: 0,
                server_status: 2
            })
        );
        assert!(matches!(classify_row(vec![1, b'a']), RowPacket::Row(_)));
        assert!(matches!(classify_row(vec![0xff, 1, 0]), RowPacket::Err(_)));
        let e = parse_eof(&[0xfe, 1, 0, 8, 0]).unwrap().unwrap();
        assert_eq!((e.warning_count, e.server_status), (1, 8));
    }

    #[test]
    fn sha2_results() {
        assert_eq!(parse_sha2_result(&[1, 3]), Some(Sha2Result::Code(3)));
        assert_eq!(parse_sha2_result(&[1, 4]), Some(Sha2Result::Code(4)));
        assert_eq!(
            parse_sha2_result(b"\xfemysql_native_password\0abc"),
            Some(Sha2Result::Switch(
                "mysql_native_password".into(),
                b"abc".to_vec()
            ))
        );
    }

    #[test]
    fn change_user_old_auth() {
        assert!(parse_change_user_response(&[0xfe], CLIENT_SECURE_CONNECTION).asked_323);
        let r =
            parse_change_user_response(b"\xfecaching_sha2_password\0xyz", CLIENT_SECURE_CONNECTION);
        assert_eq!(
            r.switch,
            Some(("caching_sha2_password".to_string(), b"xyz".to_vec()))
        );
        let _ = diag::take();
        let r = parse_change_user_response(&[0], CLIENT_SECURE_CONNECTION);
        assert!(!r.ok);
        assert_eq!(diag::take().len(), 2);
        let r = parse_change_user_response(b"\xfemysql_native", CLIENT_SECURE_CONNECTION);
        assert!(!r.ok && r.code == 0xfe && r.switch.is_none());
    }
}
