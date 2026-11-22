// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The [`Handler`] that runs PHP callbacks during a transfer
//! (`curl_write`, `curl_read`, `curl_progress`, ... in interface.c).

use crate::state::{curl_of, Curl, CurlMulti, Method};
use crate::streams;
use php_curl::consts::{
    CURLINFO_HEADER_OUT, CURLKHMATCH_MISMATCH, CURLKHMATCH_OK, CURL_FNMATCHFUNC_FAIL,
    CURL_PREREQFUNC_ABORT, CURL_PREREQFUNC_OK, CURL_PUSH_DENY, CURL_READFUNC_ABORT,
    CURL_READFUNC_PAUSE, CURL_SEEKFUNC_CANTSEEK, CURL_SEEKFUNC_FAIL, CURL_SEEKFUNC_OK,
    CURL_WRITEFUNC_ERROR,
};
use php_curl::{Easy, Handler};
use php_memlimit::TryGrow;
use php_runtime::{Obj, Value, ZArray};
use php_vm::ext::{Throw, VmResult};
use php_vm::Vm;
use std::rc::Rc;

const SEEK_MSG: &str = "The CURLOPT_SEEKFUNCTION callback must return one of CURL_SEEKFUNC_OK, CURL_SEEKFUNC_FAIL or CURL_SEEKFUNC_CANTSEEK";
const PREREQ_MSG: &str =
    "The CURLOPT_PREREQFUNCTION callback must return either CURL_PREREQFUNC_OK or CURL_PREREQFUNC_ABORT";

/// A handle taking part in the transfer.
struct Entry {
    key: u64,
    obj: Obj,
    curl: Rc<Curl>,
}

/// Dispatches libcurl callbacks to PHP for the handles of one
/// `curl_exec()`/`curl_multi_exec()` call.
pub(crate) struct Dispatch<'a> {
    pub(crate) vm: &'a mut Vm,
    entries: Vec<Entry>,
    multi: Option<(Obj, Rc<CurlMulti>)>,
    /// The first exception thrown by a callback (`EG(exception)`); later
    /// callbacks are not called, as `zend_call_function()` refuses to run
    /// with an exception pending.
    pub(crate) pending: Option<Throw>,
}

/// `php_curl_get_long()`: `zval_get_long()` of a callback result.
fn get_long(vm: &mut Vm, v: &Value) -> VmResult<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        other => vm.value_to_int(other),
    }
}

#[allow(clippy::cast_possible_truncation)]
fn as_c_int(v: i64) -> i32 {
    v as i32
}

impl<'a> Dispatch<'a> {
    /// A dispatcher for the given handles.
    pub(crate) fn new(vm: &'a mut Vm, handles: &[Obj]) -> Self {
        let entries = handles
            .iter()
            .filter_map(|o| {
                curl_of(o).map(|curl| Entry {
                    key: curl.easy.key(),
                    obj: o.clone(),
                    curl,
                })
            })
            .collect();
        Dispatch {
            vm,
            entries,
            multi: None,
            pending: None,
        }
    }

    /// Adds the multi handle (for server push).
    pub(crate) fn with_multi(mut self, obj: Obj, m: Rc<CurlMulti>) -> Self {
        self.multi = Some((obj, m));
        self
    }

    fn lookup(&self, key: u64) -> Option<(Obj, Rc<Curl>)> {
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| (e.obj.clone(), Rc::clone(&e.curl)))
    }

    fn fail(&mut self, t: Throw) {
        if self.pending.is_none() {
            self.pending = Some(t);
        }
    }

    /// Records the result of a VM operation made inside a callback.
    fn check<T>(&mut self, r: VmResult<T>) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(t) => {
                self.fail(t);
                None
            }
        }
    }

    /// `php_curl_call_callback()`: `None` when the call did not produce a
    /// value (exception pending or thrown). The callable is released while
    /// the callback guard is still set, so destructors it triggers see it.
    fn call(&mut self, curl: &Curl, cb: Value, args: Vec<Value>) -> Option<Value> {
        if self.pending.is_some() {
            return None;
        }
        let was = curl.in_callback.replace(true);
        let r = self.vm.call_value(&cb, args);
        drop(cb);
        let d = run_destructors(self.vm);
        curl.in_callback.set(was);
        let r = self.check(r);
        self.check(d);
        r
    }

    /// `_php_curl_verify_handlers(ch, 1)` after a callback returned.
    fn verify(&mut self, curl: &Curl) {
        let r = verify_handlers(self.vm, curl, true);
        self.check(r);
    }

    /// Writes to the stream of a `PHP_CURL_FILE` handler.
    fn write_stream(&mut self, stream: Option<&Value>, data: &[u8]) -> usize {
        let Some(Value::Resource(r)) = stream else {
            return 0;
        };
        let r = streams::write(self.vm, r, data);
        self.check(r).unwrap_or(0)
    }

    fn user_write(&mut self, obj: &Obj, curl: &Curl, cb: Value, data: &[u8]) -> usize {
        let args = vec![Value::Object(obj.clone()), Value::from(data)];
        match self.call(curl, cb, args) {
            Some(v) => {
                self.verify(curl);
                let r = get_long(self.vm, &v);
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                self.check(r).map_or(usize::MAX, |n| n as usize)
            }
            None => usize::MAX,
        }
    }
}

impl Handler for Dispatch<'_> {
    fn write(&mut self, key: u64, data: &[u8]) -> usize {
        let Some((obj, curl)) = self.lookup(key) else {
            return data.len();
        };
        let (method, fcc, stream) = {
            let st = curl.st.borrow();
            (
                st.write.method,
                st.write.fcc.clone(),
                st.write.stream.clone(),
            )
        };
        match method {
            Method::Stdout => {
                self.vm.output(data);
                data.len()
            }
            Method::File => self.write_stream(stream.as_ref(), data),
            Method::Return => {
                if !data.is_empty() {
                    let mut st = curl.st.borrow_mut();
                    let buf = st.write.buf.get_or_insert_with(Vec::new);
                    // `smart_str_fappendl_ex(..., fallible)`: over the memory
                    // limit the transfer fails (the VM raises the delayed
                    // fatal once curl_exec() returns).
                    if buf.try_grow(data.len()).is_err() {
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        return CURL_WRITEFUNC_ERROR as usize;
                    }
                    buf.extend_from_slice(data);
                }
                data.len()
            }
            Method::User => match fcc {
                Some(cb) => self.user_write(&obj, &curl, cb, data),
                None => data.len(),
            },
            Method::Direct | Method::Ignore => data.len(),
        }
    }

    fn header(&mut self, key: u64, data: &[u8]) -> usize {
        let Some((obj, curl)) = self.lookup(key) else {
            return data.len();
        };
        let (method, fcc, stream, write_method) = {
            let st = curl.st.borrow();
            (
                st.write_header.method,
                st.write_header.fcc.clone(),
                st.write_header.stream.clone(),
                st.write.method,
            )
        };
        match method {
            Method::Stdout => {
                if write_method == Method::Return && !data.is_empty() {
                    curl.st
                        .borrow_mut()
                        .write
                        .buf
                        .get_or_insert_with(Vec::new)
                        .extend_from_slice(data);
                } else {
                    self.vm.output(data);
                }
                data.len()
            }
            Method::File => self.write_stream(stream.as_ref(), data),
            Method::User => match fcc {
                Some(cb) => self.user_write(&obj, &curl, cb, data),
                None => data.len(),
            },
            Method::Ignore => data.len(),
            Method::Direct | Method::Return => usize::MAX,
        }
    }

    fn read(&mut self, key: u64, buf: &mut [u8]) -> usize {
        let Some((obj, curl)) = self.lookup(key) else {
            return 0;
        };
        let (method, fcc, stream) = {
            let st = curl.st.borrow();
            (st.read.method, st.read.fcc.clone(), st.read.stream.clone())
        };
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let abort = CURL_READFUNC_ABORT as usize;
        match method {
            Method::Direct => match stream {
                Some(Value::Resource(r)) => {
                    let n = streams::read(self.vm, &r, buf);
                    self.check(n).unwrap_or(0)
                }
                _ => 0,
            },
            Method::User => {
                let Some(cb) = fcc else { return 0 };
                let res = stream.unwrap_or(Value::Null);
                let nmemb = i64::try_from(buf.len()).unwrap_or(i64::MAX);
                let args = vec![Value::Object(obj.clone()), res, Value::Int(nmemb)];
                let Some(v) = self.call(&curl, cb, args) else {
                    return abort;
                };
                self.verify(&curl);
                match v {
                    Value::Str(s) => {
                        let n = s.len().min(buf.len());
                        buf[..n].copy_from_slice(&s.as_bytes()[..n]);
                        n
                    }
                    Value::Int(l) => {
                        if l == 0 || l == CURL_READFUNC_ABORT || l == CURL_READFUNC_PAUSE {
                            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                            let n = l as usize;
                            n
                        } else {
                            let t = self.vm.throw(
                                "ValueError",
                                "The CURLOPT_READFUNCTION callback must return a string or CURL_READFUNC_ABORT or CURL_READFUNC_PAUSE",
                            );
                            self.fail(t);
                            abort
                        }
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn seek(&mut self, key: u64, offset: i64, origin: i32) -> i32 {
        let cantseek = as_c_int(CURL_SEEKFUNC_CANTSEEK);
        let Some((obj, curl)) = self.lookup(key) else {
            return cantseek;
        };
        let Some(cb) = curl.st.borrow().seek.clone() else {
            return cantseek;
        };
        let args = vec![
            Value::Object(obj),
            Value::Int(offset),
            Value::Int(i64::from(origin)),
        ];
        let Some(v) = self.call(&curl, cb, args) else {
            return cantseek;
        };
        self.verify(&curl);
        match v {
            Value::Int(l)
                if l == CURL_SEEKFUNC_OK
                    || l == CURL_SEEKFUNC_FAIL
                    || l == CURL_SEEKFUNC_CANTSEEK =>
            {
                as_c_int(l)
            }
            Value::Int(_) => {
                let t = self.vm.throw("ValueError", SEEK_MSG);
                self.fail(t);
                cantseek
            }
            _ => {
                let t = self.vm.throw("TypeError", SEEK_MSG);
                self.fail(t);
                cantseek
            }
        }
    }

    fn progress(&mut self, key: u64, dltotal: f64, dlnow: f64, ultotal: f64, ulnow: f64) -> i32 {
        let Some((obj, curl)) = self.lookup(key) else {
            return 0;
        };
        let Some(cb) = curl.st.borrow().progress.clone() else {
            return 0;
        };
        #[allow(clippy::cast_possible_truncation)]
        let args = vec![
            Value::Object(obj),
            Value::Int(dltotal as i64),
            Value::Int(dlnow as i64),
            Value::Int(ultotal as i64),
            Value::Int(ulnow as i64),
        ];
        self.progress_result(&curl, cb, args)
    }

    fn xferinfo(&mut self, key: u64, dltotal: i64, dlnow: i64, ultotal: i64, ulnow: i64) -> i32 {
        let Some((obj, curl)) = self.lookup(key) else {
            return 0;
        };
        let Some(cb) = curl.st.borrow().xferinfo.clone() else {
            return 0;
        };
        let args = vec![
            Value::Object(obj),
            Value::Int(dltotal),
            Value::Int(dlnow),
            Value::Int(ultotal),
            Value::Int(ulnow),
        ];
        self.progress_result(&curl, cb, args)
    }

    fn fnmatch(&mut self, key: u64, pattern: &[u8], string: &[u8]) -> i32 {
        let fail = as_c_int(CURL_FNMATCHFUNC_FAIL);
        let Some((obj, curl)) = self.lookup(key) else {
            return fail;
        };
        let Some(cb) = curl.st.borrow().fnmatch.clone() else {
            return fail;
        };
        let args = vec![
            Value::Object(obj),
            Value::from(pattern),
            Value::from(string),
        ];
        let Some(v) = self.call(&curl, cb, args) else {
            return fail;
        };
        self.verify(&curl);
        let r = get_long(self.vm, &v);
        self.check(r).map_or(fail, as_c_int)
    }

    fn debug(&mut self, key: u64, kind: i32, data: &[u8]) {
        let Some((obj, curl)) = self.lookup(key) else {
            return;
        };
        if i64::from(kind) == CURLINFO_HEADER_OUT {
            curl.st.borrow_mut().header_out = Some(data.to_vec());
        }
        let Some(cb) = curl.st.borrow().debug.clone() else {
            return;
        };
        let args = vec![
            Value::Object(obj),
            Value::Int(i64::from(kind)),
            Value::from(data),
        ];
        let _ = self.call(&curl, cb, args);
    }

    fn prereq(
        &mut self,
        key: u64,
        primary_ip: &[u8],
        local_ip: &[u8],
        primary_port: i32,
        local_port: i32,
    ) -> i32 {
        let abort = as_c_int(CURL_PREREQFUNC_ABORT);
        let Some((obj, curl)) = self.lookup(key) else {
            return as_c_int(CURL_PREREQFUNC_OK);
        };
        let Some(cb) = curl.st.borrow().prereq.clone() else {
            return as_c_int(CURL_PREREQFUNC_OK);
        };
        let args = vec![
            Value::Object(obj),
            Value::from(primary_ip),
            Value::from(local_ip),
            Value::Int(i64::from(primary_port)),
            Value::Int(i64::from(local_port)),
        ];
        let Some(v) = self.call(&curl, cb, args) else {
            return abort;
        };
        self.verify(&curl);
        match v {
            Value::Int(l) if l == CURL_PREREQFUNC_OK || l == CURL_PREREQFUNC_ABORT => as_c_int(l),
            Value::Int(_) => {
                let t = self.vm.throw("ValueError", PREREQ_MSG);
                self.fail(t);
                abort
            }
            _ => {
                let t = self.vm.throw("TypeError", PREREQ_MSG);
                self.fail(t);
                abort
            }
        }
    }

    fn ssh_hostkey(&mut self, key: u64, keytype: i32, hostkey: &[u8]) -> i32 {
        let mismatch = as_c_int(CURLKHMATCH_MISMATCH);
        let Some((obj, curl)) = self.lookup(key) else {
            return mismatch;
        };
        let Some(cb) = curl.st.borrow().sshhostkey.clone() else {
            return mismatch;
        };
        let args = vec![
            Value::Object(obj),
            Value::Int(i64::from(keytype)),
            Value::from(hostkey),
            Value::Int(i64::try_from(hostkey.len()).unwrap_or(0)),
        ];
        let Some(v) = self.call(&curl, cb, args) else {
            return mismatch;
        };
        self.verify(&curl);
        match v {
            Value::Int(l) if l == CURLKHMATCH_OK || l == CURLKHMATCH_MISMATCH => as_c_int(l),
            _ => {
                let t = self.vm.throw(
                    "Error",
                    "The CURLOPT_SSH_HOSTKEYFUNCTION callback must return either CURLKHMATCH_OK or CURLKHMATCH_MISMATCH",
                );
                self.fail(t);
                mismatch
            }
        }
    }

    fn push(&mut self, parent: u64, child: Easy, headers: Vec<Vec<u8>>) -> bool {
        let Some((mobj, m)) = self.multi.clone() else {
            child.disown();
            return false;
        };
        let _ = mobj;
        let Some((pobj, pcurl)) = self.lookup(parent) else {
            child.disown();
            return false;
        };
        let Some(cb) = m.push.borrow().clone() else {
            child.disown();
            return false;
        };
        child.share_retained_with(&pcurl.easy);
        let st = crate::easy::copy_state(&pcurl.st.borrow());
        let child_curl = Curl::new(child, st);
        let wrapped = crate::easy::wrap_curl(self.vm, Rc::clone(&child_curl));
        let Some(cobj) = self.check(wrapped) else {
            child_curl.easy.disown();
            return false;
        };
        let mut arr = ZArray::new();
        for h in headers {
            let _ = arr.make_mut().push(Value::from(h.as_slice()));
        }
        let args = vec![
            Value::Object(pobj),
            Value::Object(cobj.clone()),
            Value::Array(arr),
        ];
        let accepted = if self.pending.is_some() {
            None
        } else {
            let r = self.vm.call_value(&cb, args);
            self.check(r)
        };
        let ok = match accepted {
            Some(v) => {
                let l = get_long(self.vm, &v);
                self.check(l).is_some_and(|l| l != CURL_PUSH_DENY)
            }
            None => false,
        };
        if ok {
            m.easyh.borrow_mut().push(cobj.clone());
            self.entries.push(Entry {
                key: child_curl.easy.key(),
                obj: cobj,
                curl: child_curl,
            });
        } else {
            child_curl.easy.disown();
        }
        ok
    }
}

impl Dispatch<'_> {
    fn progress_result(&mut self, curl: &Curl, cb: Value, args: Vec<Value>) -> i32 {
        let Some(v) = self.call(curl, cb, args) else {
            return 1;
        };
        self.verify(curl);
        let r = get_long(self.vm, &v);
        i32::from(self.check(r) != Some(0))
    }
}

/// Runs the destructors of objects released by native code right away
/// (the VM otherwise runs them after the next opcode).
pub(crate) fn run_destructors(vm: &mut Vm) -> VmResult<()> {
    while php_vm::object::has_pending_destructors() {
        for o in php_vm::object::take_pending_destructors() {
            let has = php_vm::object::class_of(&o).magic.destruct.is_some();
            if has {
                vm.call_method_by_name(&o, "__destruct", Vec::new())?;
            }
        }
    }
    Ok(())
}

/// `_php_curl_verify_handlers()`: resets handlers whose stream resource was
/// closed, warning when `report`.
pub(crate) fn verify_handlers(vm: &mut Vm, curl: &Curl, report: bool) -> VmResult<()> {
    let gone = |v: &Option<Value>| match v {
        Some(Value::Resource(r)) => !streams::is_open(r),
        _ => false,
    };
    let (e, i, h, f) = {
        let st = curl.st.borrow();
        (
            gone(&st.std_err),
            gone(&st.read.stream),
            gone(&st.write_header.stream),
            gone(&st.write.stream),
        )
    };
    if e {
        if report {
            vm.php_warning("CURLOPT_STDERR resource has gone away, resetting to stderr")?;
        }
        curl.st.borrow_mut().std_err = None;
        curl.easy.set_stderr(None);
    }
    if i {
        if report {
            vm.php_warning("CURLOPT_INFILE resource has gone away, resetting to default")?;
        }
        curl.st.borrow_mut().read.stream = None;
    }
    if h {
        if report {
            vm.php_warning("CURLOPT_WRITEHEADER resource has gone away, resetting to default")?;
        }
        let mut st = curl.st.borrow_mut();
        st.write_header.stream = None;
        st.write_header.method = Method::Ignore;
    }
    if f {
        if report {
            vm.php_warning("CURLOPT_FILE resource has gone away, resetting to default")?;
        }
        let mut st = curl.st.borrow_mut();
        st.write.stream = None;
        st.write.method = Method::Stdout;
    }
    Ok(())
}
