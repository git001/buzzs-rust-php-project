// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The connection object (ref: `mysqlnd_connection.c`, `mysqlnd_commands.c`,
//! `mysqlnd_auth.c`, `mysqlnd_result.c` header reading, `mysqlnd_loaddata.c`).
//!
//! [`Connection`] is a cheap, clonable handle (`Rc<RefCell<..>>`): result
//! sets and statements keep their connection alive, exactly like mysqlnd's
//! reference-counted `MYSQLND_CONN_DATA`.

use std::cell::{RefCell, RefMut};
use std::rc::Rc;
use std::time::Duration;

use crate::auth::{self, Plugin, RsaError};
use crate::charset::{self, Charset, UTF8_MB3_DEFAULT_ID, UTF8_MB4_DEFAULT_ID};
use crate::consts::{
    is_known_type, Command, ASSEMBLED_PACKET_MAX_SIZE, CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS,
    CLIENT_COMPRESS, CLIENT_CONNECT_ATTRS, CLIENT_CONNECT_WITH_DB, CLIENT_LOCAL_FILES,
    CLIENT_SESSION_TRACK, CLIENT_SSL, CLIENT_SSL_DONT_VERIFY_SERVER_CERT,
    CLIENT_SSL_VERIFY_SERVER_CERT, CR_CANT_FIND_CHARSET, CR_CLIENT_INTERACTION_TIMEOUT,
    CR_COMMANDS_OUT_OF_SYNC, CR_CONNECTION_ERROR, CR_INVALID_BUFFER_USE,
    CR_LOAD_DATA_LOCAL_INFILE_REJECTED, CR_MALFORMED_PACKET, CR_NOT_IMPLEMENTED,
    CR_SERVER_GONE_ERROR, CR_SERVER_LOST, CR_UNKNOWN_ERROR, DEFAULT_AUTH_PROTOCOL, MAX_PACKET_SIZE,
    MSG_OLD_PASSWD, MSG_OUT_OF_SYNC, MSG_SERVER_GONE, MYSQLND_CAPABILITIES,
    NET_CMD_BUFFER_MIN_SIZE, OPTION_MULTI_STATEMENTS_OFF, SCRAMBLE_LENGTH,
    SERVER_MORE_RESULTS_EXISTS, SERVER_QUERY_NO_GOOD_INDEX_USED, SERVER_QUERY_NO_INDEX_USED,
    SERVER_QUERY_WAS_SLOW, SERVER_STATUS_NO_BACKSLASH_ESCAPES, TRANS_COR_AND_CHAIN,
    TRANS_COR_AND_NO_CHAIN, TRANS_COR_NO_RELEASE, TRANS_COR_RELEASE, TRANS_START_READ_ONLY,
    TRANS_START_READ_WRITE, TRANS_START_WITH_CONSISTENT_SNAPSHOT, UNKNOWN_SQLSTATE,
};
use crate::diag;
use crate::error::{Error, ErrorEntry, ErrorInfo, Result, ServerError};
use crate::infile::{self, FsInfile, LocalInfileHandler};
use crate::ini;
use crate::net::{Net, SslPeer, HEADER_SIZE};
use crate::protocol::{self, AuthPacket, Response, RowPacket, RsetHeader, Sha2Result};
use crate::result::{Metadata, ResultSet};
use crate::stats::{inc_conn, inc_conn_index, inc_global, Stat, Stats};
use crate::stmt::Statement;

/// `enum mysqlnd_connection_state`
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ConnState {
    /// Allocated, never connected.
    Alloced = 0,
    /// Idle.
    Ready = 1,
    /// A query was sent; its response hasn't been read.
    QuerySent = 2,
    /// Streaming a LOAD DATA LOCAL file.
    SendingLoadData = 3,
    /// Rows of a result set are pending on the wire.
    FetchingData = 4,
    /// Another result of a multi-result response is pending.
    NextResultPending = 5,
    /// Closed (or the server went away).
    QuitSent = 6,
}

/// The kind of the last statement (`enum mysqlnd_query_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueryType {
    /// A statement without a result set.
    #[default]
    Upsert,
    /// A statement with a result set.
    Select,
    /// LOAD DATA LOCAL INFILE.
    LoadLocal,
}

/// `MYSQLND_UPSERT_STATUS`
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpsertStatus {
    /// Warning count.
    pub warning_count: u32,
    /// Server status flags.
    pub server_status: u16,
    /// Affected rows (`u64::MAX` = "error", reported as -1 by mysqli).
    pub affected_rows: u64,
    /// Last insert id.
    pub last_insert_id: u64,
}

impl UpsertStatus {
    /// `UPSERT_STATUS_RESET`
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    /// `UPSERT_STATUS_SET_AFFECTED_ROWS_TO_ERROR`
    pub fn set_affected_rows_to_error(&mut self) {
        self.affected_rows = u64::MAX;
    }
}

/// How a connection is being closed (`enum mysqlnd_connection_close_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseType {
    /// `mysqli_close()`
    Explicit,
    /// Object destruction.
    Implicit,
    /// The server disconnected.
    Disconnected,
}

/// A client option value for [`Connection::set_option`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionValue<'a> {
    /// Integer options.
    Int(i64),
    /// String options.
    Str(&'a [u8]),
    /// NULL.
    Null,
}

impl OptionValue<'_> {
    fn as_uint(self) -> u32 {
        match self {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            OptionValue::Int(i) => i as u32,
            OptionValue::Str(s) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let v = crate::codec::atoll(s) as u32;
                v
            }
            OptionValue::Null => 0,
        }
    }
    fn as_opt_string(self) -> Option<String> {
        match self {
            OptionValue::Str(s) => Some(String::from_utf8_lossy(s).into_owned()),
            OptionValue::Int(i) => Some(i.to_string()),
            OptionValue::Null => None,
        }
    }
}

/// `MYSQLND_SESSION_OPTIONS`
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// `CLIENT_*` flags set through options (e.g. `CLIENT_LOCAL_FILES`).
    pub flags: u32,
    /// `MYSQL_INIT_COMMAND`s.
    pub init_commands: Vec<Vec<u8>>,
    /// `MYSQL_SET_CHARSET_NAME`
    pub charset_name: Option<String>,
    /// `MYSQL_OPT_PROTOCOL`
    pub protocol: u32,
    /// `max_allowed_packet`
    pub max_allowed_packet: u32,
    /// Auth plugin used by the last successful authentication.
    pub auth_protocol: Option<String>,
    /// `MYSQLI_OPT_INT_AND_FLOAT_NATIVE`
    pub int_and_float_native: bool,
    /// Connection attributes (insertion ordered, unique keys).
    pub connect_attr: Vec<(String, String)>,
    /// `MYSQL_OPT_LOAD_DATA_LOCAL_DIR`
    pub local_infile_directory: Option<String>,
    /// Negotiate `CLIENT_SESSION_TRACK` (not something mysqlnd does; opt-in).
    pub session_track: bool,
}

/// Which packet is being read (selects the misaligned mysqlnd statistic,
/// see `packet_type_to_statistic_*` in `mysqlnd_wireprotocol.c`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketKind {
    Greet,
    Ok,
    Eof,
    RsetHeader,
    Field,
    Stats,
    Prepare,
    ChgUser,
    Sha256PkResponse,
    CachedSha2Result,
}

impl PacketKind {
    fn stats(self) -> Option<(Stat, Stat)> {
        match self {
            PacketKind::Greet | PacketKind::Ok => None,
            PacketKind::Eof => Some((
                Stat::BytesReceivedRsetHeader,
                Stat::PacketsReceivedRsetHeader,
            )),
            PacketKind::RsetHeader => {
                Some((Stat::BytesReceivedRsetRow, Stat::PacketsReceivedRsetRow))
            }
            PacketKind::Field => Some((
                Stat::BytesReceivedPrepareResponse,
                Stat::PacketsReceivedPrepareResponse,
            )),
            PacketKind::Stats
            | PacketKind::Prepare
            | PacketKind::ChgUser
            | PacketKind::Sha256PkResponse
            | PacketKind::CachedSha2Result => Some((Stat::BytesSent, Stat::BytesSent)),
        }
    }
}

/// Outcome of reading the first response of a query/execute.
#[derive(Debug)]
pub(crate) enum Header {
    /// OK / LOAD DATA: no result set.
    NoResult,
    /// A result set with this metadata follows.
    ResultSet(Rc<Metadata>),
}

/// The connection state (`MYSQLND_CONN_DATA`).
#[derive(Debug)]
pub struct ConnData {
    pub(crate) net: Net,
    pub(crate) stats: Stats,
    pub(crate) error_info: ErrorInfo,
    pub(crate) state: ConnState,
    pub(crate) upsert: UpsertStatus,
    pub(crate) last_message: Option<Vec<u8>>,
    pub(crate) options: Options,
    pub(crate) scheme: Option<String>,
    pub(crate) host_info: Option<String>,
    pub(crate) server_version: Option<String>,
    pub(crate) protocol_version: u8,
    pub(crate) thread_id: u64,
    pub(crate) port: u32,
    pub(crate) server_capabilities: u32,
    pub(crate) client_flags: u32,
    pub(crate) charset: Option<&'static Charset>,
    pub(crate) greet_charset: Option<&'static Charset>,
    pub(crate) auth_plugin_data: Vec<u8>,
    pub(crate) field_count: u32,
    pub(crate) last_query_type: QueryType,
    pub(crate) current_result: Option<Rc<Metadata>>,
    pub(crate) persistent: bool,
    pub(crate) infile: Option<Box<dyn LocalInfileHandler>>,
    pub(crate) session_track: Vec<(u8, Vec<u8>)>,
    pub(crate) client_api_capabilities: u32,
}

/// A handle on a `MySQL` connection (`MYSQLND`).
#[derive(Debug, Clone)]
pub struct Connection(pub(crate) Rc<RefCell<ConnData>>);

impl Drop for ConnData {
    fn drop(&mut self) {
        // free_reference() -> send_close() when the last reference goes away
        self.send_close();
    }
}

impl ConnData {
    fn new(persistent: bool) -> Self {
        let s = ini::settings();
        let mut net = Net::default();
        net.cmd_buffer_size = s.net_cmd_buffer_size;
        net.opts.net_read_buffer_size = s.net_read_buffer_size;
        net.opts.timeout_read = u32::try_from(s.net_read_timeout).unwrap_or(0);
        let mut upsert = UpsertStatus::default();
        upsert.reset();
        Self {
            net,
            stats: Stats::default(),
            error_info: ErrorInfo::default(),
            state: ConnState::Alloced,
            upsert,
            last_message: None,
            options: Options::default(),
            scheme: None,
            host_info: None,
            server_version: None,
            protocol_version: 0,
            thread_id: 0,
            port: 0,
            server_capabilities: 0,
            client_flags: 0,
            charset: None,
            greet_charset: None,
            auth_plugin_data: Vec::new(),
            field_count: 0,
            last_query_type: QueryType::Upsert,
            current_result: None,
            persistent,
            infile: None,
            session_track: Vec::new(),
            client_api_capabilities: 0,
        }
    }

    pub(crate) fn inc(&mut self, s: Stat, v: u64) {
        inc_conn(&mut self.stats, s, v);
    }

    fn session_tracking(&self) -> bool {
        self.client_flags & CLIENT_SESSION_TRACK != 0
    }

    // ------------------------------------------------------------ packets

    /// `mysqlnd_read_packet_header_and_body`
    pub(crate) fn read_packet(&mut self, kind: PacketKind) -> Option<Vec<u8>> {
        let Some(size) = self.net.read_header(&mut self.stats, &mut self.error_info) else {
            self.state = ConnState::QuitSent;
            if self.error_info.errno == 0 {
                self.error_info
                    .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            }
            return None;
        };
        if size > self.net.cmd_buffer_size.max(NET_CMD_BUFFER_MIN_SIZE) && kind != PacketKind::Greet
        {
            self.error_info.set_client(
                CR_INVALID_BUFFER_USE,
                "Packet buffer wasn't big enough; as a workaround consider increasing value of net_cmd_buffer_size",
            );
            return None;
        }
        let mut buf = vec![0u8; size];
        if !self.net.receive(&mut buf, &mut self.stats) {
            self.state = ConnState::QuitSent;
            self.error_info
                .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            return None;
        }
        if let Some((b, p)) = kind.stats() {
            self.inc(b, (HEADER_SIZE + size) as u64);
            self.inc(p, 1);
        }
        Some(buf)
    }

    /// `php_mysqlnd_rowp_read` + `php_mysqlnd_read_row_ex`. `prev_size` is
    /// the stale `packet->header.size` mysqlnd uses for its byte statistic.
    pub(crate) fn read_row(&mut self, binary: bool, prev_size: &mut usize) -> RowPacket {
        let gone = || {
            RowPacket::Err(ServerError {
                errno: CR_SERVER_GONE_ERROR,
                sqlstate: UNKNOWN_SQLSTATE.to_string(),
                message: MSG_SERVER_GONE.as_bytes().to_vec(),
            })
        };
        let mut data: Vec<u8> = Vec::new();
        let mut scratch = ErrorInfo::default();
        loop {
            let Some(size) = self.net.read_header(&mut self.stats, &mut scratch) else {
                self.state = ConnState::QuitSent;
                return if scratch.errno != 0 {
                    RowPacket::Err(ServerError {
                        errno: scratch.errno,
                        sqlstate: scratch.sqlstate.clone(),
                        message: scratch.error.clone(),
                    })
                } else {
                    gone()
                };
            };
            let start = data.len();
            data.resize(start + size, 0);
            if !self.net.receive(&mut data[start..], &mut self.stats) {
                self.state = ConnState::QuitSent;
                return gone();
            }
            if size < MAX_PACKET_SIZE {
                break;
            }
        }
        self.inc(
            Stat::BytesReceivedChangeUser,
            (HEADER_SIZE + *prev_size) as u64,
        );
        self.inc(Stat::PacketsReceivedChangeUser, 1);
        *prev_size = data.len();
        let p = protocol::classify_row(data);
        if let RowPacket::Row(_) = p {
            self.inc(
                if binary {
                    Stat::RowsFetchedFromServerPs
                } else {
                    Stat::RowsFetchedFromServerNormal
                },
                1,
            );
        }
        p
    }

    /// `mysqlnd_protocol::send_command`
    pub(crate) fn send_command(&mut self, cmd: Command, arg: &[u8], silent: bool) -> bool {
        match self.state {
            ConnState::Ready => {}
            ConnState::QuitSent => {
                self.error_info
                    .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
                return false;
            }
            _ => {
                self.error_info
                    .set_client(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
                return false;
            }
        }
        self.upsert.set_affected_rows_to_error();
        self.error_info.reset();
        inc_conn_index(
            &mut self.stats,
            Stat::ComQuit as usize + cmd as usize - 1,
            1,
        );
        if self
            .net
            .send_command(cmd, arg, &mut self.stats, &mut self.error_info)
            == 0
        {
            if !silent && self.error_info.errno != CR_SERVER_GONE_ERROR {
                diag::warn_plain(format!(
                    "Error while sending {} packet. PID={}",
                    cmd.as_text(),
                    diag::pid()
                ));
            }
            self.state = ConnState::QuitSent;
            self.send_close();
            return false;
        }
        true
    }

    /// `send_command_handle_OK`
    fn handle_ok(&mut self, ignore_upsert: bool) -> bool {
        let Some(buf) = self.read_packet(PacketKind::Ok) else {
            if self.error_info.errno != CR_SERVER_GONE_ERROR {
                self.error_info
                    .set_client(CR_MALFORMED_PACKET, "Malformed packet");
            }
            return false;
        };
        match protocol::parse_ok(&buf, self.session_tracking()) {
            None => {
                self.error_info
                    .set_client(CR_MALFORMED_PACKET, "Malformed packet");
                false
            }
            Some(Response::Err(e)) => {
                e.apply(&mut self.error_info);
                self.upsert.server_status &= !SERVER_MORE_RESULTS_EXISTS;
                self.upsert.set_affected_rows_to_error();
                false
            }
            Some(Response::Ok(ok)) => {
                self.last_message = if ok.message.is_empty() {
                    None
                } else {
                    Some(ok.message.clone())
                };
                if !ignore_upsert {
                    self.upsert.reset();
                    self.upsert.warning_count = u32::from(ok.warning_count);
                    self.upsert.server_status = ok.server_status;
                    self.upsert.affected_rows = ok.affected_rows;
                    self.upsert.last_insert_id = ok.last_insert_id;
                }
                self.session_track = ok.session_track;
                true
            }
            Some(_) => true,
        }
    }

    /// `send_command_handle_EOF`
    fn handle_eof(&mut self) -> bool {
        let Some(buf) = self.read_packet(PacketKind::Eof) else {
            self.error_info
                .set_client(CR_MALFORMED_PACKET, "Malformed packet");
            return false;
        };
        match protocol::parse_eof(&buf) {
            None => {
                self.error_info
                    .set_client(CR_MALFORMED_PACKET, "Malformed packet");
                false
            }
            Some(Err(e)) => {
                e.apply(&mut self.error_info);
                self.upsert.set_affected_rows_to_error();
                true
            }
            Some(Ok(e)) if e.field_count != protocol::EODATA_MARKER => {
                self.error_info
                    .set_client(CR_MALFORMED_PACKET, "Malformed packet");
                diag::warn(format!(
                    "EOF packet expected, field count wasn't 0xFE but 0x{:2X}",
                    e.field_count
                ));
                true
            }
            Some(Ok(_)) => true,
        }
    }

    /// `send_command_handle_response`
    fn handle_response(
        &mut self,
        eof: bool,
        silent: bool,
        cmd: Command,
        ignore_upsert: bool,
    ) -> bool {
        let ret = if eof {
            self.handle_eof()
        } else {
            self.handle_ok(ignore_upsert)
        };
        if !silent && self.error_info.errno == CR_MALFORMED_PACKET {
            diag::warn(format!(
                "Error while reading {}'s response packet. PID={}",
                cmd.as_text(),
                diag::pid()
            ));
        }
        ret
    }

    /// `mysqlnd_conn_data::send_close`
    pub(crate) fn send_close(&mut self) {
        if self.state >= ConnState::Ready {
            crate::stats::dec_global(Stat::OpenedConnections);
            if self.persistent {
                crate::stats::dec_global(Stat::OpenedPersistentConnections);
            }
        }
        match self.state {
            ConnState::Ready => {
                if self.net.is_open() {
                    self.send_command(Command::Quit, &[], true);
                    self.net.close();
                }
                self.state = ConnState::QuitSent;
            }
            ConnState::SendingLoadData
            | ConnState::NextResultPending
            | ConnState::QuerySent
            | ConnState::FetchingData => {
                inc_global(Stat::CloseInMiddle, 1);
                self.state = ConnState::QuitSent;
                self.net.close();
            }
            ConnState::Alloced => {
                self.state = ConnState::QuitSent;
                self.net.close();
            }
            ConnState::QuitSent => self.net.close(),
        }
    }

    /// `mysqlnd_conn_data::free_contents`
    fn free_contents(&mut self) {
        self.current_result = None;
        self.net.close();
        let o = &mut self.net.opts;
        o.ssl_key = None;
        o.ssl_cert = None;
        o.ssl_ca = None;
        o.ssl_capath = None;
        o.ssl_cipher = None;
        self.scheme = None;
        self.server_version = None;
        self.host_info = None;
        self.auth_plugin_data.clear();
        self.last_message = None;
        self.charset = None;
        self.greet_charset = None;
    }

    /// `mysqlnd_conn_data::get_server_version`
    pub(crate) fn server_version_number(&self) -> u64 {
        let Some(v) = &self.server_version else {
            return 0;
        };
        let b = v.as_bytes();
        let mut p = 0;
        let mut parts = [0i64; 3];
        for (i, part) in parts.iter_mut().enumerate() {
            let start = p;
            let (val, used) = strtol(&b[start.min(b.len())..]);
            *part = val;
            p = start + used;
            if i < 2 {
                p += 1;
            }
        }
        #[allow(clippy::cast_sign_loss)]
        let r = (parts[0] * 10000 + parts[1] * 100 + parts[2]) as u64;
        r
    }

    // ------------------------------------------------------------ connect

    /// `mysqlnd_conn_data::get_scheme`
    fn get_scheme(host: &str, socket: &mut Option<String>, port: u32) -> (String, bool) {
        if host.eq_ignore_ascii_case("localhost") {
            let s = socket.get_or_insert_with(|| "/tmp/mysql.sock".to_string());
            return (format!("unix://{s}"), true);
        }
        let port = if port == 0 { 3306 } else { port };
        let first = host.find(':');
        let is_v6 = first.is_some_and(|i| host[i + 1..].contains(':'));
        if !host.starts_with('[') && is_v6 {
            return (format!("tcp://[{host}]:{port}"), false);
        }
        let has_port = if host.starts_with('[') {
            host.find(']')
                .is_some_and(|i| host.as_bytes().get(i + 1) == Some(&b':'))
        } else {
            first.is_some()
        };
        if has_port {
            (format!("tcp://{host}"), false)
        } else {
            (format!("tcp://{host}:{port}"), false)
        }
    }

    /// `mysqlnd_conn_data::get_updated_connect_flags`
    fn updated_connect_flags(&self, mut flags: u32) -> u32 {
        flags |= MYSQLND_CAPABILITIES;
        flags |= self.options.flags;
        if self.net.use_compression {
            flags |= CLIENT_COMPRESS;
        }
        let o = &self.net.opts;
        if o.ssl_key.is_some()
            || o.ssl_cert.is_some()
            || o.ssl_ca.is_some()
            || o.ssl_capath.is_some()
            || o.ssl_cipher.is_some()
        {
            flags |= CLIENT_SSL;
        }
        if !self.options.connect_attr.is_empty() {
            flags |= CLIENT_CONNECT_ATTRS;
        }
        if self.options.session_track {
            flags |= CLIENT_SESSION_TRACK;
        }
        flags
    }

    /// `mysqlnd_conn_data::connect`
    #[allow(clippy::too_many_arguments)]
    fn connect(
        &mut self,
        host: Option<&str>,
        user: Option<&[u8]>,
        passwd: Option<&[u8]>,
        db: Option<&[u8]>,
        port: u32,
        socket: Option<&str>,
        mut flags: u32,
    ) -> bool {
        self.error_info.reset();
        self.upsert.set_affected_rows_to_error();
        let mut reconnect = false;
        if self.state > ConnState::Alloced {
            if self.state < ConnState::QuitSent {
                self.inc(Stat::CloseImplicit, 1);
                reconnect = true;
                self.send_close();
            }
            self.free_contents();
            self.net.compressed = false;
            self.net.ssl = false;
        } else {
            #[allow(clippy::cast_possible_truncation)]
            {
                self.options.max_allowed_packet = ASSEMBLED_PACKET_MAX_SIZE as u32;
            }
        }
        let host = match host {
            Some(h) if !h.is_empty() => h.to_string(),
            _ => "localhost".to_string(),
        };
        let user = user.unwrap_or_default().to_vec();
        let passwd = passwd.unwrap_or_default().to_vec();
        let db = db.unwrap_or_default().to_vec();
        if !db.is_empty() {
            flags |= CLIENT_CONNECT_WITH_DB;
        }
        let mut socket = socket.map(str::to_string);
        let (transport, unix_socket) = Self::get_scheme(&host, &mut socket, port);
        let flags = self.updated_connect_flags(flags);
        self.client_flags = flags;

        let ok = self.connect_handshake(&transport, &user, &passwd, &db, flags);
        if ok {
            self.state = ConnState::Ready;
            self.net.compressed = flags & CLIENT_COMPRESS != 0;
            self.scheme = Some(transport.clone());
            self.port = port;
            self.host_info = Some(if unix_socket {
                "Localhost via UNIX socket".to_string()
            } else {
                format!("{host} via TCP/IP")
            });
            self.error_info.reset();
            if self.infile.is_none() {
                self.infile = Some(Box::new(FsInfile));
            }
            if self.execute_init_commands() {
                self.inc(Stat::ConnectSuccess, 1);
                self.inc(Stat::OpenedConnections, 1);
                if reconnect {
                    inc_global(Stat::Reconnect, 1);
                }
                if self.persistent {
                    self.inc(Stat::PconnectSuccess, 1);
                    self.inc(Stat::OpenedPersistentConnections, 1);
                }
                return true;
            }
        }
        if self.error_info.errno == 0 {
            let via = self.scheme.clone().unwrap_or(transport);
            self.error_info.set_client(
                CR_CONNECTION_ERROR,
                &format!("Unknown error while trying to connect via {via}"),
            );
        }
        self.free_contents();
        self.inc(Stat::ConnectFailure, 1);
        false
    }

    /// `mysqlnd_conn_data::execute_init_commands`
    fn execute_init_commands(&mut self) -> bool {
        let cmds = self.options.init_commands.clone();
        for c in cmds {
            self.inc(Stat::InitCommandExecutedCount, 1);
            if !self.query(&c) {
                self.inc(Stat::InitCommandFailedCount, 1);
                return false;
            }
            loop {
                if self.last_query_type == QueryType::Select {
                    if let Some(meta) = self.take_current_for_use() {
                        // use_result + free: drain the rows
                        let mut prev = 0;
                        loop {
                            match self.read_row(false, &mut prev) {
                                RowPacket::Row(_) => {}
                                RowPacket::Eof(e) => {
                                    self.upsert.reset();
                                    self.upsert.warning_count = u32::from(e.warning_count);
                                    self.upsert.server_status = e.server_status;
                                    self.state =
                                        if e.server_status & SERVER_MORE_RESULTS_EXISTS != 0 {
                                            ConnState::NextResultPending
                                        } else {
                                            ConnState::Ready
                                        };
                                    break;
                                }
                                RowPacket::Err(e) => {
                                    e.apply(&mut self.error_info);
                                    if self.state != ConnState::QuitSent {
                                        self.state = ConnState::Ready;
                                    }
                                    break;
                                }
                            }
                        }
                        drop(meta);
                    }
                }
                if !self.next_result() {
                    break;
                }
            }
        }
        true
    }

    fn take_current_for_use(&mut self) -> Option<Rc<Metadata>> {
        if self.last_query_type != QueryType::Select || self.state != ConnState::FetchingData {
            return None;
        }
        self.inc(Stat::UnbufferedSets, 1);
        self.current_result.take()
    }

    /// `mysqlnd_conn_data::connect_handshake` + `mysqlnd_command::handshake`
    fn connect_handshake(
        &mut self,
        transport: &str,
        user: &[u8],
        passwd: &[u8],
        db: &[u8],
        flags: u32,
    ) -> bool {
        if !self
            .net
            .connect(transport, Duration::from_secs(60), &mut self.error_info)
        {
            return false;
        }
        self.net.reset();
        let greet = self
            .read_packet(PacketKind::Greet)
            .and_then(|b| protocol::parse_greeting(&b));
        let Some(greet) = greet else {
            diag::warn(format!(
                "Error while reading greeting packet. PID={}",
                diag::pid()
            ));
            self.server_capabilities = 0;
            return false;
        };
        if let Some(e) = &greet.error {
            e.apply(&mut self.error_info);
            self.server_capabilities = 0;
            return false;
        }
        if greet.pre41 {
            let v: String = greet.server_version.chars().take(32).collect();
            self.error_info.set_client(
                CR_NOT_IMPLEMENTED,
                &format!("Connecting to 3.22, 3.23 & 4.0 is not supported. Server is {v}"),
            );
            self.server_capabilities = 0;
            return false;
        }
        self.thread_id = u64::from(greet.thread_id);
        self.protocol_version = greet.protocol_version;
        self.server_version = Some(greet.server_version.clone());
        let mut charset_no = u32::from(greet.charset_no);
        if let Some(cs) = charset::find_by_nr(charset_no) {
            self.greet_charset = Some(cs);
        } else {
            charset_no = if self.server_version_number() >= 50500 {
                UTF8_MB4_DEFAULT_ID
            } else {
                UTF8_MB3_DEFAULT_ID
            };
            self.greet_charset = charset::find_by_nr(charset_no);
        }
        self.server_capabilities = greet.server_capabilities;
        if !self.switch_to_ssl_if_needed(charset_no, greet.server_capabilities, flags)
            || !self.run_authentication(
                user,
                passwd,
                db,
                &greet.auth_plugin_data,
                greet.auth_protocol.as_deref(),
                charset_no,
                flags,
                false,
                false,
            )
        {
            self.server_capabilities = 0;
            return false;
        }
        self.upsert.reset();
        self.upsert.server_status = greet.server_status;
        true
    }

    /// `mysqlnd_switch_to_ssl_if_needed` + `mysqlnd_command::enable_ssl`
    fn switch_to_ssl_if_needed(
        &mut self,
        mut charset_no: u32,
        server_caps: u32,
        flags: u32,
    ) -> bool {
        if let Some(cs) = self
            .options
            .charset_name
            .as_deref()
            .and_then(|n| charset::find_by_name(n.as_bytes()))
        {
            charset_no = cs.nr;
        }
        if flags & CLIENT_SSL == 0 {
            return true;
        }
        let close_conn = |c: &mut Self| {
            c.state = ConnState::QuitSent;
            c.send_close();
            c.error_info
                .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            false
        };
        if server_caps & CLIENT_SSL == 0 {
            return close_conn(self);
        }
        #[allow(clippy::cast_possible_truncation)]
        let pkt = AuthPacket {
            client_flags: flags,
            max_packet_size: ASSEMBLED_PACKET_MAX_SIZE as u32,
            charset_no,
            ..AuthPacket::default()
        };
        let Ok(payload) = protocol::encode_auth_packet(&pkt) else {
            return close_conn(self);
        };
        if self
            .net
            .send(&payload, &mut self.stats, &mut self.error_info)
            == 0
        {
            return close_conn(self);
        }
        self.net.opts.ssl_verify_peer = if flags & CLIENT_SSL_VERIFY_SERVER_CERT != 0 {
            SslPeer::Verify
        } else if flags & CLIENT_SSL_DONT_VERIFY_SERVER_CERT != 0 {
            SslPeer::DontVerify
        } else {
            SslPeer::Default
        };
        if !self.net.enable_ssl() {
            self.state = ConnState::QuitSent;
            self.error_info
                .set_client(CR_CONNECTION_ERROR, "Cannot connect to MySQL using SSL");
            return false;
        }
        true
    }

    /// The server's RSA public key for `sha256_password` /
    /// `caching_sha2_password` (`mysqlnd_sha256_get_rsa_key` /
    /// `mysqlnd_caching_sha2_get_key`).
    fn get_rsa_key(&mut self, caching: bool) -> Option<openssl::pkey::PKey<openssl::pkey::Public>> {
        let fname = self
            .net
            .server_public_key
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                ini::settings()
                    .sha256_server_public_key
                    .filter(|s| !s.is_empty())
            });
        if let Some(f) = fname {
            return match std::fs::read(&f) {
                Ok(pem) => auth::parse_public_key(&pem),
                Err(e) => {
                    diag::warn(format!(
                        "failed to open stream: {}",
                        crate::net::strerror(&e)
                    ));
                    None
                }
            };
        }
        let req: &[u8] = if caching { &[2] } else { &[1] };
        if self.net.send(req, &mut self.stats, &mut self.error_info) == 0 {
            diag::warn_plain(format!(
                "Error while sending public key request packet. PID={}",
                diag::pid()
            ));
            self.state = ConnState::QuitSent;
            return None;
        }
        let resp = self.read_packet(PacketKind::Sha256PkResponse);
        let key = match resp {
            Some(b) if b.len() >= 2 => Some(b[1..].to_vec()),
            Some(_) => {
                diag::warn("Premature end of data (mysqlnd_wireprotocol.c:2120)");
                diag::warn("SHA256_PK_REQUEST_RESPONSE packet shorter than expected");
                None
            }
            None => None,
        };
        if let Some(pem) = key {
            auth::parse_public_key(&pem)
        } else {
            diag::warn_plain(format!(
                "Error while receiving public key. PID={}",
                diag::pid()
            ));
            self.state = ConnState::QuitSent;
            None
        }
    }

    /// `auth_plugin->methods.get_auth_data`
    fn get_auth_data(&mut self, plugin: Plugin, passwd: &[u8], data: &[u8]) -> Vec<u8> {
        match plugin {
            Plugin::Native => {
                if data.len() < SCRAMBLE_LENGTH {
                    self.error_info.set_client(
                        CR_MALFORMED_PACKET,
                        "The server sent wrong length for scramble",
                    );
                    return Vec::new();
                }
                if passwd.is_empty() {
                    Vec::new()
                } else {
                    auth::scramble_native(data, passwd).to_vec()
                }
            }
            Plugin::ClearPassword => {
                let mut v = passwd.to_vec();
                v.push(0);
                v
            }
            Plugin::CachingSha2 => {
                if data.len() < SCRAMBLE_LENGTH {
                    self.error_info.set_client(
                        CR_MALFORMED_PACKET,
                        "The server sent wrong length for scramble",
                    );
                    return Vec::new();
                }
                if passwd.is_empty() {
                    Vec::new()
                } else {
                    auth::scramble_sha2(data, passwd).to_vec()
                }
            }
            Plugin::Sha256 => {
                if self.net.ssl {
                    let mut v = passwd.to_vec();
                    v.push(0);
                    return v;
                }
                match self.get_rsa_key(false) {
                    Some(key) => self.rsa_encrypt(&key, passwd, data),
                    None => Vec::new(),
                }
            }
        }
    }

    fn rsa_encrypt(
        &mut self,
        key: &openssl::pkey::PKey<openssl::pkey::Public>,
        passwd: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        match auth::rsa_encrypt_password(key, passwd, data) {
            Ok(v) => v,
            Err(RsaError::PasswordTooLong) => {
                self.error_info
                    .set_client(CR_UNKNOWN_ERROR, "password is too long");
                Vec::new()
            }
            Err(RsaError::InvalidKeySize) => {
                self.error_info
                    .set_client(CR_UNKNOWN_ERROR, "invalid public key size");
                Vec::new()
            }
            Err(RsaError::Failed) => Vec::new(),
        }
    }

    /// `mysqlnd_caching_sha2_handle_server_response`. Returns `Err(switch)`
    /// for an auth-switch request, `Ok(false)` for a hard failure.
    fn caching_sha2_response(
        &mut self,
        data: &[u8],
        passwd: &[u8],
    ) -> std::result::Result<bool, (String, Vec<u8>)> {
        if passwd.is_empty() {
            return Ok(true);
        }
        let Some(buf) = self.read_packet(PacketKind::CachedSha2Result) else {
            return Ok(true);
        };
        let Some(res) = protocol::parse_sha2_result(&buf) else {
            return Ok(true);
        };
        match res {
            Sha2Result::Err(e) => {
                self.error_info.set(e.errno, UNKNOWN_SQLSTATE, &e.message);
                Ok(false)
            }
            Sha2Result::Switch(name, d) => Err((name, d)),
            Sha2Result::Code(3) => Ok(true),
            Sha2Result::Code(4) => {
                let payload = if self.net.ssl || self.net.unix {
                    let mut v = passwd.to_vec();
                    v.push(0);
                    v
                } else {
                    match self.get_rsa_key(true) {
                        Some(key) => self.rsa_encrypt(&key, passwd, data),
                        None => Vec::new(),
                    }
                };
                self.net
                    .send(&payload, &mut self.stats, &mut self.error_info);
                Ok(true)
            }
            Sha2Result::Code(c) => {
                self.error_info.set_client(
                    CR_NOT_IMPLEMENTED,
                    &format!("Unexpected server response while doing caching_sha2 auth: {c}"),
                );
                Ok(true)
            }
        }
    }

    /// `mysqlnd_run_authentication`
    #[allow(clippy::too_many_arguments)]
    fn run_authentication(
        &mut self,
        user: &[u8],
        passwd: &[u8],
        db: &[u8],
        auth_plugin_data: &[u8],
        auth_protocol: Option<&str>,
        charset_no: u32,
        flags: u32,
        silent: bool,
        is_change_user: bool,
    ) -> bool {
        let mut first_call = true;
        let mut plugin_data = auth_plugin_data.to_vec();
        let mut requested = auth_protocol.unwrap_or(DEFAULT_AUTH_PROTOCOL).to_string();
        let mut ret;
        loop {
            let plugin = Plugin::by_name(&requested);
            if plugin.is_none() {
                if first_call {
                    requested = DEFAULT_AUTH_PROTOCOL.to_string();
                } else {
                    self.error_info.set_client(
                        CR_NOT_IMPLEMENTED,
                        &format!("The server requested authentication method unknown to the client [{requested}]"),
                    );
                    return false;
                }
            }
            self.auth_plugin_data.clone_from(&plugin_data);
            let scrambled = match plugin {
                Some(p) => self.get_auth_data(p, passwd, &plugin_data),
                None => Vec::new(),
            };
            if self.error_info.errno != 0 {
                return false;
            }
            let switch = if is_change_user {
                self.auth_change_user(
                    user,
                    passwd,
                    db,
                    silent,
                    first_call,
                    &requested,
                    plugin,
                    &plugin_data,
                    &scrambled,
                )
            } else {
                self.auth_handshake(
                    user,
                    passwd,
                    db,
                    flags,
                    charset_no,
                    first_call,
                    &requested,
                    plugin,
                    &plugin_data,
                    &scrambled,
                )
            };
            first_call = false;
            ret = switch.0;
            match switch.1 {
                Some((name, data)) => {
                    requested = name;
                    plugin_data = data;
                }
                None => break,
            }
            if ret || self.error_info.errno != 0 {
                break;
            }
        }
        if ret {
            self.options.auth_protocol = Some(requested);
        }
        ret
    }

    /// `mysqlnd_auth_handshake`: returns (success, auth switch request).
    #[allow(clippy::too_many_arguments)]
    fn auth_handshake(
        &mut self,
        user: &[u8],
        passwd: &[u8],
        db: &[u8],
        flags: u32,
        server_charset_no: u32,
        full_packet: bool,
        auth_protocol: &str,
        plugin: Option<Plugin>,
        orig_data: &[u8],
        auth_data: &[u8],
    ) -> (bool, Option<(String, Vec<u8>)>) {
        if full_packet {
            let charset_no = self
                .options
                .charset_name
                .as_deref()
                .and_then(|n| charset::find_by_name(n.as_bytes()))
                .map_or(server_charset_no, |c| c.nr);
            let attrs = if self.server_capabilities & CLIENT_CONNECT_ATTRS != 0 {
                self.options.connect_attr.clone()
            } else {
                Vec::new()
            };
            let pkt = AuthPacket {
                client_flags: flags,
                max_packet_size: self.options.max_allowed_packet,
                charset_no,
                send_auth_data: true,
                user,
                auth_data,
                db,
                auth_plugin_name: Some(auth_protocol),
                connect_attrs: &attrs,
                ..AuthPacket::default()
            };
            let Ok(payload) = protocol::encode_auth_packet(&pkt) else {
                self.error_info.set_client(
                    CR_UNKNOWN_ERROR,
                    "Authentication data too long. Won't fit into the buffer and will be truncated. Authentication will thus fail",
                );
                return (false, None);
            };
            if self
                .net
                .send(&payload, &mut self.stats, &mut self.error_info)
                == 0
            {
                self.state = ConnState::QuitSent;
                return (false, None);
            }
            self.charset = charset::find_by_nr(charset_no);
        } else if self
            .net
            .send(auth_data, &mut self.stats, &mut self.error_info)
            == 0
        {
            self.state = ConnState::QuitSent;
            self.error_info
                .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            return (false, None);
        }
        if plugin == Some(Plugin::CachingSha2) {
            match self.caching_sha2_response(orig_data, passwd) {
                Ok(true) => {}
                Ok(false) => return (false, None),
                Err(sw) => return (false, Some(sw)),
            }
        }
        let (code, resp) = match self.read_packet(PacketKind::Ok) {
            Some(b) => protocol::parse_auth_response(&b, false),
            None => (0, None),
        };
        match resp {
            Some(Response::Ok(ok)) => {
                self.last_message = if ok.message.is_empty() {
                    None
                } else {
                    Some(ok.message)
                };
                (true, None)
            }
            Some(Response::AuthSwitch(name, data)) => (false, Some((name, data))),
            Some(Response::OldAuthSwitch) => {
                self.error_info.set_client(CR_UNKNOWN_ERROR, MSG_OLD_PASSWD);
                (false, None)
            }
            Some(Response::Err(e)) => {
                self.error_info.set(e.errno, UNKNOWN_SQLSTATE, &e.message);
                (false, None)
            }
            None => {
                // a malformed 0xFE reply leaves response_code set without a plugin name
                if code == 0xFE {
                    self.error_info.set_client(CR_UNKNOWN_ERROR, MSG_OLD_PASSWD);
                }
                (false, None)
            }
        }
    }

    /// `mysqlnd_auth_change_user`: returns (success, auth switch request).
    #[allow(clippy::too_many_arguments)]
    fn auth_change_user(
        &mut self,
        user: &[u8],
        passwd: &[u8],
        db: &[u8],
        silent: bool,
        full_packet: bool,
        auth_protocol: &str,
        plugin: Option<Plugin>,
        orig_data: &[u8],
        auth_data: &[u8],
    ) -> (bool, Option<(String, Vec<u8>)>) {
        if full_packet {
            let attrs = if self.server_capabilities & CLIENT_CONNECT_ATTRS != 0 {
                self.options.connect_attr.clone()
            } else {
                Vec::new()
            };
            let charset_no = if self.server_version_number() >= 50123 {
                self.charset.map_or(0, |c| c.nr)
            } else {
                0
            };
            let pkt = AuthPacket {
                is_change_user: true,
                user,
                auth_data,
                db,
                charset_no,
                auth_plugin_name: Some(auth_protocol),
                connect_attrs: &attrs,
                ..AuthPacket::default()
            };
            let Ok(payload) = protocol::encode_auth_packet(&pkt) else {
                self.error_info.set_client(
                    CR_UNKNOWN_ERROR,
                    "Authentication data too long. Won't fit into the buffer and will be truncated. Authentication will thus fail",
                );
                return (false, None);
            };
            if !self.send_command(Command::ChangeUser, &payload, silent) {
                self.state = ConnState::QuitSent;
                self.error_info
                    .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
                return (false, None);
            }
        } else if self
            .net
            .send(auth_data, &mut self.stats, &mut self.error_info)
            == 0
        {
            self.state = ConnState::QuitSent;
            self.error_info
                .set_client(CR_SERVER_GONE_ERROR, MSG_SERVER_GONE);
            return (false, None);
        }
        if plugin == Some(Plugin::CachingSha2) {
            match self.caching_sha2_response(orig_data, passwd) {
                Ok(true) => {}
                Ok(false) => return (false, None),
                Err(sw) => return (false, Some(sw)),
            }
        }
        let mut pkt_err = ErrorInfo::default();
        let mut ret = false;
        let mut switch = None;
        let mut asked_323 = false;
        let mut code = 0;
        if let Some(buf) = self.read_packet(PacketKind::ChgUser) {
            let r = protocol::parse_change_user_response(&buf, self.server_capabilities);
            ret = r.ok;
            code = r.code;
            asked_323 = r.asked_323;
            if let Some(e) = &r.error {
                e.apply(&mut pkt_err);
            }
            switch = r.switch;
        }
        self.error_info.copy_from(&pkt_err);
        if code == 0xFE {
            ret = false;
            if switch.is_none() {
                self.error_info.set_client(CR_UNKNOWN_ERROR, MSG_OLD_PASSWD);
            }
        }
        if self.error_info.errno != 0 {
            ret = false;
        }
        if ret {
            self.last_message = None;
            self.upsert.reset();
        } else if asked_323 {
            self.error_info.set_client(CR_UNKNOWN_ERROR, MSG_OLD_PASSWD);
        }
        (ret, switch)
    }

    // ------------------------------------------------------------ queries

    /// `mysqlnd_conn_data::query` (send + reap).
    pub(crate) fn query(&mut self, sql: &[u8]) -> bool {
        if !self.send_query(sql) || !self.reap_query() {
            return false;
        }
        if self.last_query_type == QueryType::Upsert && self.upsert.affected_rows != 0 {
            let a = self.upsert.affected_rows;
            self.inc(Stat::RowsAffectedNormal, a);
        }
        true
    }

    /// `mysqlnd_conn_data::send_query`
    pub(crate) fn send_query(&mut self, sql: &[u8]) -> bool {
        if self.send_command(Command::Query, sql, false) {
            self.state = ConnState::QuerySent;
            true
        } else {
            false
        }
    }

    /// `mysqlnd_conn_data::reap_query`
    pub(crate) fn reap_query(&mut self) -> bool {
        if self.state <= ConnState::Ready || self.state == ConnState::QuitSent {
            diag::warn("Connection not opened, clear or has been closed");
        }
        match self.read_result_set_header(false) {
            Ok(Header::ResultSet(meta)) => {
                self.current_result = Some(meta);
                true
            }
            Ok(Header::NoResult) => true,
            Err(()) => false,
        }
    }

    /// `mysqlnd_query_read_result_set_header`. For prepared statements
    /// (`ps`), the metadata is returned instead of being parked in
    /// `current_result`.
    pub(crate) fn read_result_set_header(&mut self, ps: bool) -> std::result::Result<Header, ()> {
        self.upsert.set_affected_rows_to_error();
        let Some(buf) = self.read_packet(PacketKind::RsetHeader) else {
            if self.error_info.errno != CR_SERVER_GONE_ERROR
                && self.error_info.errno != CR_CLIENT_INTERACTION_TIMEOUT
            {
                diag::warn("Error reading result set's header");
            }
            return Err(());
        };
        let Some(header) = protocol::parse_rset_header(&buf, self.session_tracking()) else {
            diag::warn("Error reading result set's header");
            return Err(());
        };
        match header {
            RsetHeader::Err(e) => {
                self.upsert.server_status &= !SERVER_MORE_RESULTS_EXISTS;
                e.apply(&mut self.error_info);
                self.state = ConnState::Ready;
                Err(())
            }
            RsetHeader::LocalInfile(file) => {
                self.error_info.errno = 0;
                self.last_query_type = QueryType::LoadLocal;
                self.field_count = 0;
                self.state = ConnState::SendingLoadData;
                let (ok, is_warning) = self.handle_local_infile(&file);
                self.state = if ok || is_warning {
                    ConnState::Ready
                } else {
                    ConnState::QuitSent
                };
                self.inc(Stat::NonRsetQuery, 1);
                if ok {
                    Ok(Header::NoResult)
                } else {
                    Err(())
                }
            }
            RsetHeader::Upsert(ok) => {
                self.error_info.errno = 0;
                self.last_query_type = QueryType::Upsert;
                self.field_count = 0;
                self.upsert.reset();
                self.upsert.warning_count = u32::from(ok.warning_count);
                self.upsert.server_status = ok.server_status;
                self.upsert.affected_rows = ok.affected_rows;
                self.upsert.last_insert_id = ok.last_insert_id;
                self.last_message = if ok.message.is_empty() {
                    None
                } else {
                    Some(ok.message)
                };
                self.session_track = ok.session_track;
                self.state = if self.upsert.server_status & SERVER_MORE_RESULTS_EXISTS != 0 {
                    ConnState::NextResultPending
                } else {
                    ConnState::Ready
                };
                self.inc(Stat::NonRsetQuery, 1);
                Ok(Header::NoResult)
            }
            RsetHeader::ResultSet(n) => {
                self.error_info.errno = 0;
                self.last_message = None;
                self.inc(Stat::RsetQuery, 1);
                self.upsert.reset();
                self.upsert.set_affected_rows_to_error();
                self.last_query_type = QueryType::Select;
                self.state = ConnState::FetchingData;
                self.field_count = u32::try_from(n).unwrap_or(u32::MAX);
                let _ = ps;
                let meta = self.read_metadata(self.field_count)?;
                let Some(eof) = self
                    .read_packet(PacketKind::Eof)
                    .and_then(|b| protocol::parse_eof(&b))
                else {
                    return Err(());
                };
                let eof = eof.unwrap_or_default();
                self.upsert.warning_count = u32::from(eof.warning_count);
                self.upsert.server_status = eof.server_status;
                let stat = if eof.server_status & SERVER_QUERY_NO_GOOD_INDEX_USED != 0 {
                    Some(Stat::BadIndexUsed)
                } else if eof.server_status & SERVER_QUERY_NO_INDEX_USED != 0 {
                    Some(Stat::NoIndexUsed)
                } else if eof.server_status & SERVER_QUERY_WAS_SLOW != 0 {
                    Some(Stat::QueryWasSlow)
                } else {
                    None
                };
                if let Some(s) = stat {
                    self.inc(s, 1);
                }
                Ok(Header::ResultSet(Rc::new(meta)))
            }
        }
    }

    /// `mysqlnd_res_meta::read_metadata`
    pub(crate) fn read_metadata(&mut self, n: u32) -> std::result::Result<Metadata, ()> {
        let mut fields = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let Some(buf) = self.read_packet(PacketKind::Field) else {
                return Err(());
            };
            match protocol::parse_field(&buf) {
                None => return Err(()),
                Some(Err(e)) => {
                    e.apply(&mut self.error_info);
                    return Err(());
                }
                Some(Ok(f)) => {
                    if !is_known_type(f.type_) {
                        diag::warn(format!(
                            "Unknown type {} sent by the server. Please send a report to the developers",
                            f.type_
                        ));
                        return Err(());
                    }
                    fields.push(f);
                }
            }
        }
        Ok(Metadata::new(fields))
    }

    /// `mysqlnd_conn_data::next_result`
    pub(crate) fn next_result(&mut self) -> bool {
        self.error_info.reset();
        if self.state != ConnState::NextResultPending {
            return false;
        }
        self.upsert.set_affected_rows_to_error();
        match self.read_result_set_header(false) {
            Err(()) => {
                if self.error_info.errno == 0 {
                    diag::warn(format!("Serious error. PID={}", diag::pid()));
                    self.state = ConnState::QuitSent;
                    self.send_close();
                }
                false
            }
            Ok(h) => {
                if let Header::ResultSet(m) = h {
                    self.current_result = Some(m);
                }
                if self.last_query_type == QueryType::Upsert && self.upsert.affected_rows != 0 {
                    let a = self.upsert.affected_rows;
                    self.inc(Stat::RowsAffectedNormal, a);
                }
                true
            }
        }
    }

    /// `mysqlnd_handle_local_infile`: returns (success, `is_warning`).
    fn handle_local_infile(&mut self, filename: &[u8]) -> (bool, bool) {
        let enabled = self.options.flags & CLIENT_LOCAL_FILES == CLIENT_LOCAL_FILES;
        let dir = self.options.local_infile_directory.clone();
        let mut prerequisites_ok = true;
        if !enabled && dir.is_none() {
            self.error_info.set_client(
                CR_LOAD_DATA_LOCAL_INFILE_REJECTED,
                "LOAD DATA LOCAL INFILE is forbidden, check related settings like mysqli.allow_local_infile|mysqli.local_infile_directory or Pdo\\Mysql::ATTR_LOCAL_INFILE|Pdo\\Mysql::ATTR_LOCAL_INFILE_DIRECTORY",
            );
            prerequisites_ok = false;
        }
        if let Some(d) = &dir {
            if !infile::dir_accessible(d) {
                self.error_info.set_client(
                    CR_LOAD_DATA_LOCAL_INFILE_REJECTED,
                    "cannot open local_infile_directory",
                );
                prerequisites_ok = false;
            }
        }
        if prerequisites_ok && !enabled {
            if let Some(d) = &dir {
                if !infile::path_within(d, filename) {
                    self.error_info.set_client(
                        CR_LOAD_DATA_LOCAL_INFILE_REJECTED,
                        "LOAD DATA LOCAL INFILE DIRECTORY restriction in effect. Unable to open file",
                    );
                    prerequisites_ok = false;
                }
            }
        }
        let mut result = false;
        let mut is_warning = false;
        'send: {
            if !prerequisites_ok {
                self.net.send(&[], &mut self.stats, &mut self.error_info);
                is_warning = true;
                break 'send;
            }
            let mut handler = self.infile.take().unwrap_or_else(|| Box::new(FsInfile));
            let opened = handler.open(filename);
            self.infile = Some(handler);
            let mut reader = match opened {
                Ok(r) => r,
                Err((errno, msg)) => {
                    is_warning = true;
                    self.error_info.set_client(errno, &msg);
                    self.net.send(&[], &mut self.stats, &mut self.error_info);
                    break 'send;
                }
            };
            let mut buf = vec![0u8; 4096 - HEADER_SIZE];
            let mut read_error = false;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if self
                            .net
                            .send(&buf[..n], &mut self.stats, &mut self.error_info)
                            == 0
                        {
                            self.error_info.set_client(
                                CR_SERVER_LOST,
                                "Lost connection to MySQL server during LOAD DATA of a local file",
                            );
                            break 'send;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        read_error = true;
                        break;
                    }
                }
            }
            if self.net.send(&[], &mut self.stats, &mut self.error_info) == 0 {
                self.error_info.set_client(
                    CR_SERVER_LOST,
                    "Lost connection to MySQL server during LOAD DATA of a local file",
                );
                break 'send;
            }
            if read_error {
                is_warning = true;
                self.error_info
                    .set_client(infile::READ_ERROR.0, infile::READ_ERROR.1);
                break 'send;
            }
            result = true;
        }
        if !self.handle_response(false, false, Command::Query, false) {
            result = false;
        }
        (result, is_warning)
    }

    /// Buffers every remaining row of the current result set
    /// (`mysqlnd_res::store_result_fetch_data`). On failure the error is
    /// returned (not yet copied to the connection).
    ///
    /// At `memory_limit` (a violation pending from the last row, or the
    /// buffer growing past the limit) the rows read so far are dropped and
    /// the rest of the set is read and discarded, so the connection stays
    /// in step with the server; the VM raises the pending memory fatal when
    /// the call returns.
    pub(crate) fn fetch_all_rows(
        &mut self,
        binary: bool,
    ) -> std::result::Result<Vec<Vec<u8>>, ErrorEntry> {
        use php_memlimit::TryGrow;
        let mut rows = Vec::new();
        let mut prev = 0;
        let mut err = None;
        let mut over_limit = false;
        loop {
            match self.read_row(binary, &mut prev) {
                RowPacket::Row(r) => {
                    if !over_limit && (php_memlimit::poll().is_err() || rows.try_grow(1).is_err()) {
                        over_limit = true;
                        rows = Vec::new();
                    }
                    if !over_limit {
                        rows.push(r);
                    }
                }
                RowPacket::Eof(e) => {
                    self.upsert.reset();
                    self.upsert.warning_count = u32::from(e.warning_count);
                    self.upsert.server_status = e.server_status;
                    break;
                }
                RowPacket::Err(e) => {
                    err = Some(ErrorEntry {
                        errno: e.errno,
                        sqlstate: e.sqlstate,
                        error: e.message,
                    });
                    break;
                }
            }
        }
        self.inc(
            if binary {
                Stat::RowsBufferedFromClientPs
            } else {
                Stat::RowsBufferedFromClientNormal
            },
            rows.len() as u64,
        );
        if err.is_some() {
            self.upsert.server_status &= !SERVER_MORE_RESULTS_EXISTS;
        }
        self.state = if self.upsert.server_status & SERVER_MORE_RESULTS_EXISTS != 0 {
            ConnState::NextResultPending
        } else {
            ConnState::Ready
        };
        match err {
            Some(e) => Err(e),
            None => Ok(rows),
        }
    }

    /// `mysqlnd_conn_data::set_charset`
    fn set_charset(&mut self, name: &str) -> bool {
        let Some(cs) = charset::find_by_name(name.as_bytes()) else {
            self.error_info
                .set_client(CR_CANT_FIND_CHARSET, "Invalid character set was provided");
            return false;
        };
        let q = format!("SET NAMES {name}");
        if !self.query(q.as_bytes()) || self.error_info.errno != 0 {
            return false;
        }
        self.charset = Some(cs);
        true
    }

    /// `mysqlnd_conn_data::change_user`
    fn change_user(&mut self, user: &[u8], passwd: &[u8], db: &[u8], silent: bool) -> bool {
        self.error_info.reset();
        self.upsert.set_affected_rows_to_error();
        let data = self.auth_plugin_data.clone();
        let proto = self.options.auth_protocol.clone();
        let caps = self.server_capabilities;
        let db = match db.iter().position(|&b| b == 0) {
            Some(n) => &db[..n],
            None => db,
        };
        self.run_authentication(
            user,
            passwd,
            db,
            &data,
            proto.as_deref(),
            0,
            caps,
            silent,
            true,
        )
    }
}

/// `strtol(p, &end, 10)` returning (value, bytes consumed).
fn strtol(b: &[u8]) -> (i64, usize) {
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        i += 1;
    }
    let ds = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == ds {
        return (0, 0);
    }
    (crate::codec::atoll(&b[start..i]), i)
}

/// `mysqlnd_escape_string_for_tx_name_in_comment`
fn tx_name_comment(name: Option<&[u8]>) -> String {
    let Some(name) = name else {
        return String::new();
    };
    let mut out = String::from(" /*");
    let mut warned = false;
    for &v in name {
        if v == 0 {
            break;
        }
        if v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_' | b' ' | b'=') {
            out.push(char::from(v));
        } else if !warned {
            diag::warn("Transaction name has been truncated, since it can only contain the A-Z, a-z, 0-9, \"\\\", \"-\", \"_\", and \"=\" characters");
            warned = true;
        }
    }
    out.push_str("*/");
    out
}

/// `tx_cor_options_to_string`
fn tx_cor_options(flags: u32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if flags & TRANS_COR_AND_CHAIN != 0 && flags & TRANS_COR_AND_NO_CHAIN == 0 {
        parts.push("AND CHAIN");
    } else if flags & TRANS_COR_AND_NO_CHAIN != 0 && flags & TRANS_COR_AND_CHAIN == 0 {
        parts.push("AND NO CHAIN");
    }
    if flags & TRANS_COR_RELEASE != 0 && flags & TRANS_COR_NO_RELEASE == 0 {
        parts.push("RELEASE");
    } else if flags & TRANS_COR_NO_RELEASE != 0 && flags & TRANS_COR_RELEASE == 0 {
        parts.push("NO RELEASE");
    }
    parts.join(" ")
}

impl Default for Connection {
    fn default() -> Self {
        Self::init()
    }
}

impl Connection {
    /// `mysqlnd_init()` / `mysqli_init()`: an unconnected handle.
    #[must_use]
    pub fn init() -> Self {
        Self::init_ex(false)
    }

    /// Like [`Connection::init`], marking the handle persistent (affects
    /// the `p*` statistics only).
    #[must_use]
    pub fn init_ex(persistent: bool) -> Self {
        Connection(Rc::new(RefCell::new(ConnData::new(persistent))))
    }

    pub(crate) fn data(&self) -> RefMut<'_, ConnData> {
        self.0.borrow_mut()
    }

    /// Whether two handles refer to the same connection.
    #[must_use]
    pub fn ptr_eq(a: &Self, b: &Self) -> bool {
        Rc::ptr_eq(&a.0, &b.0)
    }

    fn fail<T>(&self) -> Result<T> {
        Err(self.data().error_info.to_error())
    }

    fn check(&self, ok: bool) -> Result<()> {
        if ok {
            Ok(())
        } else {
            self.fail()
        }
    }

    // ------------------------------------------------------------ options

    /// `mysqlnd_conn_data::set_client_option` (`mysqli_options`). Returns
    /// `false` for unsupported options or rejected values.
    #[allow(clippy::too_many_lines)]
    pub fn set_option(&self, option: u32, value: OptionValue<'_>) -> bool {
        use crate::consts::opt::{
            MYSQLND_OPT_AUTH_PROTOCOL, MYSQLND_OPT_INT_AND_FLOAT_NATIVE,
            MYSQLND_OPT_MAX_ALLOWED_PACKET, MYSQLND_OPT_NET_CMD_BUFFER_SIZE,
            MYSQLND_OPT_NET_READ_BUFFER_SIZE, MYSQLND_OPT_SSL_CA, MYSQLND_OPT_SSL_CAPATH,
            MYSQLND_OPT_SSL_CERT, MYSQLND_OPT_SSL_CIPHER, MYSQLND_OPT_SSL_KEY,
            MYSQLND_OPT_SSL_PASSPHRASE, MYSQL_INIT_COMMAND, MYSQL_OPT_CAN_HANDLE_EXPIRED_PASSWORDS,
            MYSQL_OPT_COMPRESS, MYSQL_OPT_CONNECT_ATTR_DELETE, MYSQL_OPT_CONNECT_ATTR_RESET,
            MYSQL_OPT_CONNECT_TIMEOUT, MYSQL_OPT_LOAD_DATA_LOCAL_DIR, MYSQL_OPT_LOCAL_INFILE,
            MYSQL_OPT_NAMED_PIPE, MYSQL_OPT_PROTOCOL, MYSQL_OPT_READ_TIMEOUT,
            MYSQL_OPT_SSL_VERIFY_SERVER_CERT, MYSQL_READ_DEFAULT_FILE, MYSQL_READ_DEFAULT_GROUP,
            MYSQL_SERVER_PUBLIC_KEY, MYSQL_SET_CHARSET_NAME,
        };
        let mut c = self.data();
        match option {
            MYSQL_OPT_READ_TIMEOUT => c.net.opts.timeout_read = value.as_uint(),
            MYSQL_OPT_CONNECT_TIMEOUT => c.net.opts.timeout_connect = value.as_uint(),
            MYSQLND_OPT_NET_READ_BUFFER_SIZE => {
                c.net.opts.net_read_buffer_size = value.as_uint() as usize;
            }
            MYSQLND_OPT_SSL_KEY => c.net.opts.ssl_key = value.as_opt_string(),
            MYSQLND_OPT_SSL_CERT => c.net.opts.ssl_cert = value.as_opt_string(),
            MYSQLND_OPT_SSL_CA => c.net.opts.ssl_ca = value.as_opt_string(),
            MYSQLND_OPT_SSL_CAPATH => c.net.opts.ssl_capath = value.as_opt_string(),
            MYSQLND_OPT_SSL_CIPHER => c.net.opts.ssl_cipher = value.as_opt_string(),
            MYSQLND_OPT_SSL_PASSPHRASE => c.net.opts.ssl_passphrase = value.as_opt_string(),
            MYSQL_OPT_SSL_VERIFY_SERVER_CERT => {
                c.net.opts.ssl_verify_peer = match value.as_uint() {
                    1 => SslPeer::Verify,
                    2 => SslPeer::DontVerify,
                    _ => SslPeer::Default,
                }
            }
            MYSQLND_OPT_NET_CMD_BUFFER_SIZE => {
                let v = value.as_uint() as usize;
                if v < NET_CMD_BUFFER_MIN_SIZE {
                    return false;
                }
                c.net.cmd_buffer_size = v;
            }
            MYSQL_OPT_COMPRESS => c.net.use_compression = true,
            MYSQL_SERVER_PUBLIC_KEY => c.net.server_public_key = value.as_opt_string(),
            MYSQLND_OPT_INT_AND_FLOAT_NATIVE => {
                c.options.int_and_float_native = value.as_uint() != 0;
            }
            MYSQL_OPT_LOCAL_INFILE => {
                if value.as_uint() != 0 {
                    c.options.flags |= CLIENT_LOCAL_FILES;
                } else {
                    c.options.flags &= !CLIENT_LOCAL_FILES;
                }
            }
            MYSQL_OPT_LOAD_DATA_LOCAL_DIR => {
                c.options.local_infile_directory = value.as_opt_string().filter(|s| !s.is_empty());
            }
            MYSQL_INIT_COMMAND => {
                let v = match value {
                    OptionValue::Str(s) => s.to_vec(),
                    OptionValue::Int(i) => i.to_string().into_bytes(),
                    OptionValue::Null => Vec::new(),
                };
                c.options.init_commands.push(v);
            }
            MYSQL_READ_DEFAULT_FILE | MYSQL_READ_DEFAULT_GROUP => {}
            MYSQL_SET_CHARSET_NAME => {
                let name = value.as_opt_string().unwrap_or_default();
                if charset::find_by_name(name.as_bytes()).is_none() {
                    c.error_info
                        .set_client(CR_CANT_FIND_CHARSET, "Unknown character set");
                    return false;
                }
                c.options.charset_name = Some(name);
            }
            MYSQL_OPT_NAMED_PIPE => c.options.protocol = 3,
            MYSQL_OPT_PROTOCOL => {
                let v = value.as_uint();
                if v < 5 {
                    c.options.protocol = v;
                }
            }
            MYSQLND_OPT_MAX_ALLOWED_PACKET => {
                let v = value.as_uint();
                if v > (1 << 16) {
                    c.options.max_allowed_packet = v;
                }
            }
            MYSQLND_OPT_AUTH_PROTOCOL => c.options.auth_protocol = value.as_opt_string(),
            MYSQL_OPT_CAN_HANDLE_EXPIRED_PASSWORDS => {
                if value.as_uint() != 0 {
                    c.options.flags |= CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS;
                } else {
                    c.options.flags &= !CLIENT_CAN_HANDLE_EXPIRED_PASSWORDS;
                }
            }
            MYSQL_OPT_CONNECT_ATTR_RESET => c.options.connect_attr.clear(),
            MYSQL_OPT_CONNECT_ATTR_DELETE => {
                if let Some(k) = value.as_opt_string() {
                    c.options.connect_attr.retain(|(key, _)| *key != k);
                }
            }
            _ => return false,
        }
        true
    }

    /// `MYSQL_OPT_CONNECT_ATTR_ADD` (`set_client_option_2d`).
    pub fn connect_attr_add(&self, key: &str, value: &str) {
        let mut c = self.data();
        if let Some(slot) = c.options.connect_attr.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value.to_string();
        } else {
            c.options
                .connect_attr
                .push((key.to_string(), value.to_string()));
        }
    }

    /// The connection attributes that will be sent.
    #[must_use]
    pub fn connect_attrs(&self) -> Vec<(String, String)> {
        self.data().options.connect_attr.clone()
    }

    /// `mysqlnd_ssl_set` (`mysqli_ssl_set`).
    pub fn ssl_set(
        &self,
        key: Option<&str>,
        cert: Option<&str>,
        ca: Option<&str>,
        capath: Option<&str>,
        cipher: Option<&str>,
    ) {
        let mut c = self.data();
        let o = &mut c.net.opts;
        o.ssl_key = key.map(str::to_string);
        o.ssl_cert = cert.map(str::to_string);
        o.ssl_ca = ca.map(str::to_string);
        o.ssl_capath = capath.map(str::to_string);
        o.ssl_cipher = cipher.map(str::to_string);
    }

    /// Opt in to `CLIENT_SESSION_TRACK` (mysqlnd never requests it; useful
    /// for clients that want GTIDs / system-variable changes from OK packets).
    pub fn enable_session_track(&self, on: bool) {
        self.data().options.session_track = on;
    }

    /// Session-state changes from the last OK packet (type, raw data).
    #[must_use]
    pub fn session_track(&self) -> Vec<(u8, Vec<u8>)> {
        self.data().session_track.clone()
    }

    /// Replaces the LOAD DATA LOCAL file opener.
    pub fn set_local_infile_handler(&self, h: Box<dyn LocalInfileHandler>) {
        self.data().infile = Some(h);
    }

    /// Restores the default LOAD DATA LOCAL opener (`mysqlnd_local_infile_default`).
    pub fn set_local_infile_default(&self) {
        self.data().infile = Some(Box::new(FsInfile));
    }

    /// `negotiate_client_api_capabilities`: returns the previous value.
    pub fn negotiate_client_api_capabilities(&self, flags: u32) -> u32 {
        std::mem::replace(&mut self.data().client_api_capabilities, flags)
    }

    // ------------------------------------------------------------ connect

    /// `mysqlnd_conn::connect`: connects (or reconnects) this handle.
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &self,
        host: Option<&str>,
        user: Option<&[u8]>,
        passwd: Option<&[u8]>,
        db: Option<&[u8]>,
        port: u32,
        socket: Option<&str>,
        flags: u32,
    ) -> Result<()> {
        self.connect_attr_add("_client_name", "mysqlnd");
        if let Some(h) = host.filter(|h| !h.is_empty()) {
            self.connect_attr_add("_server_host", h);
        }
        let ok = self
            .data()
            .connect(host, user, passwd, db, port, socket, flags);
        self.check(ok)
    }

    /// `mysqlnd_conn::close`: closes the connection, counting the close type.
    pub fn close(&self, close_type: CloseType) {
        let mut c = self.data();
        if c.state >= ConnState::Ready {
            let s = match close_type {
                CloseType::Explicit => Stat::CloseExplicit,
                CloseType::Implicit => Stat::CloseImplicit,
                CloseType::Disconnected => Stat::CloseDisconnect,
            };
            c.inc(s, 1);
        }
        c.send_close();
    }

    /// `restart_psession`: a persistent connection is being reused.
    pub fn restart_psession(&self) {
        let mut c = self.data();
        c.inc(Stat::ConnectReused, 1);
        c.current_result = None;
        c.last_message = None;
    }

    /// `end_psession`: forget per-request state of a persistent connection.
    pub fn end_psession(&self) {
        let mut c = self.data();
        c.current_result = None;
        c.last_message = None;
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> ConnState {
        self.data().state
    }

    /// Whether the handle is connected and usable.
    #[must_use]
    pub fn is_connected(&self) -> bool {
        let s = self.state();
        s != ConnState::Alloced && s != ConnState::QuitSent
    }

    // ------------------------------------------------------------ simple commands

    /// `mysqlnd_conn_data::select_db` (`COM_INIT_DB`).
    pub fn select_db(&self, db: &[u8]) -> Result<()> {
        let ok = {
            let mut c = self.data();
            let mut ok = c.send_command(Command::InitDb, db, false);
            if ok {
                ok = c.handle_response(false, false, Command::InitDb, true);
            }
            c.upsert.set_affected_rows_to_error();
            ok
        };
        self.check(ok)
    }

    /// `COM_PING`
    pub fn ping(&self) -> Result<()> {
        let ok = {
            let mut c = self.data();
            let mut ok = c.send_command(Command::Ping, &[], true);
            if ok {
                ok = c.handle_response(false, true, Command::Ping, true);
            }
            c.upsert.set_affected_rows_to_error();
            ok
        };
        self.check(ok)
    }

    /// `COM_STATISTICS`: the server status line.
    pub fn statistics(&self) -> Result<Vec<u8>> {
        let r = {
            let mut c = self.data();
            if c.send_command(Command::Statistics, &[], false) {
                c.read_packet(PacketKind::Stats)
            } else {
                None
            }
        };
        match r {
            Some(m) => Ok(m),
            None => self.fail(),
        }
    }

    /// `mysqlnd_conn_data::kill` (`COM_PROCESS_KILL`). Killing our own
    /// thread doesn't wait for a reply and marks the connection closed.
    pub fn kill(&self, pid: u32) -> Result<()> {
        let ok = {
            let mut c = self.data();
            let read_response = u64::from(pid) != c.thread_id;
            let mut ok = c.send_command(Command::ProcessKill, &pid.to_le_bytes(), false);
            if ok && read_response {
                ok = c.handle_response(false, false, Command::ProcessKill, true);
            }
            if read_response {
                c.upsert.set_affected_rows_to_error();
            } else if ok {
                c.state = ConnState::QuitSent;
                c.send_close();
            }
            ok
        };
        self.check(ok)
    }

    /// `COM_REFRESH`
    pub fn refresh(&self, options: u8) -> Result<()> {
        let ok = {
            let mut c = self.data();
            c.send_command(Command::Refresh, &[options], false)
                && c.handle_response(false, false, Command::Refresh, true)
        };
        self.check(ok)
    }

    /// `COM_SET_OPTION` (multi statements on/off).
    pub fn set_server_option(&self, option: u16) -> Result<()> {
        let ok = {
            let mut c = self.data();
            c.send_command(Command::SetOption, &option.to_le_bytes(), false)
                && c.handle_response(true, false, Command::SetOption, true)
        };
        self.check(ok)
    }

    /// `COM_DEBUG` (`mysqli_dump_debug_info`).
    pub fn dump_debug_info(&self) -> Result<()> {
        let ok = {
            let mut c = self.data();
            c.send_command(Command::Debug, &[], false)
                && c.handle_response(true, false, Command::Debug, true)
        };
        self.check(ok)
    }

    /// `mysqlnd_conn_data::set_charset` (`SET NAMES`).
    pub fn set_charset(&self, name: &str) -> Result<()> {
        let ok = self.data().set_charset(name);
        self.check(ok)
    }

    /// `mysqlnd_conn_data::change_user` (`COM_CHANGE_USER`).
    pub fn change_user(
        &self,
        user: &[u8],
        passwd: &[u8],
        db: Option<&[u8]>,
        silent: bool,
    ) -> Result<()> {
        let ok = self
            .data()
            .change_user(user, passwd, db.unwrap_or_default(), silent);
        self.check(ok)
    }

    /// `mysqlnd_conn_data::escape_string`: charset-aware escaping
    /// (`mysqli_real_escape_string`). Uses quote doubling when the server
    /// runs with `NO_BACKSLASH_ESCAPES`.
    #[must_use]
    pub fn escape_string(&self, s: &[u8]) -> Vec<u8> {
        let c = self.data();
        let cs = c
            .charset
            .or_else(|| charset::find_by_nr(UTF8_MB4_DEFAULT_ID))
            .unwrap_or(&charset::CHARSETS[0]);
        if c.upsert.server_status & SERVER_STATUS_NO_BACKSLASH_ESCAPES != 0 {
            charset::escape_quotes(cs, s)
        } else {
            charset::escape_slashes(cs, s)
        }
    }

    // ------------------------------------------------------------ transactions

    /// `SET AUTOCOMMIT=0|1`
    pub fn autocommit(&self, on: bool) -> Result<()> {
        self.query(if on {
            b"SET AUTOCOMMIT=1"
        } else {
            b"SET AUTOCOMMIT=0"
        })
    }

    /// `tx_commit_or_rollback`
    pub fn commit_or_rollback(&self, commit: bool, flags: u32, name: Option<&[u8]>) -> Result<()> {
        let opts = tx_cor_options(flags);
        let name = tx_name_comment(name);
        let q = format!(
            "{}{} {}",
            if commit { "COMMIT" } else { "ROLLBACK" },
            name,
            opts
        );
        self.query(q.as_bytes())
    }

    /// `COMMIT`
    pub fn commit(&self, flags: u32, name: Option<&[u8]>) -> Result<()> {
        self.commit_or_rollback(true, flags, name)
    }

    /// `ROLLBACK`
    pub fn rollback(&self, flags: u32, name: Option<&[u8]>) -> Result<()> {
        self.commit_or_rollback(false, flags, name)
    }

    /// `tx_begin`: `START TRANSACTION[ /*name*/] [options]`.
    pub fn begin_transaction(&self, mode: u32, name: Option<&[u8]>) -> Result<()> {
        let mut parts: Vec<&str> = Vec::new();
        if mode & TRANS_START_WITH_CONSISTENT_SNAPSHOT != 0 {
            parts.push("WITH CONSISTENT SNAPSHOT");
        }
        if mode & TRANS_START_READ_WRITE != 0 {
            parts.push("READ WRITE");
        } else if mode & TRANS_START_READ_ONLY != 0 {
            parts.push("READ ONLY");
        }
        let q = format!(
            "START TRANSACTION{} {}",
            tx_name_comment(name),
            parts.join(", ")
        );
        let r = self.query(q.as_bytes());
        if r.is_err()
            && mode & (TRANS_START_READ_WRITE | TRANS_START_READ_ONLY) != 0
            && self.errno() == 1064
        {
            self.data().error_info.set_client(
                CR_NOT_IMPLEMENTED,
                "This server version doesn't support 'READ WRITE' and 'READ ONLY'. Minimum 5.6.5 is required",
            );
            return self.fail();
        }
        r
    }

    /// ``SAVEPOINT `name` ``
    pub fn savepoint(&self, name: Option<&[u8]>) -> Result<()> {
        self.savepoint_cmd("SAVEPOINT", name)
    }

    /// ``RELEASE SAVEPOINT `name` ``
    pub fn release_savepoint(&self, name: Option<&[u8]>) -> Result<()> {
        self.savepoint_cmd("RELEASE SAVEPOINT", name)
    }

    fn savepoint_cmd(&self, verb: &str, name: Option<&[u8]>) -> Result<()> {
        let Some(name) = name else {
            self.data()
                .error_info
                .set_client(CR_UNKNOWN_ERROR, "Savepoint name not provided");
            return self.fail();
        };
        let mut q = format!("{verb} `").into_bytes();
        q.extend_from_slice(name);
        q.push(b'`');
        self.query(&q)
    }

    // ------------------------------------------------------------ queries

    /// `mysqlnd_conn_data::query`: sends `sql` and reads the first response.
    /// When it starts a result set, call [`Connection::store_result`] or
    /// [`Connection::use_result`].
    pub fn query(&self, sql: &[u8]) -> Result<()> {
        let ok = self.data().query(sql);
        self.check(ok)
    }

    /// `mysqlnd_conn_data::send_query` (async half 1).
    pub fn send_query(&self, sql: &[u8]) -> Result<()> {
        let ok = self.data().send_query(sql);
        self.check(ok)
    }

    /// `mysqlnd_conn_data::reap_query` (async half 2).
    pub fn reap_query(&self) -> Result<()> {
        let ok = {
            let mut c = self.data();
            let ok = c.reap_query();
            if ok && c.last_query_type == QueryType::Upsert && c.upsert.affected_rows != 0 {
                let a = c.upsert.affected_rows;
                c.inc(Stat::RowsAffectedNormal, a);
            }
            ok
        };
        self.check(ok)
    }

    /// `mysqlnd_conn_data::store_result`: buffers the pending result set.
    /// `Ok(None)` when there is no pending result set.
    pub fn store_result(&self) -> Result<Option<ResultSet>> {
        let meta = {
            let mut c = self.data();
            let Some(meta) = c.current_result.clone() else {
                return Ok(None);
            };
            if c.last_query_type != QueryType::Select || c.state != ConnState::FetchingData {
                c.error_info
                    .set_client(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
                return self.fail();
            }
            c.inc(Stat::BufferedSets, 1);
            c.current_result = None;
            meta
        };
        let rows = self.data().fetch_all_rows(false);
        match rows {
            Ok(rows) => {
                let n = rows.len() as u64;
                self.data().upsert.affected_rows = n;
                Ok(Some(ResultSet::buffered(
                    Some(self.clone()),
                    meta,
                    rows,
                    false,
                )))
            }
            Err(e) => {
                let mut c = self.data();
                c.error_info.set(e.errno, &e.sqlstate, &e.error);
                drop(c);
                self.fail()
            }
        }
    }

    /// `mysqlnd_conn_data::use_result`: streams the pending result set.
    pub fn use_result(&self) -> Result<Option<ResultSet>> {
        let meta = {
            let mut c = self.data();
            let Some(meta) = c.current_result.clone() else {
                return Ok(None);
            };
            if c.last_query_type != QueryType::Select || c.state != ConnState::FetchingData {
                c.error_info
                    .set_client(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
                return self.fail();
            }
            c.inc(Stat::UnbufferedSets, 1);
            c.error_info.reset();
            c.current_result = None;
            meta
        };
        Ok(Some(ResultSet::unbuffered(self.clone(), meta, None)))
    }

    /// Whether more results are pending (`SERVER_MORE_RESULTS_EXISTS`).
    #[must_use]
    pub fn more_results(&self) -> bool {
        self.data().upsert.server_status & SERVER_MORE_RESULTS_EXISTS != 0
    }

    /// `mysqlnd_conn_data::next_result`: reads the next result header.
    /// `Ok(false)` when nothing is pending (no error recorded).
    pub fn next_result(&self) -> Result<bool> {
        let mut c = self.data();
        if c.state != ConnState::NextResultPending {
            c.error_info.reset();
            return Ok(false);
        }
        let ok = c.next_result();
        drop(c);
        if ok {
            Ok(true)
        } else {
            self.fail()
        }
    }

    /// `mysqlnd_stmt_init`
    #[must_use]
    pub fn stmt_init(&self) -> Statement {
        Statement::new(self.clone())
    }

    // ------------------------------------------------------------ accessors

    /// Records an error on the connection (`SET_CLIENT_ERROR`; `errno == 0`
    /// clears it). Lets the binding mirror errors raised elsewhere, e.g. a
    /// statement's close resetting the connection error.
    pub fn set_error(&self, errno: u32, sqlstate: &str, message: &[u8]) {
        self.data().error_info.set(errno, sqlstate, message);
    }

    /// Snapshot of the full error state (errno, SQLSTATE, message, list).
    #[must_use]
    pub fn error_info(&self) -> ErrorInfo {
        self.data().error_info.clone()
    }

    /// Restores an error state saved with [`Connection::error_info`].
    pub fn restore_error(&self, info: ErrorInfo) {
        self.data().error_info = info;
    }

    /// Overwrites the affected-rows value (`u64::MAX` = -1).
    pub fn set_affected_rows(&self, rows: u64) {
        self.data().upsert.affected_rows = rows;
    }

    /// Snapshot of the upsert status (affected rows, insert id, warnings,
    /// server status).
    #[must_use]
    pub fn upsert_status(&self) -> UpsertStatus {
        self.data().upsert
    }

    /// Restores an upsert status saved with [`Connection::upsert_status`].
    pub fn restore_upsert_status(&self, u: UpsertStatus) {
        self.data().upsert = u;
    }

    /// `MYSQLI_DISABLE_MQ`: sends `COM_SET_OPTION(MULTI_STATEMENTS_OFF)` right
    /// away. Failures (e.g. results still pending) are ignored and leave the
    /// error state untouched. Returns whether the option was applied.
    pub fn exit_multi_statements(&self) -> bool {
        let saved = self.error_info();
        let ok = self.set_server_option(OPTION_MULTI_STATEMENTS_OFF).is_ok();
        if !ok {
            self.restore_error(saved);
        }
        ok
    }

    /// `mysqlnd_errno`
    #[must_use]
    pub fn errno(&self) -> u32 {
        self.data().error_info.errno
    }

    /// `mysqlnd_error`
    #[must_use]
    pub fn error(&self) -> Vec<u8> {
        self.data().error_info.error.clone()
    }

    /// `mysqlnd_sqlstate`
    #[must_use]
    pub fn sqlstate(&self) -> String {
        self.data().error_info.sqlstate_or_null().to_string()
    }

    /// `mysqli_error_list`
    #[must_use]
    pub fn error_list(&self) -> Vec<ErrorEntry> {
        self.data().error_info.list.clone()
    }

    /// The current error as an [`Error`].
    #[must_use]
    pub fn last_error(&self) -> Error {
        self.data().error_info.to_error()
    }

    /// `mysqlnd_field_count`
    #[must_use]
    pub fn field_count(&self) -> u32 {
        self.data().field_count
    }

    /// `mysqlnd_affected_rows` (`u64::MAX` = -1).
    #[must_use]
    pub fn affected_rows(&self) -> u64 {
        self.data().upsert.affected_rows
    }

    /// `mysqlnd_insert_id`
    #[must_use]
    pub fn insert_id(&self) -> u64 {
        self.data().upsert.last_insert_id
    }

    /// `mysqlnd_warning_count`
    #[must_use]
    pub fn warning_count(&self) -> u32 {
        self.data().upsert.warning_count
    }

    /// `mysqlnd_server_status`
    #[must_use]
    pub fn server_status(&self) -> u16 {
        self.data().upsert.server_status
    }

    /// `mysqlnd_info`: the info message of the last statement.
    #[must_use]
    pub fn info(&self) -> Option<Vec<u8>> {
        self.data().last_message.clone()
    }

    /// `mysqlnd_get_server_info`
    #[must_use]
    pub fn server_info(&self) -> Option<String> {
        self.data().server_version.clone()
    }

    /// `mysqlnd_get_server_version` (`major*10000 + minor*100 + patch`).
    #[must_use]
    pub fn server_version(&self) -> u64 {
        self.data().server_version_number()
    }

    /// `mysqlnd_get_host_info`
    #[must_use]
    pub fn host_info(&self) -> Option<String> {
        self.data().host_info.clone()
    }

    /// `mysqlnd_get_proto_info`
    #[must_use]
    pub fn proto_info(&self) -> u32 {
        u32::from(self.data().protocol_version)
    }

    /// `mysqlnd_thread_id`
    #[must_use]
    pub fn thread_id(&self) -> u64 {
        self.data().thread_id
    }

    /// The active character set (`conn->charset`).
    #[must_use]
    pub fn charset(&self) -> Option<&'static Charset> {
        self.data().charset
    }

    /// `mysqlnd_character_set_name`
    #[must_use]
    pub fn character_set_name(&self) -> Option<&'static str> {
        self.data().charset.map(|c| c.name)
    }

    /// The server's capability flags (0 when not connected).
    #[must_use]
    pub fn server_capabilities(&self) -> u32 {
        self.data().server_capabilities
    }

    /// The capability flags this client sent.
    #[must_use]
    pub fn client_flags(&self) -> u32 {
        self.data().client_flags
    }

    /// Whether TLS is active.
    #[must_use]
    pub fn is_ssl(&self) -> bool {
        self.data().net.ssl
    }

    /// The transport string (`tcp://host:port` / `unix://path`).
    #[must_use]
    pub fn scheme(&self) -> Option<String> {
        self.data().scheme.clone()
    }

    /// The auth plugin used by the last authentication.
    #[must_use]
    pub fn auth_protocol(&self) -> Option<String> {
        self.data().options.auth_protocol.clone()
    }

    /// Whether `MYSQLI_OPT_INT_AND_FLOAT_NATIVE` is on.
    #[must_use]
    pub fn int_and_float_native(&self) -> bool {
        self.data().options.int_and_float_native
    }

    /// `mysqli_get_connection_stats()`
    #[must_use]
    pub fn stats(&self) -> Stats {
        self.data().stats.clone()
    }

    /// Whether this is a persistent connection.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        self.data().persistent
    }
}

impl ConnData {
    /// Crate-visible wrapper of `send_command_handle_response`.
    pub(crate) fn handle_response_pub(
        &mut self,
        eof: bool,
        silent: bool,
        cmd: Command,
        ignore_upsert: bool,
    ) -> bool {
        self.handle_response(eof, silent, cmd, ignore_upsert)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_building() {
        let mut s = None;
        assert_eq!(
            ConnData::get_scheme("localhost", &mut s, 0),
            ("unix:///tmp/mysql.sock".to_string(), true)
        );
        assert_eq!(s.as_deref(), Some("/tmp/mysql.sock"));
        let mut s = Some("/run/x.sock".to_string());
        assert_eq!(
            ConnData::get_scheme("LocalHost", &mut s, 0).0,
            "unix:///run/x.sock"
        );
        let mut s = None;
        assert_eq!(
            ConnData::get_scheme("127.0.0.1", &mut s, 0).0,
            "tcp://127.0.0.1:3306"
        );
        assert_eq!(
            ConnData::get_scheme("127.0.0.1:3307", &mut s, 5).0,
            "tcp://127.0.0.1:3307"
        );
        assert_eq!(
            ConnData::get_scheme("::1", &mut s, 3306).0,
            "tcp://[::1]:3306"
        );
        assert_eq!(
            ConnData::get_scheme("[::1]:3310", &mut s, 3306).0,
            "tcp://[::1]:3310"
        );
        assert_eq!(
            ConnData::get_scheme("[::1]", &mut s, 3306).0,
            "tcp://[::1]:3306"
        );
    }

    #[test]
    fn version_number() {
        let mut d = ConnData::new(false);
        d.server_version = Some("10.11.14-MariaDB-0ubuntu0.24.04.1".into());
        assert_eq!(d.server_version_number(), 101_114);
        d.server_version = Some("8.0.36".into());
        assert_eq!(d.server_version_number(), 80036);
    }

    #[test]
    fn tx_strings() {
        assert_eq!(
            tx_cor_options(TRANS_COR_AND_CHAIN | TRANS_COR_NO_RELEASE),
            "AND CHAIN NO RELEASE"
        );
        assert_eq!(
            tx_cor_options(TRANS_COR_AND_CHAIN | TRANS_COR_AND_NO_CHAIN),
            ""
        );
        let _ = diag::take();
        assert_eq!(tx_name_comment(Some(b"my tx=1")), " /*my tx=1*/");
        assert_eq!(tx_name_comment(Some(b"a*/b")), " /*ab*/");
        assert_eq!(diag::take().len(), 1);
        assert_eq!(tx_name_comment(None), "");
    }

    #[test]
    fn options_roundtrip() {
        use crate::consts::opt::*;
        let c = Connection::init();
        assert!(c.set_option(MYSQL_OPT_LOCAL_INFILE, OptionValue::Int(1)));
        assert!(c.data().options.flags & CLIENT_LOCAL_FILES != 0);
        assert!(!c.set_option(MYSQL_SET_CHARSET_NAME, OptionValue::Str(b"nosuch")));
        assert_eq!(c.errno(), CR_CANT_FIND_CHARSET);
        assert!(c.set_option(MYSQL_SET_CHARSET_NAME, OptionValue::Str(b"latin1")));
        assert!(!c.set_option(MYSQLND_OPT_NET_CMD_BUFFER_SIZE, OptionValue::Int(10)));
        assert!(!c.set_option(9999, OptionValue::Int(1)));
        c.connect_attr_add("a", "1");
        c.connect_attr_add("a", "2");
        assert_eq!(c.connect_attrs(), vec![("a".to_string(), "2".to_string())]);
        assert!(c.set_option(MYSQL_OPT_CONNECT_ATTR_DELETE, OptionValue::Str(b"a")));
        assert!(c.connect_attrs().is_empty());
    }

    #[test]
    fn commands_on_unconnected_handle() {
        let c = Connection::init();
        let e = c.query(b"SELECT 1").unwrap_err();
        assert_eq!(e.errno, CR_COMMANDS_OUT_OF_SYNC);
        assert_eq!(c.affected_rows(), 0);
    }
}

/// Outcome of [`poll`] (`mysqlnd_poll`), as indices into the input slices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollResult {
    /// Connections from `read` with a result ready to be reaped.
    pub read: Vec<usize>,
    /// Connections from `error` reported in error (never set, like `select()`).
    pub error: Vec<usize>,
    /// Connections from `read` that have no pending query (`$reject`).
    pub dont_poll: Vec<usize>,
    /// Number of ready descriptors.
    pub ready: usize,
}

/// `mysqlnd_poll` (`mysqli_poll`): waits up to `sec`/`usec` for any of the
/// `read` connections to have an async result. `None` on failure (a warning
/// has been queued).
#[must_use]
pub fn poll(read: &[Connection], error: &[Connection], sec: i64, usec: i64) -> Option<PollResult> {
    if sec < 0 || usec < 0 {
        diag::warn("Negative values passed for sec and/or usec");
        return None;
    }
    let mut res = PollResult::default();
    let mut candidates = Vec::new();
    for (i, c) in read.iter().enumerate() {
        let st = c.state();
        if st <= ConnState::Ready || st == ConnState::QuitSent {
            res.dont_poll.push(i);
        } else if c.data().net.is_open() {
            candidates.push(i);
        }
    }
    let err_open = error.iter().filter(|c| c.data().net.is_open()).count();
    if candidates.is_empty() && err_open == 0 {
        diag::warn(if res.dont_poll.is_empty() {
            "No stream arrays were passed"
        } else {
            "All arrays passed are clear"
        });
        return None;
    }
    #[allow(clippy::cast_sign_loss)]
    let timeout = Duration::from_secs(sec as u64) + Duration::from_micros(usec as u64);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        res.read = candidates
            .iter()
            .copied()
            .filter(|&i| read[i].data().net.readable())
            .collect();
        if !res.read.is_empty() || std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(
            Duration::from_millis(1)
                .min(deadline.saturating_duration_since(std::time::Instant::now())),
        );
    }
    res.ready = res.read.len();
    Some(res)
}

/// A registry of idle persistent connections keyed by their connect
/// parameters (the `p:` host prefix of mysqli). The binding parks a
/// connection with [`pool_put`] at request end and reuses it with
/// [`pool_take`] (which calls `restart_psession`).
pub mod pool {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::Connection;

    thread_local! {
        static POOL: RefCell<HashMap<String, Vec<Connection>>> = RefCell::new(HashMap::new());
    }

    /// Takes an idle connection for `key`, if any.
    #[must_use]
    pub fn pool_take(key: &str) -> Option<Connection> {
        let c = POOL.with(|p| p.borrow_mut().get_mut(key).and_then(Vec::pop))?;
        c.restart_psession();
        Some(c)
    }

    /// Parks `c` for reuse (after `end_psession`).
    pub fn pool_put(key: &str, c: Connection) {
        c.end_psession();
        POOL.with(|p| p.borrow_mut().entry(key.to_string()).or_default().push(c));
    }

    /// Number of idle connections for `key`.
    #[must_use]
    pub fn pool_len(key: &str) -> usize {
        POOL.with(|p| p.borrow().get(key).map_or(0, Vec::len))
    }

    /// Drops every idle connection.
    pub fn pool_clear() {
        POOL.with(|p| p.borrow_mut().clear());
    }
}
