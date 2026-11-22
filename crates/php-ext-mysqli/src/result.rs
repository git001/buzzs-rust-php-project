// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `mysqli_result` (ref: `mysqli_api.c` fetch functions, `mysqli_nonapi.c`
//! `mysqli_fetch_*`, `mysqli_result_iterator.c`).

use std::cell::RefCell;
use std::rc::Rc;

use php_mysqlnd::consts::{FETCH_ASSOC, FETCH_NUM};
use php_mysqlnd::{Field, ResultSet};
use php_runtime::{ArrayKey, Obj, Value, ZArray, ZStr};
use php_vm::class::cflags;
use php_vm::ext::{Args, NativeIterator};
use php_vm::object::{internal, set_internal};
use php_vm::{Vm, VmResult};

use crate::globals::{self, REPORT_ERROR, REPORT_INDEX};
use crate::objects::{fetch_link, fetch_result, ResultObj, ResultRes, Status};
use crate::report::{flush_diag, report_conn_error, report_error, report_index};
use crate::util::{bstr, long_or_string, pos, recv, std_object};

/// `MYSQLI_ASSOC`
pub const ASSOC: i64 = 1;
/// `MYSQLI_NUM`
pub const NUM: i64 = 2;
/// `MYSQLI_BOTH`
pub const BOTH: i64 = 3;

fn res(vm: &mut Vm, a: &Args) -> VmResult<(Obj, Rc<RefCell<ResultSet>>, usize)> {
    let (o, b) = recv(vm, a)?;
    let rs = fetch_result(vm, &o, Status::Valid)?;
    Ok((o, rs, b))
}

fn mysqlnd_flags(mode: i64) -> u32 {
    (if mode & NUM != 0 { FETCH_NUM } else { 0 })
        | (if mode & ASSOC != 0 { FETCH_ASSOC } else { 0 })
}

/// `php_mysqli_fetch_into_hash_aux`: the next row as an array, `null` at
/// the end, `false` on error (reported per the report mode).
pub fn fetch_into(vm: &mut Vm, rs: &Rc<RefCell<ResultSet>>, mode: i64) -> VmResult<Value> {
    let r = rs.borrow_mut().fetch_array(mysqlnd_flags(mode));
    flush_diag(vm)?;
    let v = match r {
        Ok(Some(arr)) => Value::Array(arr),
        Ok(None) => Value::Null,
        Err(_) => Value::Bool(false),
    };
    if globals::report_mode() & REPORT_ERROR != 0 {
        let conn = rs.borrow().connection().cloned();
        if let Some(c) = conn {
            let errno = c.errno();
            if errno != 0 {
                report_error(vm, &c.sqlstate(), errno, &c.error())?;
            }
        }
    }
    Ok(v)
}

/// `mysqli_result::__construct()`
pub fn construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let this = a.this_obj();
    let Value::Object(link) = a.at(0) else {
        return Ok(Value::Null);
    };
    let mode = a.int(1);
    let (_, c) = fetch_link(vm, &link, Status::Valid)?;
    let r = match mode {
        0 => c.store_result(),
        1 => c.use_result(),
        _ => {
            return Err(a.value_error(
                vm,
                2,
                "must be either MYSQLI_STORE_RESULT or MYSQLI_USE_RESULT",
            ));
        }
    };
    let Ok(Some(rs)) = r else {
        report_conn_error(vm, &c)?;
        return Ok(Value::Null);
    };
    flush_diag(vm)?;
    if globals::report_mode() & REPORT_INDEX != 0 {
        report_index(vm, b"from previous query", c.server_status())?;
    }
    set_internal(
        &this,
        ResultObj(Some(ResultRes {
            status: Status::Valid,
            rs: Rc::new(RefCell::new(rs)),
        })),
    );
    Ok(Value::Null)
}

/// `mysqli_fetch_array()`
pub fn fetch_array(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, b) = recv(vm, a)?;
    let mode = a.int(b);
    if !(ASSOC..=BOTH).contains(&mode) {
        return Err(a.value_error(
            vm,
            pos(a, 2),
            "must be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH",
        ));
    }
    let rs = fetch_result(vm, &o, Status::Valid)?;
    fetch_into(vm, &rs, mode)
}

/// `mysqli_fetch_assoc()`
pub fn fetch_assoc(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    fetch_into(vm, &rs, ASSOC)
}

/// `mysqli_fetch_row()`
pub fn fetch_row(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    fetch_into(vm, &rs, NUM)
}

/// `mysqli_fetch_column()`
pub fn fetch_column(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, b) = res(vm, a)?;
    let col = a.int(b);
    if col < 0 {
        return Err(a.value_error(vm, pos(a, 2), "must be greater than or equal to 0"));
    }
    if col >= i64::from(rs.borrow().num_fields()) {
        return Err(a.value_error(
            vm,
            pos(a, 2),
            "must be less than the number of fields for this result set",
        ));
    }
    let row = fetch_into(vm, &rs, NUM)?;
    match row {
        Value::Array(arr) => Ok(arr.get(&ArrayKey::Int(col)).cloned().unwrap_or(Value::Null)),
        _ => Ok(Value::Bool(false)),
    }
}

/// `mysqli_fetch_all()`
pub fn fetch_all(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, b) = res(vm, a)?;
    let mode = a.int(b);
    if mode == 0 || (mode & !BOTH) != 0 {
        return Err(a.value_error(
            vm,
            pos(a, 2),
            "must be one of MYSQLI_NUM, MYSQLI_ASSOC, or MYSQLI_BOTH",
        ));
    }
    let mut rows = Vec::new();
    while let v @ Value::Array(_) = fetch_into(vm, &rs, mode)? {
        // `memory_limit` per row (see `php_memlimit::poll`).
        php_memlimit::poll()?;
        php_memlimit::TryGrow::try_grow(&mut rows, 1)?;
        rows.push(v);
    }
    Ok(Value::Array(ZArray::from_values(rows)))
}

/// `mysqli_fetch_object()`
pub fn fetch_object(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, b) = recv(vm, a)?;
    let class = if a.has(b) {
        let v = a.at(b);
        let name = match &v {
            Value::Str(s) => s.clone(),
            Value::Object(obj) if php_vm::object::class_of(obj).magic.to_string.is_some() => {
                vm.value_to_string(&v)?
            }
            other => {
                let msg = format!(
                    "must be a valid class name, {} given",
                    crate::util::type_name(other)
                );
                return Err(a.type_error(vm, pos(a, 2), &msg));
            }
        };
        let Some(c) = vm.lookup_class(&name)? else {
            let msg = format!(
                "must be a valid class name, \"{}\" given",
                name.to_string_lossy()
            );
            return Err(a.type_error(vm, pos(a, 2), &msg));
        };
        c
    } else {
        match vm.find_class(b"stdClass") {
            Some(c) => c,
            None => return Err(vm.throw("Error", "stdClass missing")),
        }
    };
    let ctor_args = match a.get(b + 1) {
        None => ZArray::new(),
        Some(Value::Array(arr)) => arr,
        Some(other) => {
            let msg = format!(
                "must be of type array, {} given",
                crate::util::type_name(&other)
            );
            return Err(a.type_error(vm, pos(a, 3), &msg));
        }
    };
    if class.flags & (cflags::INTERFACE | cflags::ABSTRACT) != 0
        || class.is_trait()
        || class.is_enum()
    {
        let msg = if class.is_enum() {
            format!("Cannot instantiate enum {}", class.name.to_string_lossy())
        } else {
            format!(
                "Class {} cannot be instantiated",
                class.name.to_string_lossy()
            )
        };
        return Err(vm.throw("Error", &msg));
    }
    if class.magic.construct.is_none() && !ctor_args.is_empty() {
        let msg = format!(
            "must be empty when the specified class ({}) does not have a constructor",
            class.name.to_string_lossy()
        );
        return Err(a.value_error(vm, pos(a, 3), &msg));
    }
    let rs = fetch_result(vm, &o, Status::Valid)?;
    let row = fetch_into(vm, &rs, ASSOC)?;
    let Value::Array(row) = row else {
        return Ok(row);
    };
    let obj = vm.new_object(&class)?;
    let direct = class
        .props
        .iter()
        .all(|p| p.flags & php_vm::func::flags::STATIC != 0)
        && class.magic.set.is_none();
    for (k, v) in row.iter() {
        let name = match k {
            ArrayKey::Int(i) => i.to_string(),
            ArrayKey::Str(s) => s.to_string_lossy(),
        };
        if direct {
            vm.set_property_raw(&obj, &name, v.clone());
        } else {
            vm.set_property(&obj, &name, v.clone())?;
        }
    }
    if class.magic.construct.is_some() {
        let args: Vec<Value> = ctor_args.values().cloned().collect();
        vm.call_method_by_name(&obj, "__construct", args)?;
    }
    Ok(Value::Object(obj))
}

fn field_object(vm: &mut Vm, f: &Field) -> VmResult<Value> {
    std_object(
        vm,
        vec![
            ("name", bstr(&f.name)),
            ("orgname", bstr(&f.org_name)),
            ("table", bstr(&f.table)),
            ("orgtable", bstr(&f.org_table)),
            ("def", bstr(&f.def)),
            ("db", bstr(&f.db)),
            ("catalog", Value::Str(ZStr::from("def"))),
            ("max_length", Value::Int(0)),
            ("length", Value::Int(i64::from(f.length))),
            ("charsetnr", Value::Int(i64::from(f.charsetnr))),
            ("flags", Value::Int(i64::from(f.flags))),
            ("type", Value::Int(i64::from(f.type_))),
            ("decimals", Value::Int(i64::from(f.decimals))),
        ],
    )
}

/// `mysqli_fetch_field()`
pub fn fetch_field(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let f = rs.borrow_mut().fetch_field().cloned();
    match f {
        Some(f) => field_object(vm, &f),
        None => Ok(Value::Bool(false)),
    }
}

/// `mysqli_fetch_fields()`
pub fn fetch_fields(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let fields = rs.borrow().fetch_fields().to_vec();
    let mut out = Vec::with_capacity(fields.len());
    for f in &fields {
        out.push(field_object(vm, f)?);
    }
    Ok(Value::Array(ZArray::from_values(out)))
}

/// `mysqli_fetch_field_direct()`
pub fn fetch_field_direct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, b) = recv(vm, a)?;
    let n = a.int(b);
    if n < 0 {
        return Err(a.value_error(vm, pos(a, 2), "must be greater than or equal to 0"));
    }
    let rs = fetch_result(vm, &o, Status::Valid)?;
    if n >= i64::from(rs.borrow().num_fields()) {
        return Err(a.value_error(
            vm,
            pos(a, 2),
            "must be less than the number of fields for this result set",
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let f = rs.borrow().fetch_field_direct(n as u32).cloned();
    match f {
        Some(f) => field_object(vm, &f),
        None => Ok(Value::Bool(false)),
    }
}

/// `mysqli_fetch_lengths()`
pub fn fetch_lengths(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let l = rs.borrow().fetch_lengths();
    Ok(match l {
        Some(l) => lengths_array(&l),
        None => Value::Bool(false),
    })
}

/// `[0 => len, ...]`
#[must_use]
pub fn lengths_array(l: &[usize]) -> Value {
    Value::Array(ZArray::from_values(
        l.iter()
            .map(|&n| Value::Int(i64::try_from(n).unwrap_or(i64::MAX))),
    ))
}

/// `mysqli_field_seek()`
pub fn field_seek(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, b) = recv(vm, a)?;
    let n = a.int(b);
    if n < 0 {
        return Err(a.value_error(vm, pos(a, 2), "must be greater than or equal to 0"));
    }
    let rs = fetch_result(vm, &o, Status::Valid)?;
    if n >= i64::from(rs.borrow().num_fields()) {
        return Err(a.value_error(
            vm,
            pos(a, 2),
            "must be less than the number of fields for this result set",
        ));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    rs.borrow_mut().field_seek(n as u32);
    Ok(Value::Bool(true))
}

/// `mysqli_field_tell()`
pub fn field_tell(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let n = rs.borrow().field_tell();
    Ok(Value::Int(i64::from(n)))
}

/// `mysqli_data_seek()`
pub fn data_seek(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, b) = recv(vm, a)?;
    let off = a.int(b);
    if off < 0 {
        return Err(a.value_error(vm, pos(a, 2), "must be greater than or equal to 0"));
    }
    let rs = fetch_result(vm, &o, Status::Valid)?;
    if !rs.borrow().is_buffered() {
        let f = if a.this.is_some() {
            "mysqli_result::data_seek"
        } else {
            "mysqli_data_seek"
        };
        return Err(vm.throw(
            "Error",
            &format!("{f}() cannot be used in MYSQLI_USE_RESULT mode"),
        ));
    }
    #[allow(clippy::cast_sign_loss)]
    let off = off as u64;
    if off >= rs.borrow().num_rows() {
        return Ok(Value::Bool(false));
    }
    rs.borrow_mut().data_seek(off);
    Ok(Value::Bool(true))
}

/// `mysqli_free_result()` / `close()` / `free()`
pub fn free_result(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (o, rs, _) = res(vm, a)?;
    rs.borrow_mut().free(false);
    internal::<ResultObj, _>(&o, |r| r.0 = None);
    flush_diag(vm)?;
    Ok(Value::Null)
}

/// `mysqli_num_fields()`
pub fn num_fields(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let n = rs.borrow().num_fields();
    Ok(Value::Int(i64::from(n)))
}

/// `mysqli_num_rows()`
pub fn num_rows(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, rs, _) = res(vm, a)?;
    let (buffered, eof, n) = {
        let r = rs.borrow();
        (r.is_buffered(), r.eof(), r.num_rows())
    };
    if !buffered && !eof {
        return Err(vm.throw(
            "Error",
            "mysqli_num_rows() cannot be used in MYSQLI_USE_RESULT mode",
        ));
    }
    Ok(long_or_string(n))
}

/// `mysqli_result::getIterator()`
pub fn get_iterator_method(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = a.this_obj();
    let it = ResultIter::new(o);
    let obj = vm.new_internal_iterator(Box::new(it))?;
    Ok(Value::Object(obj))
}

/// `get_iterator` handler of `mysqli_result`.
pub fn get_iterator(vm: &mut Vm, o: &Obj, by_ref: bool) -> VmResult<Box<dyn NativeIterator>> {
    if by_ref {
        return Err(vm.throw(
            "Error",
            "An iterator cannot be used with foreach by reference",
        ));
    }
    Ok(Box::new(ResultIter::new(o.clone())))
}

/// `php_mysqli_result_iterator`
#[derive(Debug)]
pub struct ResultIter {
    result: Obj,
    current: Value,
    row_num: i64,
}

impl ResultIter {
    fn new(result: Obj) -> Self {
        Self {
            result,
            current: Value::Null,
            row_num: -1,
        }
    }
}

impl NativeIterator for ResultIter {
    fn rewind(&mut self, vm: &mut Vm) -> VmResult<()> {
        let rs = fetch_result(vm, &self.result, Status::Valid)?;
        let (buffered, eof) = {
            let r = rs.borrow();
            (r.is_buffered(), r.eof())
        };
        if buffered {
            rs.borrow_mut().data_seek(0);
        } else if eof {
            vm.raise(
                crate::report::E_WARNING,
                "Data fetched with MYSQLI_USE_RESULT can be iterated only once",
            )?;
            return Ok(());
        }
        self.row_num = -1;
        self.next(vm)
    }

    fn valid(&mut self, _vm: &mut Vm) -> VmResult<bool> {
        Ok(matches!(self.current, Value::Array(_)))
    }

    fn current(&mut self, _vm: &mut Vm) -> VmResult<Value> {
        Ok(self.current.clone())
    }

    fn key(&mut self, _vm: &mut Vm) -> VmResult<Value> {
        Ok(Value::Int(self.row_num))
    }

    fn next(&mut self, vm: &mut Vm) -> VmResult<()> {
        let rs = fetch_result(vm, &self.result, Status::Valid)?;
        self.current = fetch_into(vm, &rs, ASSOC)?;
        if matches!(self.current, Value::Array(_)) {
            self.row_num += 1;
        }
        Ok(())
    }
}
