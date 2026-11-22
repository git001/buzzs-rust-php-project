// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The fetch engine of `pdo_stmt.c`: `fetch_value()` (with the
//! `ORACLE_NULLS` / stringify / type-override conversions), `do_fetch()`
//! for every `PDO::FETCH_*` mode, `fetchAll()` grouping, fetch-mode
//! validation and `setFetchMode()`.

use crate::consts::{
    FETCH_ASSOC, FETCH_BOTH, FETCH_BOUND, FETCH_CLASS, FETCH_CLASSTYPE, FETCH_COLUMN,
    FETCH_DEFAULT, FETCH_FUNC, FETCH_GROUP, FETCH_INTO, FETCH_KEY_PAIR, FETCH_LAZY, FETCH_NAMED,
    FETCH_NUM, FETCH_OBJ, FETCH_ORI_NEXT, FETCH_PROPS_LATE, FETCH_SERIALIZE, FETCH_UNIQUE,
    FIRST_INVALID_FLAG, NULL_EMPTY_STRING, NULL_TO_STRING, PARAM_BOOL, PARAM_INT, PARAM_LOB,
    PARAM_NULL, PARAM_STR,
};
use crate::driver::ParamEvent;
use crate::error::{Func, STMT_FETCH, STMT_FETCH_ALL, STMT_FETCH_OBJECT, STMT_SET_FETCH_MODE};
use crate::host::{Ctx, PdoHost};
use crate::row::PdoRow;
use crate::stmt::{PdoStatement, Stmt};
use php_runtime::{ArrayKey, ErrorLevel, Obj, RtResult, RuntimeError, Value, ZArray, ZStr};
use std::rc::Rc;

/// The mode bits of a fetch mode (`how & ~PDO_FETCH_FLAGS`).
fn mode_of(how: i64) -> i64 {
    how & 0xF
}

/// The flag bits of a fetch mode (`how & PDO_FETCH_FLAGS`).
fn flags_of(how: i64) -> i64 {
    how & 0xFFFF_FFF0
}

/// `stmt->fetch`: the per-mode data of the default fetch mode (a C union;
/// readers of another member see `0`/`NULL`).
#[derive(Debug, Clone, Default)]
pub(crate) enum FetchUnion {
    /// Nothing set (`memset(0)`).
    #[default]
    None,
    /// `fetch.column`.
    Column(i64),
    /// `fetch.cls`.
    Class {
        /// `ce` (`None`: not set, e.g. with `PDO::FETCH_CLASSTYPE`).
        ce: Option<ZStr>,
        /// `ctor_args`.
        ctor_args: Option<ZArray>,
    },
    /// `fetch.func.fcc`.
    Func(Value),
    /// `fetch.into`.
    Into(Value),
}

impl FetchUnion {
    fn column(&self) -> i64 {
        match self {
            FetchUnion::Column(c) => *c,
            _ => 0,
        }
    }

    fn class(&self) -> Option<ZStr> {
        match self {
            FetchUnion::Class { ce, .. } => ce.clone(),
            _ => None,
        }
    }

    fn ctor_args(&self) -> Option<ZArray> {
        match self {
            FetchUnion::Class { ctor_args, .. } => ctor_args.clone(),
            _ => None,
        }
    }

    pub(crate) fn gc_values(&self) -> Vec<Value> {
        match self {
            FetchUnion::Class {
                ctor_args: Some(a), ..
            } => vec![Value::Array(a.clone())],
            FetchUnion::Func(v) | FetchUnion::Into(v) => vec![v.clone()],
            _ => Vec::new(),
        }
    }
}

/// `fetch_value()`: the value of column `colno` of the current row, with
/// the handle's conversions applied.
pub(crate) fn fetch_value(
    cx: &mut Ctx<'_>,
    st: &Stmt,
    colno: i64,
    type_override: Option<i64>,
) -> Value {
    if colno < 0 {
        cx.throw_value_error("Column index must be greater than or equal to 0");
        return Value::Null;
    }
    if colno >= st.column_count.get() {
        cx.throw_value_error("Invalid column index");
        return Value::Null;
    }
    let Some(dbh) = st.dbh() else {
        return Value::Null;
    };
    let mut dest = match st.driver() {
        Some(d) => match cx.with_driver(|dcx| d.get_col(dcx, colno)) {
            Ok(v) => v,
            Err(e) => {
                st.apply(cx, e);
                Value::Null
            }
        },
        None => Value::Null,
    };
    let oracle_nulls = dbh.oracle_nulls.get();
    if oracle_nulls == NULL_EMPTY_STRING && matches!(&dest, Value::Str(s) if s.is_empty()) {
        dest = Value::Null;
    }
    let type_override = if dbh.stringify.get() {
        Some(PARAM_STR)
    } else {
        type_override
    };
    if let Some(t) = type_override {
        if !dest.is_null() {
            dest = match t {
                PARAM_INT => {
                    let r = dest.to_long(cx.host());
                    Value::Int(cx.check(r).unwrap_or(0))
                }
                PARAM_BOOL => {
                    let r = dest.to_bool(cx.host());
                    Value::Bool(cx.check(r).unwrap_or(false))
                }
                PARAM_STR => match &dest {
                    Value::Bool(false) => Value::from("0"),
                    Value::Resource(_) => {
                        Value::Str(ZStr::from(cx.host().read_stream(&dest).unwrap_or_default()))
                    }
                    Value::Str(_) => dest,
                    _ => {
                        let r = dest.to_zstr(cx.host());
                        Value::Str(cx.check(r).unwrap_or_else(ZStr::empty))
                    }
                },
                PARAM_NULL => Value::Null,
                PARAM_LOB => match dest {
                    Value::Str(s) => {
                        let r = cx.host().memory_stream(s);
                        cx.check(r).unwrap_or(Value::Null)
                    }
                    other => other,
                },
                _ => dest,
            };
        }
    }
    if dest.is_null() && oracle_nulls == NULL_TO_STRING {
        dest = Value::Str(ZStr::empty());
    }
    dest
}

/// `do_fetch_common()`: advances the cursor and updates bound columns.
pub(crate) fn do_fetch_common(cx: &mut Ctx<'_>, st: &Stmt, ori: i64, offset: i64) -> bool {
    if !st.executed.get() || st.execute_incomplete.get() {
        return false;
    }
    if !st.dispatch_param_event(cx, ParamEvent::FetchPre) {
        return false;
    }
    if !st.run_fetcher(cx, ori, offset) {
        return false;
    }
    if st.columns.borrow().is_none() && !st.describe_columns(cx) {
        return false;
    }
    if !st.dispatch_param_event(cx, ParamEvent::FetchPost) {
        return false;
    }
    let bound: Vec<(i64, Value, i64)> = st
        .bound_columns
        .borrow()
        .as_ref()
        .map(|t| {
            t.iter()
                .filter(|(_, p)| p.paramno >= 0 && p.parameter.is_reference())
                .map(|(_, p)| (p.paramno, p.parameter.clone(), p.param_type))
                .collect()
        })
        .unwrap_or_default();
    for (colno, var, ty) in bound {
        let v = fetch_value(cx, st, colno, Some(ty));
        if let Value::Reference(r) = var {
            r.set(v);
        }
    }
    true
}

/// `pdo_do_key_pair_fetch()`.
fn key_pair_fetch(cx: &mut Ctx<'_>, st: &Stmt, ori: i64, offset: i64, out: &mut ZArray) -> bool {
    if !do_fetch_common(cx, st, ori, offset) {
        return false;
    }
    if st.column_count.get() != 2 {
        st.raise(
            cx,
            "HY000",
            "PDO::FETCH_KEY_PAIR fetch mode requires the result set to contain exactly 2 columns.",
        );
        return false;
    }
    let key = fetch_value(cx, st, 0, None);
    let val = fetch_value(cx, st, 1, None);
    let key = if let Value::Int(i) = key {
        ArrayKey::Int(i)
    } else {
        let r = key.to_zstr(cx.host());
        ArrayKey::from_zstr(&cx.check(r).unwrap_or_else(ZStr::empty))
    };
    within_limit(cx, || out.make_mut().try_insert(key, val).map(drop))
}

/// Stores a fetched row where php-src's `emalloc` would bail out at
/// `memory_limit`: a violation pending from building the row, or growing
/// the result past the limit, becomes the pending memory fatal and the
/// fetch loop ends (`false`).
fn within_limit(
    cx: &mut Ctx<'_>,
    store: impl FnOnce() -> Result<(), php_memlimit::OverLimit>,
) -> bool {
    let r = php_memlimit::poll().and_then(|()| store());
    cx.check(r.map_err(RuntimeError::from)).is_some()
}

/// Converts a fetched group key to a string (`convert_to_string`).
fn to_key_string(cx: &mut Ctx<'_>, v: &Value) -> ZStr {
    let r = v.to_zstr(cx.host());
    cx.check(r).unwrap_or_else(ZStr::empty)
}

/// `pdo_call_fetch_object_constructor()`: `true` when it failed (an
/// exception is pending, and constructors are not called while one is).
fn call_ctor_failed(cx: &mut Ctx<'_>, obj: &Value, args: Option<&ZArray>) -> bool {
    if cx.has_exception() {
        return true;
    }
    let r = cx.host().call_constructor(obj, args);
    cx.check(r).is_none()
}

/// `pdo_get_lazy_object()`.
fn lazy_object(cx: &mut Ctx<'_>, st: &Rc<Stmt>) -> Option<Value> {
    let existing = st.lazy_object.borrow().clone().and_then(|w| w.upgrade());
    if let Some(rc) = existing {
        return Some(Value::Object(Obj::from_rc(rc)));
    }
    let row = PdoRow::new(PdoStatement(st.clone()));
    let r = cx.host().create_row(row);
    let v = cx.check(r)?;
    if let Value::Object(o) = &v {
        *st.lazy_object.borrow_mut() = Some(Rc::downgrade(o.rc()));
    }
    Some(v)
}

/// `do_fetch()`: fetches one row in mode `how` (`0` = the statement's
/// default). With `group_key`, the first column is stored there as a string
/// (`PDO::FETCH_GROUP` / `PDO::FETCH_UNIQUE`). `None` on failure.
#[allow(clippy::too_many_lines)]
pub(crate) fn do_fetch(
    cx: &mut Ctx<'_>,
    st: &Rc<Stmt>,
    how: i64,
    ori: i64,
    offset: i64,
    group_key: Option<&mut Value>,
) -> Option<Value> {
    let how = if how == FETCH_DEFAULT {
        st.default_fetch_type.get()
    } else {
        how
    };
    let flags = flags_of(how);
    let how = mode_of(how);

    if !do_fetch_common(cx, st, ori, offset) {
        return None;
    }
    if how == FETCH_BOUND {
        return Some(Value::Bool(true));
    }
    if how == FETCH_LAZY {
        return lazy_object(cx, st);
    }
    let count = st.column_count.get();

    if how == FETCH_COLUMN {
        let fetch_column = st.fetch.borrow().column();
        let mut colno = fetch_column;
        if flags & (FETCH_GROUP | FETCH_UNIQUE) != 0 && fetch_column == -1 {
            colno = 1;
        }
        if colno < 0 {
            cx.throw_value_error("Column index must be greater than or equal to 0");
            return None;
        }
        if colno >= count {
            cx.throw_value_error("Invalid column index");
            return None;
        }
        let value = if flags == FETCH_GROUP && fetch_column == -1 {
            fetch_value(cx, st, 1, None)
        } else if flags == FETCH_GROUP && colno != 0 {
            fetch_value(cx, st, 0, None)
        } else {
            fetch_value(cx, st, colno, None)
        };
        if let Some(gk) = group_key {
            let k = if flags == FETCH_GROUP && fetch_column > 0 {
                fetch_value(cx, st, colno, None)
            } else {
                fetch_value(cx, st, 0, None)
            };
            *gk = Value::Str(to_key_string(cx, &k));
        }
        return Some(value);
    }

    st.in_fetch.set(true);
    let r = do_fetch_row(cx, st, how, flags, count, group_key);
    st.in_fetch.set(false);
    r
}

/// The part of `do_fetch()` running with `in_fetch` set.
#[allow(clippy::too_many_lines)]
fn do_fetch_row(
    cx: &mut Ctx<'_>,
    st: &Rc<Stmt>,
    how: i64,
    flags: i64,
    count: i64,
    group_key: Option<&mut Value>,
) -> Option<Value> {
    let mut column_index = 0;
    // the scope of property writes (`ce`)
    let mut scope: Option<ZStr> = None;
    let mut ret: Value;
    let mut ctor_args: Option<ZArray> = None;
    let mut func: Option<Value> = None;
    let mut func_params: Vec<Value> = Vec::new();

    match how {
        FETCH_DEFAULT | FETCH_ASSOC | FETCH_BOTH | FETCH_NUM | FETCH_NAMED => {
            ret = Value::Array(ZArray::new());
        }
        FETCH_OBJ => {
            let std = ZStr::from("stdClass");
            let r = cx.host().instantiate(&std);
            ret = cx.check(r)?;
            scope = Some(std);
        }
        FETCH_CLASS => {
            let mut ce = st.fetch.borrow().class();
            if flags & FETCH_CLASSTYPE != 0 {
                let name = fetch_value(cx, st, column_index, None);
                column_index += 1;
                if let Value::Str(s) = &name {
                    ce = cx.host().lookup_class(s).map(|i| i.name);
                }
                if ce.is_none() {
                    ce = Some(ZStr::from("stdClass"));
                }
            } else if ce.is_none() {
                st.raise(cx, "HY000", "No fetch class specified");
                return None;
            }
            let ce = ce.unwrap_or_else(|| ZStr::from("stdClass"));
            ctor_args = st.fetch.borrow().ctor_args();
            if flags & FETCH_SERIALIZE != 0 {
                let can = cx
                    .host()
                    .lookup_class(&ce)
                    .is_some_and(|i| i.can_unserialize);
                if !can {
                    st.raise(cx, "HY000", "cannot unserialize class");
                    return None;
                }
                ret = Value::Null;
            } else {
                let r = cx.host().instantiate(&ce);
                ret = cx.check(r)?;
                if flags & FETCH_PROPS_LATE != 0 && call_ctor_failed(cx, &ret, ctor_args.as_ref()) {
                    return None;
                }
            }
            scope = Some(ce);
        }
        FETCH_INTO => {
            let into = match &*st.fetch.borrow() {
                FetchUnion::Into(v) => Some(v.clone()),
                _ => None,
            };
            let Some(into) = into else {
                st.raise(cx, "HY000", "No fetch-into object specified.");
                return None;
            };
            ret = into;
        }
        FETCH_FUNC => {
            func = match &*st.fetch.borrow() {
                FetchUnion::Func(v) => Some(v.clone()),
                _ => None,
            };
            if func.is_none() {
                st.raise(cx, "HY000", "No fetch function specified");
                return None;
            }
            ret = Value::Null;
        }
        _ => return None,
    }

    if let Some(gk) = group_key {
        let k = fetch_value(cx, st, column_index, None);
        *gk = Value::Str(to_key_string(cx, &k));
        column_index += 1;
    }

    if how == FETCH_CLASS && flags & FETCH_SERIALIZE != 0 {
        let data = fetch_value(cx, st, column_index, None);
        let bytes = data
            .as_str()
            .map(|s| s.as_bytes().to_vec())
            .unwrap_or_default();
        let ce = scope.clone().unwrap_or_else(|| ZStr::from("stdClass"));
        let r = cx.host().unserialize_class(&ce, &bytes);
        match r {
            Ok(v) => ret = v,
            Err(e) => {
                cx.throw(e);
                st.raise(cx, "HY000", "cannot unserialize class");
                return None;
            }
        }
        column_index += 1;
    }

    while column_index < count {
        let val = fetch_value(cx, st, column_index, None);
        let name = st.column_name(column_index);
        match how {
            FETCH_ASSOC => {
                if let Some(t) = ret.as_array_mut() {
                    t.insert(ArrayKey::from_zstr(&name), val);
                }
            }
            FETCH_DEFAULT | FETCH_BOTH => {
                if let Some(t) = ret.as_array_mut() {
                    t.insert(ArrayKey::from_zstr(&name), val.clone());
                    let _ = t.add(ArrayKey::Int(column_index), val);
                }
            }
            FETCH_NAMED => {
                if let Some(t) = ret.as_array_mut() {
                    let key = ArrayKey::Str(name);
                    match t.get_mut(&key) {
                        Some(cur) => {
                            if let Value::Array(a) = cur {
                                let _ = a.make_mut().push(val);
                            } else {
                                let old = std::mem::replace(cur, Value::Null);
                                *cur = Value::Array(ZArray::from_values([old, val]));
                            }
                        }
                        None => {
                            t.insert(key, val);
                        }
                    }
                }
            }
            FETCH_NUM => {
                if let Some(t) = ret.as_array_mut() {
                    let _ = t.push(val);
                }
            }
            FETCH_CLASS | FETCH_OBJ | FETCH_INTO => {
                let r = cx.host().update_property(&ret, scope.as_ref(), &name, val);
                cx.check(r);
            }
            FETCH_FUNC => func_params.push(val),
            _ => {}
        }
        column_index += 1;
    }

    if how == FETCH_CLASS && flags & (FETCH_PROPS_LATE | FETCH_SERIALIZE) == 0 {
        if call_ctor_failed(cx, &ret, ctor_args.as_ref()) {
            return None;
        }
    } else if how == FETCH_FUNC {
        if let Some(f) = func {
            if !cx.has_exception() {
                let r = cx.host().call(&f, &func_params);
                ret = cx.check(r).unwrap_or(Value::Null);
            }
        }
    }
    Some(ret)
}

/// `pdo_verify_fetch_mode()`.
fn verify_fetch_mode(
    cx: &mut Ctx<'_>,
    default_mode: i64,
    mode: i64,
    arg_num: u32,
    fetch_all: bool,
    func: Func,
) -> bool {
    if !(0..FIRST_INVALID_FLAG).contains(&mode) {
        cx.throw(func.value_error(arg_num, "must be a bitmask of PDO::FETCH_* constants"));
        return false;
    }
    let mut flags = flags_of(mode);
    let mut m = mode_of(mode);
    if m == FETCH_DEFAULT {
        flags = flags_of(default_mode);
        m = mode_of(default_mode);
    }
    if flags != 0 {
        let class_flags = flags & (FETCH_CLASSTYPE | FETCH_PROPS_LATE | FETCH_SERIALIZE) != 0;
        if class_flags && m != FETCH_CLASS {
            cx.throw(func.value_error(arg_num, "cannot use PDO::FETCH_CLASSTYPE, PDO::FETCH_PROPS_LATE, or PDO::FETCH_SERIALIZE fetch flags with a fetch mode other than PDO::FETCH_CLASS"));
            return false;
        }
        if flags & FETCH_SERIALIZE != 0 {
            cx.docref(
                ErrorLevel::Deprecated,
                "The PDO::FETCH_SERIALIZE mode is deprecated",
            );
            if cx.has_exception() {
                return false;
            }
        }
    }
    match m {
        FETCH_FUNC => {
            if !fetch_all {
                cx.throw(func.value_error(
                    arg_num,
                    "PDO::FETCH_FUNC can only be used with PDOStatement::fetchAll()",
                ));
                return false;
            }
            true
        }
        FETCH_LAZY => {
            if fetch_all {
                cx.throw(func.value_error(
                    arg_num,
                    "PDO::FETCH_LAZY cannot be used with PDOStatement::fetchAll()",
                ));
                return false;
            }
            true
        }
        FETCH_INTO => {
            if fetch_all {
                cx.throw(func.value_error(
                    arg_num,
                    "PDO::FETCH_INTO cannot be used with PDOStatement::fetchAll()",
                ));
                return false;
            }
            true
        }
        FETCH_ASSOC | FETCH_NUM | FETCH_BOTH | FETCH_OBJ | FETCH_BOUND | FETCH_COLUMN
        | FETCH_CLASS | FETCH_NAMED | FETCH_KEY_PAIR => true,
        _ => {
            cx.throw(func.value_error(arg_num, "must be a bitmask of PDO::FETCH_* constants"));
            false
        }
    }
}

/// `pdo_stmt_setup_fetch_mode()`: `args` are the variadic arguments after
/// the mode (argument `mode_arg_num`) of `func`.
#[allow(clippy::too_many_lines)]
pub(crate) fn setup_fetch_mode(
    cx: &mut Ctx<'_>,
    st: &Rc<Stmt>,
    mode: i64,
    mode_arg_num: u32,
    args: &[Value],
    func: Func,
) -> bool {
    let Some(dbh) = st.dbh() else {
        return false;
    };
    let first_arg_num = mode_arg_num + 1;
    let ctor_arg_num = mode_arg_num + 2;
    let nargs = u32::try_from(args.len()).unwrap_or(u32::MAX);
    let total = mode_arg_num + nargs;
    let fetch_type = mode_of(mode);
    let flags = flags_of(mode);

    if !verify_fetch_mode(
        cx,
        dbh.default_fetch_type.get(),
        mode,
        mode_arg_num,
        false,
        func,
    ) {
        return false;
    }
    let count_error = |cx: &mut Ctx<'_>, kind: &str, n: u32| {
        cx.throw(func.count_error(&format!(
            "expects {kind} {n} arguments for the fetch mode provided, {total} given"
        )));
    };
    let new_union = match fetch_type {
        FETCH_DEFAULT | FETCH_LAZY | FETCH_ASSOC | FETCH_NUM | FETCH_BOTH | FETCH_OBJ
        | FETCH_BOUND | FETCH_NAMED | FETCH_KEY_PAIR => {
            if nargs != 0 {
                count_error(cx, "exactly", mode_arg_num);
                return false;
            }
            FetchUnion::None
        }
        FETCH_COLUMN => {
            if nargs != 1 {
                count_error(cx, "exactly", first_arg_num);
                return false;
            }
            let Value::Int(c) = args[0].deref() else {
                cx.throw(func.type_error(
                    first_arg_num,
                    &format!("must be of type int, {} given", args[0].value_name()),
                ));
                return false;
            };
            if c < 0 {
                cx.throw(func.value_error(first_arg_num, "must be greater than or equal to 0"));
                return false;
            }
            FetchUnion::Column(c)
        }
        FETCH_CLASS => {
            if flags & FETCH_CLASSTYPE == FETCH_CLASSTYPE {
                if nargs != 0 {
                    count_error(cx, "exactly", mode_arg_num);
                    return false;
                }
                FetchUnion::Class {
                    ce: None,
                    ctor_args: None,
                }
            } else {
                if nargs == 0 {
                    count_error(cx, "at least", first_arg_num);
                    return false;
                }
                if nargs > 2 {
                    count_error(cx, "at most", ctor_arg_num);
                    return false;
                }
                let Value::Str(name) = args[0].deref() else {
                    cx.throw(func.wrong_type(first_arg_num, "string", &args[0]));
                    return false;
                };
                let Some(info) = cx.host().lookup_class(&name) else {
                    if !cx.has_exception() {
                        cx.throw(func.type_error(first_arg_num, "must be a valid class"));
                    }
                    return false;
                };
                let mut ctor_args = None;
                if nargs == 2 {
                    match args[1].deref() {
                        Value::Null => {}
                        Value::Array(a) => {
                            if !a.is_empty() {
                                if !info.has_constructor {
                                    cx.throw(func.value_error(3, "must be empty when class provided in argument #2 ($class) does not have a constructor"));
                                    return false;
                                }
                                ctor_args = Some(a);
                            }
                        }
                        _ => {
                            cx.throw(func.wrong_type(ctor_arg_num, "?array", &args[1]));
                            return false;
                        }
                    }
                }
                FetchUnion::Class {
                    ce: Some(info.name),
                    ctor_args,
                }
            }
        }
        FETCH_INTO => {
            if total != first_arg_num {
                count_error(cx, "exactly", first_arg_num);
                return false;
            }
            let v = args[0].deref();
            if !matches!(v, Value::Object(_)) {
                cx.throw(func.wrong_type(first_arg_num, "object", &args[0]));
                return false;
            }
            FetchUnion::Into(v)
        }
        _ => {
            cx.throw(func.value_error(mode_arg_num, "must be one of the PDO::FETCH_* constants"));
            return false;
        }
    };
    let old = std::mem::replace(&mut *st.fetch.borrow_mut(), new_union);
    drop(old);
    if fetch_type == FETCH_DEFAULT {
        st.default_fetch_type.set(dbh.default_fetch_type.get());
    } else {
        st.default_fetch_type.set(mode);
    }
    true
}

impl PdoStatement {
    /// `PDOStatement::fetch()`: the row, or `false`.
    pub fn fetch(
        &self,
        host: &mut dyn PdoHost,
        mode: i64,
        cursor_orientation: i64,
        cursor_offset: i64,
    ) -> RtResult<Value> {
        let mut cx = Ctx::new(host);
        let r = self.fetch_impl(&mut cx, mode, cursor_orientation, cursor_offset);
        cx.finish(r)
    }

    fn fetch_impl(&self, cx: &mut Ctx<'_>, how: i64, ori: i64, off: i64) -> Value {
        if self.get(cx).is_none() {
            return Value::Null;
        }
        let st = &self.0;
        st.clear_err();
        if !verify_fetch_mode(cx, st.default_fetch_type.get(), how, 1, false, STMT_FETCH) {
            return Value::Null;
        }
        if mode_of(how) == FETCH_KEY_PAIR {
            let mut out = ZArray::new();
            if !key_pair_fetch(cx, st, ori, off, &mut out) {
                st.handle_err(cx);
                return Value::Bool(false);
            }
            return Value::Array(out);
        }
        if let Some(v) = do_fetch(cx, st, how, ori, off, None) {
            v
        } else {
            st.handle_err(cx);
            Value::Bool(false)
        }
    }

    /// `PDOStatement::fetchObject()`: `class` is the class name (`None`
    /// for `stdClass`).
    pub fn fetch_object(
        &self,
        host: &mut dyn PdoHost,
        class: Option<&ZStr>,
        ctor_args: Option<&ZArray>,
    ) -> RtResult<Value> {
        let mut cx = Ctx::new(host);
        let r = self.fetch_object_impl(&mut cx, class, ctor_args);
        cx.finish(r)
    }

    fn fetch_object_impl(
        &self,
        cx: &mut Ctx<'_>,
        class: Option<&ZStr>,
        ctor_args: Option<&ZArray>,
    ) -> Value {
        let info = match class {
            Some(name) => {
                if let Some(i) = cx.host().lookup_class(name) {
                    Some(i)
                } else {
                    if !cx.has_exception() {
                        cx.throw(STMT_FETCH_OBJECT.type_error(1, &unknown_class_message(name)));
                    }
                    return Value::Null;
                }
            }
            None => None,
        };
        if self.get(cx).is_none() {
            return Value::Null;
        }
        let st = &self.0;
        st.clear_err();
        let (ce, has_ctor) = match info {
            Some(i) => (i.name, i.has_constructor),
            None => (ZStr::from("stdClass"), false),
        };
        if ctor_args.is_some_and(|a| !a.is_empty()) && !has_ctor {
            cx.throw(STMT_FETCH_OBJECT.value_error(
                2,
                "must be empty when class provided in argument #1 ($class) does not have a constructor",
            ));
            return Value::Null;
        }
        let old = std::mem::replace(
            &mut *st.fetch.borrow_mut(),
            FetchUnion::Class {
                ce: Some(ce),
                ctor_args: ctor_args.cloned(),
            },
        );
        let r = if let Some(v) = do_fetch(cx, st, FETCH_CLASS, FETCH_ORI_NEXT, 0, None) {
            v
        } else {
            st.handle_err(cx);
            Value::Bool(false)
        };
        *st.fetch.borrow_mut() = old;
        r
    }

    /// `PDOStatement::fetchColumn()`.
    pub fn fetch_column(&self, host: &mut dyn PdoHost, column: i64) -> RtResult<Value> {
        let mut cx = Ctx::new(host);
        let r = self.fetch_column_impl(&mut cx, column);
        cx.finish(r)
    }

    fn fetch_column_impl(&self, cx: &mut Ctx<'_>, column: i64) -> Value {
        if self.get(cx).is_none() {
            return Value::Null;
        }
        let st = &self.0;
        st.clear_err();
        if !do_fetch_common(cx, st, FETCH_ORI_NEXT, 0) {
            st.handle_err(cx);
            return Value::Bool(false);
        }
        fetch_value(cx, st, column, None)
    }

    /// `PDOStatement::fetchAll()`: `mode` is `None` when no argument was
    /// passed; `args` are the arguments after the mode (at most two:
    /// `$class`/`$column`/`$callback` and `$constructorArgs`).
    pub fn fetch_all(
        &self,
        host: &mut dyn PdoHost,
        mode: Option<i64>,
        args: &[Value],
    ) -> RtResult<ZArray> {
        let mut cx = Ctx::new(host);
        let r = self.fetch_all_impl(&mut cx, mode, args);
        cx.finish(r.unwrap_or_default())
    }

    #[allow(clippy::too_many_lines)]
    fn fetch_all_impl(
        &self,
        cx: &mut Ctx<'_>,
        mode: Option<i64>,
        args: &[Value],
    ) -> Option<ZArray> {
        let f = STMT_FETCH_ALL;
        let num_args = u32::from(mode.is_some()) + u32::try_from(args.len()).unwrap_or(u32::MAX);
        if num_args > 3 {
            cx.throw(f.count_error(&format!("expects at most 3 arguments, {num_args} given")));
            return None;
        }
        let mut how = mode.unwrap_or(FETCH_DEFAULT);
        let second = args.first().map(Value::deref).filter(|v| !v.is_null());
        let ctor_args = match args.get(1).map(Value::deref) {
            None | Some(Value::Null) => None,
            Some(Value::Array(a)) => Some(a),
            Some(other) => {
                cx.throw(f.wrong_type(3, "?array", &other));
                return None;
            }
        };
        self.get(cx)?;
        let st = &self.0;
        if !verify_fetch_mode(cx, st.default_fetch_type.get(), how, 1, true, f) {
            return None;
        }
        let mut fetch_mode = mode_of(how);
        let mut flags = flags_of(how);
        let old = st.fetch.borrow().clone();

        match fetch_mode {
            FETCH_CLASS => {
                let fetch_class = if let Some(a2) = &second {
                    let Value::Str(name) = a2 else {
                        cx.throw(f.wrong_type(2, "string", a2));
                        return None;
                    };
                    let Some(info) = cx.host().lookup_class(name) else {
                        if !cx.has_exception() {
                            cx.throw(f.type_error(2, "must be a valid class"));
                        }
                        return None;
                    };
                    info
                } else {
                    crate::host::ClassInfo::plain("stdClass")
                };
                let mut new_args = old_ctor_args(&old);
                if let Some(a) = &ctor_args {
                    if !a.is_empty() {
                        if !fetch_class.has_constructor {
                            cx.throw(f.value_error(3, "must be empty when class provided in argument #2 ($class) does not have a constructor"));
                            return None;
                        }
                        new_args = Some(a.clone());
                    }
                }
                *st.fetch.borrow_mut() = FetchUnion::Class {
                    ce: Some(fetch_class.name),
                    ctor_args: new_args,
                };
            }
            FETCH_FUNC => {
                if num_args != 2 {
                    cx.throw(f.count_error(&format!(
                        "expects exactly 2 argument for PDO::FETCH_FUNC, {num_args} given"
                    )));
                    return None;
                }
                let Some(callable) = second else {
                    cx.throw(f.type_error(2, "must be a callable, null given"));
                    return None;
                };
                // `pdo_get_fcc_from_zval`: `zend_is_callable()` raises the
                // callable deprecations (pdo_fetch_func_001).
                let checked = cx.host().check_callable_with_deprecations(&callable);
                if let Err(msg) = cx.check(checked)? {
                    cx.throw_type_error(msg);
                    return None;
                }
                *st.fetch.borrow_mut() = FetchUnion::Func(callable);
            }
            FETCH_COLUMN => {
                if num_args > 2 {
                    cx.throw(f.count_error(&format!(
                        "expects at most 2 argument for the fetch mode provided, {num_args} given"
                    )));
                    return None;
                }
                let col = if let Some(a2) = &second {
                    let Value::Int(c) = a2 else {
                        cx.throw(f.wrong_type(2, "int", a2));
                        return None;
                    };
                    if *c < 0 {
                        cx.throw(f.value_error(2, "must be greater than or equal to 0"));
                        return None;
                    }
                    *c
                } else if flags & (FETCH_GROUP | FETCH_UNIQUE) != 0 {
                    -1
                } else {
                    0
                };
                *st.fetch.borrow_mut() = FetchUnion::Column(col);
            }
            _ => {
                if num_args > 1 {
                    cx.throw(f.count_error(&format!(
                        "expects exactly 1 argument for the fetch mode provided, {num_args} given"
                    )));
                    return None;
                }
            }
        }

        if fetch_mode == FETCH_DEFAULT {
            let def = st.default_fetch_type.get();
            flags |= flags_of(def);
            fetch_mode = mode_of(def);
            how = fetch_mode | flags;
        }
        st.clear_err();
        let mut out = ZArray::new();

        if fetch_mode == FETCH_KEY_PAIR {
            while key_pair_fetch(cx, st, FETCH_ORI_NEXT, 0, &mut out) {}
            st.handle_err(cx);
            return Some(out);
        }

        if flags & (FETCH_GROUP | FETCH_UNIQUE) != 0 {
            loop {
                let mut gk = Value::Null;
                let Some(data) = do_fetch(cx, st, how | flags, FETCH_ORI_NEXT, 0, Some(&mut gk))
                else {
                    break;
                };
                let key = match &gk {
                    Value::Str(s) => ArrayKey::from_zstr(s),
                    _ => ArrayKey::from_bytes(b""),
                };
                let t = out.make_mut();
                let stored = within_limit(cx, || {
                    if flags & FETCH_UNIQUE == FETCH_UNIQUE {
                        return t.try_insert(key, data).map(drop);
                    }
                    if !matches!(t.get(&key), Some(Value::Array(_))) {
                        t.try_insert(key.clone(), Value::Array(ZArray::new()))?;
                    }
                    match t.lookup(key) {
                        Value::Array(g) => g.make_mut().try_push(data).map(drop),
                        _ => Ok(()),
                    }
                });
                if !stored {
                    break;
                }
            }
        } else {
            while let Some(data) = do_fetch(cx, st, how, FETCH_ORI_NEXT, 0, None) {
                if !within_limit(cx, || out.make_mut().try_push(data).map(drop)) {
                    break;
                }
            }
        }

        // restore the defaults changed by the class/column/func modes
        *st.fetch.borrow_mut() = old;

        if st.error_code.get() != crate::error::ERR_NONE {
            st.handle_err(cx);
            return Some(ZArray::new());
        }
        Some(out)
    }

    /// `PDOStatement::setFetchMode()`: `args` are the arguments after the
    /// mode.
    pub fn set_fetch_mode(
        &self,
        host: &mut dyn PdoHost,
        mode: i64,
        args: &[Value],
    ) -> RtResult<bool> {
        let mut cx = Ctx::new(host);
        let r = self.set_fetch_mode_impl(&mut cx, mode, args);
        cx.finish(r)
    }

    fn set_fetch_mode_impl(&self, cx: &mut Ctx<'_>, mode: i64, args: &[Value]) -> bool {
        if self.get(cx).is_none() {
            return false;
        }
        if self.0.in_fetch.get() {
            cx.throw_error("Cannot change default fetch mode while fetching");
            return false;
        }
        setup_fetch_mode(cx, &self.0, mode, 1, args, STMT_SET_FETCH_MODE)
    }

    /// `PDOStatement::getIterator()` / `foreach`: the statement iterator
    /// (`pdo_stmt_iter_get`), positioned on the first row.
    pub fn iterator(&self, host: &mut dyn PdoHost, by_ref: bool) -> RtResult<StmtIterator> {
        let mut cx = Ctx::new(host);
        if by_ref {
            cx.throw_error("An iterator cannot be used with foreach by reference");
            return Err(cx
                .take_exception()
                .unwrap_or_else(|| php_runtime::RuntimeError::Error("iterator error".into())));
        }
        let mut it = StmtIterator {
            stmt: self.clone(),
            key: Some(0),
            fetch_ahead: None,
        };
        if self.get(&mut cx).is_some() {
            it.advance(&mut cx, false);
        }
        cx.finish(it)
    }
}

/// `zend_wrong_parameter_type_error(Z_EXPECTED_CLASS_NAME_OR_NULL)` for a
/// string naming no class (`Z_PARAM_CLASS_OR_NULL`): the name is quoted up
/// to its first NUL byte (`Z_STRVAL`).
fn unknown_class_message(name: &ZStr) -> String {
    let b = name.as_bytes();
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    format!(
        "must be a valid class name or null, \"{}\" given",
        String::from_utf8_lossy(&b[..end])
    )
}

/// The ctor args a `fetchAll(PDO::FETCH_CLASS)` keeps when none are given
/// (the `cls.ctor_args` slot of the union).
fn old_ctor_args(old: &FetchUnion) -> Option<ZArray> {
    old.ctor_args()
}

/// The `foreach` iterator of a statement (`struct php_pdo_iterator`).
#[derive(Debug, Clone)]
pub struct StmtIterator {
    stmt: PdoStatement,
    /// `None` is the `(zend_ulong)-1` "no key".
    key: Option<u64>,
    fetch_ahead: Option<Value>,
}

impl StmtIterator {
    fn advance(&mut self, cx: &mut Ctx<'_>, increment: bool) {
        self.fetch_ahead = None;
        let st = &self.stmt.0;
        if let Some(v) = do_fetch(cx, st, FETCH_DEFAULT, FETCH_ORI_NEXT, 0, None) {
            self.fetch_ahead = Some(v);
            if increment {
                self.key = Some(self.key.map_or(0, |k| k.wrapping_add(1)));
            }
        } else {
            st.handle_err(cx);
            self.key = None;
        }
    }

    /// `valid`.
    pub fn valid(&self) -> bool {
        self.fetch_ahead.is_some()
    }

    /// `get_current_data`.
    pub fn current(&self) -> Option<Value> {
        self.fetch_ahead.clone()
    }

    /// `get_current_key`.
    pub fn key(&self) -> Value {
        match self.key {
            #[allow(clippy::cast_possible_wrap)]
            Some(k) => Value::Int(k as i64),
            None => Value::Null,
        }
    }

    /// `move_forward`.
    pub fn move_forward(&mut self, host: &mut dyn PdoHost) -> RtResult<()> {
        let mut cx = Ctx::new(host);
        self.advance(&mut cx, true);
        cx.finish(())
    }

    /// The statement iterated over.
    pub fn statement(&self) -> &PdoStatement {
        &self.stmt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_class_message_quotes_the_name() {
        assert_eq!(
            unknown_class_message(&ZStr::from("class_does_not_exist")),
            "must be a valid class name or null, \"class_does_not_exist\" given"
        );
        assert_eq!(
            unknown_class_message(&ZStr::from("a\0b")),
            "must be a valid class name or null, \"a\" given"
        );
    }
}
