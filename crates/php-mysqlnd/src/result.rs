// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Result sets (ref: `mysqlnd_result.c`, `mysqlnd_result_meta.c`):
//! buffered (`store_result`) and unbuffered (`use_result`) sets over the
//! text or binary protocol, row fetching into PHP values, field metadata
//! navigation and the mysqlnd fetch statistics.

use std::rc::Rc;

use php_runtime::{ArrayKey, HashTable, Value, ZArray};

use crate::codec;
use crate::conn::{ConnState, Connection};
use crate::consts::{
    CR_COMMANDS_OUT_OF_SYNC, FETCH_ASSOC, FETCH_NUM, MSG_OUT_OF_SYNC, SERVER_MORE_RESULTS_EXISTS,
};
use crate::error::{Error, Result};
use crate::protocol::{Field, RowPacket};
use crate::stats::{inc_global, Stat, Stats};

/// Result-set metadata (`MYSQLND_RES_METADATA`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Column definitions.
    pub fields: Vec<Field>,
    /// Hash keys for associative fetches (numeric names become int keys).
    pub keys: Vec<ArrayKey>,
}

impl Metadata {
    /// Builds metadata, precomputing the fetch keys (`ZEND_HANDLE_NUMERIC`).
    #[must_use]
    pub fn new(fields: Vec<Field>) -> Self {
        let keys = fields
            .iter()
            .map(|f| ArrayKey::from_bytes(&f.name))
            .collect();
        Self { fields, keys }
    }
}

#[derive(Debug)]
enum Data {
    /// Metadata only (`mysqli_stmt_result_metadata`, freed sets).
    None,
    /// `store_result`: raw row packets, decoded lazily.
    Buffered { rows: Vec<Vec<u8>>, current: u64 },
    /// `use_result`: rows streamed from the connection.
    Unbuffered {
        eof: bool,
        count: u64,
        has_row: bool,
    },
}

/// A result set (`MYSQLND_RES`).
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct ResultSet {
    conn: Option<Connection>,
    meta: Rc<Metadata>,
    current_field: u32,
    data: Data,
    binary: bool,
    ps: bool,
    lengths: Vec<usize>,
    prev_row_size: usize,
    freed: bool,
    /// Owned by a statement: its lifecycle statistics are handled there.
    pub(crate) stmt_owned: bool,
    /// Rows of an unbuffered PS set come through `COM_STMT_FETCH`.
    pub(crate) cursor_stmt_id: Option<u32>,
}

impl Drop for ResultSet {
    fn drop(&mut self) {
        if !self.freed && !self.stmt_owned {
            self.free(false);
        }
    }
}

impl ResultSet {
    pub(crate) fn buffered(
        conn: Option<Connection>,
        meta: Rc<Metadata>,
        rows: Vec<Vec<u8>>,
        binary: bool,
    ) -> Self {
        let n = meta.fields.len();
        Self {
            conn,
            meta,
            current_field: 0,
            data: Data::Buffered { rows, current: 0 },
            binary,
            ps: binary,
            lengths: vec![0; n],
            prev_row_size: 0,
            freed: false,
            stmt_owned: false,
            cursor_stmt_id: None,
        }
    }

    pub(crate) fn unbuffered(conn: Connection, meta: Rc<Metadata>, ps: Option<bool>) -> Self {
        let n = meta.fields.len();
        Self {
            conn: Some(conn),
            meta,
            current_field: 0,
            data: Data::Unbuffered {
                eof: false,
                count: 0,
                has_row: false,
            },
            binary: ps.is_some(),
            ps: ps.is_some(),
            lengths: vec![0; n],
            prev_row_size: 0,
            freed: false,
            stmt_owned: false,
            cursor_stmt_id: None,
        }
    }

    pub(crate) fn metadata_only(conn: Option<Connection>, meta: Rc<Metadata>) -> Self {
        let n = meta.fields.len();
        Self {
            conn,
            meta,
            current_field: 0,
            data: Data::Unbuffered {
                eof: true,
                count: 0,
                has_row: false,
            },
            binary: true,
            ps: true,
            lengths: vec![0; n],
            prev_row_size: 0,
            freed: false,
            stmt_owned: false,
            cursor_stmt_id: None,
        }
    }

    /// The metadata.
    #[must_use]
    pub fn metadata(&self) -> &Rc<Metadata> {
        &self.meta
    }

    /// The connection this set belongs to.
    #[must_use]
    pub fn connection(&self) -> Option<&Connection> {
        self.conn.as_ref()
    }

    /// Whether the rows are buffered client-side.
    #[must_use]
    pub fn is_buffered(&self) -> bool {
        matches!(self.data, Data::Buffered { .. })
    }

    /// Whether this set was explicitly freed.
    #[must_use]
    pub fn is_freed(&self) -> bool {
        self.freed
    }

    /// Whether rows use the binary (prepared statement) protocol.
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.binary
    }

    /// `mysqlnd_num_fields`
    #[must_use]
    pub fn num_fields(&self) -> u32 {
        u32::try_from(self.meta.fields.len()).unwrap_or(u32::MAX)
    }

    /// `mysqlnd_num_rows`: buffered row count; for unbuffered sets 0 until
    /// every row has been read (libmysql compatible).
    #[must_use]
    pub fn num_rows(&self) -> u64 {
        match &self.data {
            Data::Buffered { rows, .. } => rows.len() as u64,
            Data::Unbuffered { eof, count, .. } => {
                if *eof {
                    *count
                } else {
                    0
                }
            }
            Data::None => 0,
        }
    }

    /// Whether every row has been read (unbuffered) / always true when buffered.
    #[must_use]
    pub fn eof(&self) -> bool {
        match &self.data {
            Data::Unbuffered { eof, .. } => *eof,
            _ => true,
        }
    }

    // ------------------------------------------------------------ fields

    /// `mysqlnd_fetch_field`: the next field, advancing the field cursor.
    pub fn fetch_field(&mut self) -> Option<&Field> {
        let i = self.current_field as usize;
        if i >= self.meta.fields.len() {
            return None;
        }
        self.current_field += 1;
        self.meta.fields.get(i)
    }

    /// `mysqlnd_fetch_field_direct`
    #[must_use]
    pub fn fetch_field_direct(&self, n: u32) -> Option<&Field> {
        self.meta.fields.get(n as usize)
    }

    /// `mysqlnd_fetch_fields`
    #[must_use]
    pub fn fetch_fields(&self) -> &[Field] {
        &self.meta.fields
    }

    /// `mysqlnd_field_seek`: returns the previous offset.
    pub fn field_seek(&mut self, offset: u32) -> u32 {
        std::mem::replace(&mut self.current_field, offset)
    }

    /// `mysqlnd_field_tell`
    #[must_use]
    pub fn field_tell(&self) -> u32 {
        self.current_field
    }

    // ------------------------------------------------------------ rows

    /// `mysqlnd_data_seek` (buffered sets only).
    pub fn data_seek(&mut self, row: u64) -> bool {
        match &mut self.data {
            Data::Buffered { rows, current } => {
                let n = rows.len() as u64;
                *current = if row >= n { n } else { row };
                true
            }
            _ => false,
        }
    }

    /// `mysqlnd_fetch_lengths` (text-protocol sets only).
    #[must_use]
    pub fn fetch_lengths(&self) -> Option<Vec<usize>> {
        if self.binary {
            return None;
        }
        match &self.data {
            Data::Buffered { rows, current } => {
                if *current > rows.len() as u64 || *current == 0 {
                    None
                } else {
                    Some(self.lengths.clone())
                }
            }
            Data::Unbuffered { eof, has_row, .. } => {
                if *has_row || *eof {
                    Some(self.lengths.clone())
                } else {
                    None
                }
            }
            Data::None => None,
        }
    }

    fn decode(&mut self, raw: &[u8], stats: &mut Stats, native: bool) -> Option<Vec<Value>> {
        let row = if self.binary {
            codec::decode_binary_row(raw, &self.meta.fields, stats)
        } else {
            codec::decode_text_row(raw, &self.meta.fields, native, stats)
        }?;
        if !self.binary {
            for (l, v) in self.lengths.iter_mut().zip(row.iter()) {
                *l = match v {
                    Value::Str(s) => s.len(),
                    _ => 0,
                };
            }
        }
        Some(row)
    }

    /// `mysqlnd_res::fetch_row`: the next row as values, `Ok(None)` at the
    /// end. Errors are recorded on the connection.
    pub fn fetch_row_values(&mut self) -> Result<Option<Vec<Value>>> {
        self.fetch_inner(true)
    }

    /// `fetch_row` with `row_ptr == NULL`: advances without decoding (no
    /// per-type statistics), as `skip_result` does.
    fn fetch_inner(&mut self, decode: bool) -> Result<Option<Vec<Value>>> {
        if self.cursor_stmt_id.is_some() {
            return Err(Error {
                errno: 0,
                sqlstate: String::new(),
                message: Vec::new(),
            });
        }
        match &mut self.data {
            Data::None => Ok(None),
            Data::Buffered { rows, current } => {
                if *current < rows.len() as u64 && !decode {
                    *current += 1;
                    inc_global(
                        if self.ps {
                            Stat::RowsFetchedFromClientPsBuf
                        } else {
                            Stat::RowsFetchedFromClientNormalBuf
                        },
                        1,
                    );
                    return Ok(Some(Vec::new()));
                }
                if *current < rows.len() as u64 {
                    #[allow(clippy::cast_possible_truncation)]
                    let raw = std::mem::take(&mut rows[*current as usize]);
                    let conn = self.conn.clone();
                    let (native, mut stats) = match &conn {
                        Some(c) => {
                            let mut d = c.data();
                            let native = d.options.int_and_float_native;
                            (native, std::mem::take(&mut d.stats))
                        }
                        None => (false, Stats::default()),
                    };
                    let decoded = self.decode(&raw, &mut stats, native);
                    if let Some(c) = &conn {
                        c.data().stats = stats;
                    }
                    if let Data::Buffered { rows, current } = &mut self.data {
                        #[allow(clippy::cast_possible_truncation)]
                        {
                            rows[*current as usize] = raw;
                        }
                        let Some(row) = decoded else {
                            return Err(Error {
                                errno: 0,
                                sqlstate: String::new(),
                                message: Vec::new(),
                            });
                        };
                        *current += 1;
                        inc_global(
                            if self.ps {
                                Stat::RowsFetchedFromClientPsBuf
                            } else {
                                Stat::RowsFetchedFromClientNormalBuf
                            },
                            1,
                        );
                        return Ok(Some(row));
                    }
                    Ok(None)
                } else {
                    if *current == rows.len() as u64 {
                        *current = rows.len() as u64 + 1;
                    }
                    Ok(None)
                }
            }
            Data::Unbuffered { eof, .. } => {
                if *eof {
                    return Ok(None);
                }
                self.fetch_unbuffered(decode)
            }
        }
    }

    fn fetch_unbuffered(&mut self, decode: bool) -> Result<Option<Vec<Value>>> {
        let Some(conn) = self.conn.clone() else {
            return Ok(None);
        };
        let mut c = conn.data();
        if c.state != ConnState::FetchingData {
            c.error_info
                .set_client(CR_COMMANDS_OUT_OF_SYNC, MSG_OUT_OF_SYNC);
            return Err(c.error_info.to_error());
        }
        let binary = self.binary;
        let mut prev = self.prev_row_size;
        let pkt = c.read_row(binary, &mut prev);
        self.prev_row_size = prev;
        match pkt {
            // Past `memory_limit`: no row; the rest of the set stays unread
            // (dropping the result drains it) and the VM raises the fatal
            // as the call returns.
            RowPacket::OverLimit => Ok(None),
            RowPacket::Row(raw) => {
                c.inc(
                    if self.ps {
                        Stat::RowsFetchedFromClientPsUnbuf
                    } else {
                        Stat::RowsFetchedFromClientNormalUnbuf
                    },
                    1,
                );
                if let Data::Unbuffered { count, has_row, .. } = &mut self.data {
                    *count += 1;
                    *has_row = true;
                }
                if !decode {
                    return Ok(Some(Vec::new()));
                }
                let native = c.options.int_and_float_native;
                let mut stats = std::mem::take(&mut c.stats);
                let decoded = self.decode(&raw, &mut stats, native);
                c.stats = stats;
                match decoded {
                    Some(r) => Ok(Some(r)),
                    None => Err(Error {
                        errno: 0,
                        sqlstate: String::new(),
                        message: Vec::new(),
                    }),
                }
            }
            RowPacket::Err(e) => {
                e.apply(&mut c.error_info);
                if c.state != ConnState::QuitSent {
                    c.state = ConnState::Ready;
                }
                if let Data::Unbuffered { eof, .. } = &mut self.data {
                    *eof = true;
                }
                Err(c.error_info.to_error())
            }
            RowPacket::Eof(e) => {
                if let Data::Unbuffered { eof, .. } = &mut self.data {
                    *eof = true;
                }
                c.upsert.reset();
                c.upsert.warning_count = u32::from(e.warning_count);
                c.upsert.server_status = e.server_status;
                c.state = if e.server_status & SERVER_MORE_RESULTS_EXISTS != 0 {
                    ConnState::NextResultPending
                } else {
                    ConnState::Ready
                };
                Ok(None)
            }
        }
    }

    /// Builds the row array exactly like `mysqlnd_res::fetch_into`
    /// (`FETCH_NUM` uses add-if-absent, `FETCH_ASSOC` overwrites).
    #[must_use]
    pub fn row_to_array(&self, row: Vec<Value>, flags: u32) -> ZArray {
        let mut size = row.len();
        if flags & (FETCH_NUM | FETCH_ASSOC) == (FETCH_NUM | FETCH_ASSOC) {
            size *= 2;
        }
        let mut ht = HashTable::with_capacity(size);
        for (i, v) in row.into_iter().enumerate() {
            if flags & FETCH_NUM != 0 {
                let _ = ht.add(
                    ArrayKey::Int(i64::try_from(i).unwrap_or(i64::MAX)),
                    v.clone(),
                );
            }
            if flags & FETCH_ASSOC != 0 {
                ht.insert(self.meta.keys[i].clone(), v);
            }
        }
        ZArray::from_table(ht)
    }

    /// `mysqlnd_fetch_into`: `Ok(None)` when there are no more rows.
    pub fn fetch_array(&mut self, flags: u32) -> Result<Option<ZArray>> {
        if let Some(c) = &self.conn {
            c.data().error_info.reset();
        }
        match self.fetch_row_values()? {
            Some(row) => Ok(Some(self.row_to_array(row, flags))),
            None => Ok(None),
        }
    }

    /// `mysqli_fetch_row`
    pub fn fetch_row(&mut self) -> Result<Option<ZArray>> {
        self.fetch_array(FETCH_NUM)
    }

    /// `mysqli_fetch_assoc`
    pub fn fetch_assoc(&mut self) -> Result<Option<ZArray>> {
        self.fetch_array(FETCH_ASSOC)
    }

    /// `mysqli_fetch_all`: every remaining row; stops at `memory_limit`,
    /// leaving the violation pending for the VM.
    pub fn fetch_all(&mut self, flags: u32) -> ZArray {
        use php_memlimit::TryGrow;
        let mut out = Vec::new();
        while let Ok(Some(r)) = self.fetch_array(flags) {
            if php_memlimit::poll().is_err() || out.try_grow(1).is_err() {
                break;
            }
            out.push(Value::Array(r));
        }
        ZArray::from_values(out)
    }

    /// `mysqli_fetch_column`: column `n` of the next row.
    pub fn fetch_column(&mut self, n: usize) -> Result<Option<Value>> {
        Ok(self.fetch_row_values()?.map(|mut r| {
            if n < r.len() {
                r.swap_remove(n)
            } else {
                Value::Null
            }
        }))
    }

    /// `mysqlnd_res::skip_result`: drains an unbuffered set.
    pub fn skip_result(&mut self) {
        if let Data::Unbuffered { eof: false, .. } = self.data {
            let Some(conn) = self.conn.clone() else {
                return;
            };
            conn.data().inc(
                if self.ps {
                    Stat::FlushedPsSets
                } else {
                    Stat::FlushedNormalSets
                },
                1,
            );
            while let Ok(Some(_)) = self.fetch_inner(false) {
                conn.data().inc(
                    if self.ps {
                        Stat::RowsSkippedPs
                    } else {
                        Stat::RowsSkippedNormal
                    },
                    1,
                );
            }
        }
    }

    /// `mysqlnd_free_result`: drains unbuffered rows and releases the data.
    pub fn free(&mut self, implicit: bool) {
        if self.freed {
            return;
        }
        if let Some(c) = &self.conn {
            c.data().inc(
                if implicit {
                    Stat::FreeResultImplicit
                } else {
                    Stat::FreeResultExplicit
                },
                1,
            );
        } else {
            inc_global(
                if implicit {
                    Stat::FreeResultImplicit
                } else {
                    Stat::FreeResultExplicit
                },
                1,
            );
        }
        self.skip_result();
        self.data = Data::None;
        self.freed = true;
        self.conn = None;
    }

    /// `free_result_buffers` without statistics (statement-owned sets).
    pub(crate) fn release_buffers(&mut self) {
        self.skip_result();
        self.data = Data::None;
    }

    /// Unbuffered-set bookkeeping for cursor fetches driven by a statement.
    #[allow(clippy::option_option)]
    pub(crate) fn cursor_row(
        &mut self,
        raw: Option<&[u8]>,
        conn: &Connection,
    ) -> Option<Option<Vec<Value>>> {
        if let Some(raw) = raw {
            let mut stats = std::mem::take(&mut conn.data().stats);
            let r = codec::decode_binary_row(raw, &self.meta.fields, &mut stats);
            conn.data().stats = stats;
            if let Data::Unbuffered { count, has_row, .. } = &mut self.data {
                *count += 1;
                *has_row = true;
            }
            r.map(Some)
        } else {
            if let Data::Unbuffered { eof, .. } = &mut self.data {
                *eof = true;
            }
            Some(None)
        }
    }

    pub(crate) fn mark_eof(&mut self) {
        if let Data::Unbuffered { eof, .. } = &mut self.data {
            *eof = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::*;

    fn meta(names: &[&str]) -> Rc<Metadata> {
        Rc::new(Metadata::new(
            names
                .iter()
                .map(|n| Field {
                    name: n.as_bytes().to_vec(),
                    type_: MYSQL_TYPE_VAR_STRING,
                    ..Field::default()
                })
                .collect(),
        ))
    }

    fn row(vals: &[&str]) -> Vec<u8> {
        let mut b = Vec::new();
        for v in vals {
            crate::wire::put_lenenc_str(&mut b, v.as_bytes());
        }
        b
    }

    #[test]
    fn buffered_fetch_and_seek() {
        let mut r = ResultSet::buffered(
            None,
            meta(&["a", "b"]),
            vec![row(&["1", "x"]), row(&["2", "y"])],
            false,
        );
        assert_eq!(r.num_rows(), 2);
        assert!(r.fetch_lengths().is_none());
        let a = r.fetch_assoc().unwrap().unwrap();
        assert_eq!(crate::render(a.get_str(b"a").unwrap()), "s:1");
        assert_eq!(r.fetch_lengths(), Some(vec![1, 1]));
        let n = r.fetch_row().unwrap().unwrap();
        assert_eq!(crate::render(n.get_int(1).unwrap()), "s:y");
        assert!(r.fetch_row().unwrap().is_none());
        assert!(r.fetch_lengths().is_none());
        assert!(r.data_seek(0));
        assert_eq!(crate::render(&r.fetch_column(1).unwrap().unwrap()), "s:x");
        assert!(r.data_seek(10));
        assert!(r.fetch_row().unwrap().is_none());
    }

    #[test]
    fn numeric_column_names() {
        let r = ResultSet::buffered(None, meta(&["1", "x", "01"]), vec![], false);
        let arr = r.row_to_array(
            vec![Value::string("a"), Value::string("b"), Value::string("c")],
            FETCH_BOTH,
        );
        let keys: Vec<_> = arr.keys().cloned().collect();
        assert_eq!(
            keys,
            vec![
                ArrayKey::Int(0),
                ArrayKey::Int(1),
                ArrayKey::from_bytes(b"x"),
                ArrayKey::Int(2),
                ArrayKey::from_bytes(b"01")
            ]
        );
        // assoc "1" overwrote index 1, then NUM add for index 1 kept it
        assert_eq!(crate::render(arr.get_int(1).unwrap()), "s:a");
    }

    #[test]
    fn field_navigation() {
        let mut r = ResultSet::buffered(None, meta(&["a", "b"]), vec![], false);
        assert_eq!(r.fetch_field().unwrap().name, b"a");
        assert_eq!(r.field_tell(), 1);
        assert_eq!(r.fetch_field().unwrap().name, b"b");
        assert!(r.fetch_field().is_none());
        assert_eq!(r.field_seek(0), 2);
        assert_eq!(r.fetch_field_direct(1).unwrap().name, b"b");
        assert!(r.fetch_field_direct(2).is_none());
    }
}
