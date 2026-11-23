// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A minimal engine for exercising the extension without the VM.

#![allow(dead_code, clippy::missing_panics_doc, clippy::must_use_candidate)]

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

use php_runtime::array::ArrayKey;
use php_runtime::{
    ErrorLevel, Obj, ObjectDump, ObjectKind, PhpObject, PropertyPurpose, PropertySlot, RtResult,
    RuntimeContext, RuntimeError, Value, ZArray, ZStr,
};
use php_soap::classes::{find_class, PropDef};
use php_soap::host::{
    ConnectTarget, EnumKind, HttpConnector, HttpStream, LocalTime, MethodCall, SoapHost,
};
use php_soap::state::{create_state, SoapObject};

/// An object of the test engine.
#[derive(Debug)]
pub struct TestObj {
    handle: u32,
    class: ZStr,
    pub props: RefCell<ZArray>,
    state: RefCell<Option<Rc<SoapObject>>>,
}

impl PhpObject for TestObj {
    fn handle(&self) -> u32 {
        self.handle
    }
    fn class_name(&self) -> ZStr {
        self.class.clone()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub type MethodFn = Rc<dyn Fn(&mut TestHost, &Obj, &[Value]) -> RtResult<Value>>;
pub type FuncFn = Rc<dyn Fn(&mut TestHost, &[Value]) -> RtResult<Value>>;

/// A user class.
#[derive(Clone, Default)]
pub struct UserClass {
    pub name: String,
    pub parent: Option<String>,
    pub props: Vec<(String, Value)>,
    pub methods: Vec<(String, MethodFn)>,
}

/// A PHP built-in web server (`php -S`) emulated in-process: every request
/// runs `code` in a fresh interpreter (see `php_cli_server_start`).
#[derive(Debug, Clone)]
pub struct CliServer {
    pub code: String,
    /// The script file name the code is stored under (`index.php` or the router).
    pub script: String,
    pub doc_root: std::path::PathBuf,
}

/// Serves one raw HTTP request; returns the raw response and the files the
/// script wrote.
pub type CliHandler = fn(&CliServer, u16, Vec<u8>) -> (Vec<u8>, HashMap<String, Vec<u8>>);

/// Canned HTTP exchanges.
#[derive(Debug, Default, Clone)]
pub struct FakeNet {
    pub responses: Vec<Vec<u8>>,
    pub requests: Rc<RefCell<Vec<Vec<u8>>>>,
    pub targets: Rc<RefCell<Vec<ConnectTarget>>>,
    /// Emulated `php -S` servers by port.
    pub servers: HashMap<u16, CliServer>,
    /// Files written by emulated servers (shared "disk").
    pub server_files: Rc<RefCell<HashMap<String, Vec<u8>>>>,
    pub cli_handler: Option<CliHandler>,
}

impl FakeNet {
    fn server_for(&self, host: &str, port: u16) -> Option<(CliServer, CliHandler)> {
        if !matches!(host, "localhost" | "127.0.0.1") {
            return None;
        }
        Some((self.servers.get(&port)?.clone(), self.cli_handler?))
    }

    /// Performs one request against an emulated server.
    pub fn exchange(&self, host: &str, port: u16, request: Vec<u8>) -> Option<Vec<u8>> {
        let (server, handler) = self.server_for(host, port)?;
        let (resp, files) = handler(&server, port, request);
        self.server_files.borrow_mut().extend(files);
        Some(resp)
    }
}

/// A connection to an emulated `php -S` server: the request is buffered and
/// served on the first read.
struct CliStream {
    server: CliServer,
    port: u16,
    handler: CliHandler,
    request: Vec<u8>,
    response: Option<FakeStream>,
    files: Rc<RefCell<HashMap<String, Vec<u8>>>>,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl CliStream {
    fn response(&mut self) -> &mut FakeStream {
        if self.response.is_none() {
            let req = std::mem::take(&mut self.request);
            let (data, files) = (self.handler)(&self.server, self.port, req);
            self.files.borrow_mut().extend(files);
            self.response = Some(FakeStream {
                data,
                pos: 0,
                log: Rc::clone(&self.log),
            });
        }
        self.response.as_mut().expect("response")
    }
}

impl HttpStream for CliStream {
    fn write(&mut self, data: &[u8]) -> usize {
        self.log.borrow_mut().push(data.to_vec());
        self.request.extend_from_slice(data);
        data.len()
    }
    fn read_line(&mut self) -> Option<Vec<u8>> {
        self.response().read_line()
    }
    fn read(&mut self, n: usize) -> Vec<u8> {
        self.response().read(n)
    }
    fn eof(&mut self) -> bool {
        self.response().eof()
    }
}

/// The payload of a `stream-context` resource.
#[derive(Debug, Default)]
pub struct StreamContext(pub RefCell<Vec<(String, String, Value)>>);

struct FakeStream {
    data: Vec<u8>,
    pos: usize,
    log: Rc<RefCell<Vec<Vec<u8>>>>,
}

impl HttpStream for FakeStream {
    fn write(&mut self, data: &[u8]) -> usize {
        self.log.borrow_mut().push(data.to_vec());
        data.len()
    }
    fn read_line(&mut self) -> Option<Vec<u8>> {
        if self.pos >= self.data.len() {
            return None;
        }
        let end = self.data[self.pos..]
            .iter()
            .position(|&c| c == b'\n')
            .map_or(self.data.len(), |p| self.pos + p + 1);
        let line = self.data[self.pos..end].to_vec();
        self.pos = end;
        Some(line)
    }
    fn read(&mut self, n: usize) -> Vec<u8> {
        let end = (self.pos + n).min(self.data.len());
        let out = self.data[self.pos..end].to_vec();
        self.pos = end;
        out
    }
    fn eof(&mut self) -> bool {
        self.pos >= self.data.len()
    }
}

impl HttpConnector for FakeNet {
    fn connect(&mut self, target: &ConnectTarget) -> Option<Box<dyn HttpStream>> {
        self.targets.borrow_mut().push(target.clone());
        if let Some((server, handler)) = self.server_for(&target.host, target.port) {
            return Some(Box::new(CliStream {
                server,
                port: target.port,
                handler,
                request: Vec::new(),
                response: None,
                files: Rc::clone(&self.server_files),
                log: Rc::clone(&self.requests),
            }));
        }
        if self.responses.is_empty() {
            return None;
        }
        let data = self.responses.remove(0);
        Some(Box::new(FakeStream {
            data,
            pos: 0,
            log: Rc::clone(&self.requests),
        }))
    }
}

/// The test engine.
#[derive(Default)]
pub struct TestHost {
    next_handle: u32,
    pub classes: HashMap<String, UserClass>,
    pub functions: HashMap<String, (String, FuncFn)>,
    pub output: Vec<u8>,
    pub ob: Vec<Vec<u8>>,
    pub headers: Vec<String>,
    pub files: HashMap<String, Vec<u8>>,
    pub ini: HashMap<String, Vec<u8>>,
    pub globals: HashMap<String, Value>,
    pub net: FakeNet,
    pub tz_offset: i64,
    pub server_vars: HashMap<String, Vec<u8>>,
    pub request_body: Option<Vec<u8>>,
    /// Request headers in arrival order (`getallheaders()`).
    pub request_headers: Vec<(String, String)>,
    /// `http_response_code()`.
    pub response_code: Option<i64>,
    pub now: i64,
    /// File name used in error messages.
    pub script: String,
    /// Directory relative includes resolve against.
    pub base_dir: std::path::PathBuf,
    /// The file being executed (`__FILE__`).
    pub current_file: std::path::PathBuf,
    /// Replacement sources for included files (by base name).
    pub include_override: HashMap<String, String>,
    /// User call depth (simulates the memory limit on runaway recursion).
    pub depth: usize,
    /// Parameter names of user functions/methods (`func` or `class::method`, lower-case).
    pub param_names: HashMap<String, Vec<String>>,
    /// Builtin functions of the interpreter.
    pub builtin_hook: Option<fn(&mut TestHost, &str, &[Value]) -> Option<RtResult<Value>>>,
    /// Internal methods of the interpreter.
    pub method_hook: Option<fn(&mut TestHost, &Obj, &str, &[Value]) -> Option<RtResult<Value>>>,
}

impl TestHost {
    pub fn new() -> Self {
        let mut h = Self {
            next_handle: 1,
            now: 1_000_000,
            script: "test.php".into(),
            ..Self::default()
        };
        h.ini
            .insert("soap.wsdl_cache_enabled".into(), b"0".to_vec());
        h
    }

    /// `echo`.
    pub fn echo(&mut self, data: &[u8]) {
        self.write_output(data);
    }

    pub fn add_function(
        &mut self,
        name: &str,
        f: impl Fn(&mut TestHost, &[Value]) -> RtResult<Value> + 'static,
    ) {
        self.functions
            .insert(name.to_ascii_lowercase(), (name.to_owned(), Rc::new(f)));
    }

    pub fn add_class(&mut self, c: UserClass) {
        self.classes.insert(c.name.to_ascii_lowercase(), c);
    }

    fn parent_of(&self, class: &str) -> Option<String> {
        let lc = class.to_ascii_lowercase();
        if let Some(c) = self.classes.get(&lc) {
            return c.parent.clone();
        }
        let p = match lc.as_str() {
            "soapfault" => "Exception",
            "typeerror" | "valueerror" | "arithmeticerror" | "argumentcounterror" => "Error",
            "divisionbyzeroerror" => "ArithmeticError",
            "exception" | "error" => "Throwable",
            _ => return find_class(class).and_then(|c| c.parent.map(str::to_owned)),
        };
        Some(p.to_owned())
    }

    pub fn is_a(&self, class: &str, target: &str) -> bool {
        let mut cur = Some(class.to_owned());
        while let Some(c) = cur {
            if c.eq_ignore_ascii_case(target) {
                return true;
            }
            cur = self.parent_of(&c);
        }
        false
    }

    fn declared_props(&self, class: &str) -> Vec<(String, Option<Value>)> {
        let mut chain = Vec::new();
        let mut cur = Some(class.to_owned());
        while let Some(c) = cur {
            chain.push(c.clone());
            cur = self.parent_of(&c);
        }
        let mut out = Vec::new();
        for c in chain.iter().rev() {
            if c.eq_ignore_ascii_case("Exception") || c.eq_ignore_ascii_case("Error") {
                out.push(("message".into(), Some(Value::from(""))));
                out.push(("string".into(), Some(Value::from(""))));
                out.push(("code".into(), Some(Value::Int(0))));
                out.push(("file".into(), Some(Value::from("test.php"))));
                out.push(("line".into(), Some(Value::Int(1))));
                out.push(("trace".into(), Some(Value::Array(ZArray::new()))));
                out.push(("previous".into(), Some(Value::Null)));
            }
            if let Some(def) = find_class(c) {
                for p in def.properties {
                    out.push((p.name.to_owned(), default_of(p)));
                }
            }
            if let Some(uc) = self.classes.get(&c.to_ascii_lowercase()) {
                for (n, v) in &uc.props {
                    out.push((n.clone(), Some(v.clone())));
                }
            }
        }
        out
    }

    fn canonical_class(&self, name: &str) -> Option<String> {
        if let Some(c) = self.classes.get(&name.to_ascii_lowercase()) {
            return Some(c.name.clone());
        }
        if let Some(c) = find_class(name) {
            return Some(c.name.to_owned());
        }
        for b in ["stdClass", "Exception", "Error", "TypeError", "ValueError"] {
            if b.eq_ignore_ascii_case(name) {
                return Some(b.to_owned());
            }
        }
        None
    }

    /// Creates an object of `class` (declared props initialized).
    pub fn new_object(&mut self, class: &str) -> Obj {
        let class = self
            .canonical_class(class)
            .unwrap_or_else(|| class.to_owned());
        let mut props = ZArray::new();
        for (n, v) in self.declared_props(&class) {
            if let Some(v) = v {
                props
                    .make_mut()
                    .insert(ArrayKey::Str(ZStr::from(n.as_str())), v);
            }
        }
        let mut base = class.clone();
        let mut state = None;
        loop {
            if let Some(s) = create_state(&base) {
                state = Some(s);
                break;
            }
            match self.parent_of(&base) {
                Some(p) => base = p,
                None => break,
            }
        }
        let h = self.next_handle;
        self.next_handle += 1;
        Obj::new(TestObj {
            handle: h,
            class: ZStr::from(class.as_str()),
            props: RefCell::new(props),
            state: RefCell::new(state),
        })
    }

    pub fn props_of(obj: &Obj) -> &RefCell<ZArray> {
        &obj.downcast_ref::<TestObj>().expect("test object").props
    }

    /// The `memory_limit` INI value in bytes (0 = unlimited).
    pub fn memory_limit(&self) -> usize {
        let v = self.ini.get("memory_limit").map_or_else(
            || "128M".to_owned(),
            |v| String::from_utf8_lossy(v).into_owned(),
        );
        let v = v.trim();
        if v.starts_with('-') {
            return 0;
        }
        let (num, mul) = match v.as_bytes().last() {
            Some(b'M' | b'm') => (&v[..v.len() - 1], 1024 * 1024),
            Some(b'K' | b'k') => (&v[..v.len() - 1], 1024),
            Some(b'G' | b'g') => (&v[..v.len() - 1], 1024 * 1024 * 1024),
            _ => (v, 1),
        };
        num.parse::<usize>().unwrap_or(0) * mul
    }

    /// Whether a class exists.
    pub fn class_exists(&self, name: &str) -> bool {
        self.canonical_class(name).is_some()
    }

    /// The declared spelling of a class name.
    pub fn canonical(&self, name: &str) -> String {
        self.canonical_class(name)
            .unwrap_or_else(|| name.to_owned())
    }

    /// A user method in the class chain.
    pub fn find_user_method(&self, class: &str, name: &str) -> Option<MethodFn> {
        self.find_method(class, name)
    }

    fn find_method(&self, class: &str, name: &str) -> Option<MethodFn> {
        let mut cur = Some(class.to_owned());
        while let Some(c) = cur {
            if let Some(uc) = self.classes.get(&c.to_ascii_lowercase()) {
                if let Some((_, f)) = uc
                    .methods
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                {
                    return Some(Rc::clone(f));
                }
            }
            cur = self.parent_of(&c);
        }
        None
    }

    /// `var_dump`.
    pub fn var_dump(&mut self, v: &Value) {
        let mut out = Vec::new();
        php_runtime::dump::var_dump(&mut out, v, self).expect("var_dump");
        self.echo(&out);
    }
}

fn default_of(p: &PropDef) -> Option<Value> {
    match p.default? {
        "null" => Some(Value::Null),
        "false" => Some(Value::Bool(false)),
        "true" => Some(Value::Bool(true)),
        "0" => Some(Value::Int(0)),
        "[]" => Some(Value::Array(ZArray::new())),
        "\"\"" => Some(Value::from("")),
        _ => Some(Value::Null),
    }
}

impl RuntimeContext for TestHost {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        if php_soap::globals::error_disposition(level) == php_soap::globals::Disposition::Suppress {
            return Ok(());
        }
        let label = match level {
            ErrorLevel::Warning => "Warning",
            ErrorLevel::Deprecated => "Deprecated",
            ErrorLevel::Notice => "Notice",
            _ => "Error",
        };
        let s = format!("\n{label}: {message} in {} on line 1\n", self.script);
        self.echo(s.as_bytes());
        Ok(())
    }

    fn object_properties(
        &mut self,
        obj: &Obj,
        _purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        Ok(obj
            .downcast_ref::<TestObj>()
            .map(|o| o.props.borrow().clone()))
    }

    fn object_dump(&mut self, obj: &Obj, _purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        let props = obj
            .downcast_ref::<TestObj>()
            .map(|o| o.props.borrow().clone());
        let kind = if obj.class_name().eq_ignore_ascii_case(b"stdClass") {
            ObjectKind::StdClass
        } else {
            ObjectKind::Regular { lazy_prefix: "" }
        };
        Ok(ObjectDump {
            kind,
            properties: props.map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                    .collect()
            }),
        })
    }
}

impl SoapHost for TestHost {
    fn soap_state(&mut self, obj: &Obj) -> Option<Rc<SoapObject>> {
        obj.downcast_ref::<TestObj>()
            .and_then(|o| o.state.borrow().clone())
    }

    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool {
        if class.eq_ignore_ascii_case("Traversable")
            || class.eq_ignore_ascii_case("DateTimeInterface")
        {
            return false;
        }
        self.is_a(&obj.class_name().to_string_lossy(), class)
    }

    fn fetch_class(&mut self, name: &[u8]) -> RtResult<Option<ZStr>> {
        let n = String::from_utf8_lossy(name);
        match self.canonical_class(&n) {
            Some(c) => Ok(Some(ZStr::from(c.as_str()))),
            None => Err(RuntimeError::Error(format!("Class \"{n}\" not found"))),
        }
    }

    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj> {
        Ok(self.new_object(&class.to_string_lossy()))
    }

    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj> {
        let o = self.new_object(&class.to_string_lossy());
        if let Some(ctor) = self.find_method(&class.to_string_lossy(), "__construct") {
            ctor(self, &o, args)?;
        }
        Ok(o)
    }

    fn set_object_class(&mut self, _obj: &Obj, _class: &ZStr) {}

    fn read_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<Option<Value>> {
        Ok(Self::props_of(obj).borrow().get_str(name).cloned())
    }

    fn write_property(&mut self, obj: &Obj, name: &[u8], value: Value) -> RtResult<()> {
        Self::props_of(obj)
            .borrow_mut()
            .make_mut()
            .insert(ArrayKey::Str(ZStr::from(name)), value);
        Ok(())
    }

    fn unset_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<()> {
        Self::props_of(obj)
            .borrow_mut()
            .make_mut()
            .remove(&ArrayKey::Str(ZStr::from(name)));
        Ok(())
    }

    fn property_table(&mut self, obj: &Obj) -> ZArray {
        Self::props_of(obj).borrow().clone()
    }

    fn read_slot(&mut self, obj: &Obj, name: &str) -> Option<Value> {
        Self::props_of(obj)
            .borrow()
            .get_str(name.as_bytes())
            .cloned()
    }

    fn write_slot(&mut self, obj: &Obj, name: &str, value: Value) {
        Self::props_of(obj)
            .borrow_mut()
            .make_mut()
            .insert(ArrayKey::Str(ZStr::from(name)), value);
    }

    fn enum_kind(&mut self, _obj: &Obj) -> EnumKind {
        EnumKind::NotEnum
    }

    fn localtime(&mut self, ts: i64) -> Option<LocalTime> {
        let mut t = LocalTime::utc(ts + self.tz_offset);
        t.gmtoff = self.tz_offset;
        Some(t)
    }

    fn exception_object(&mut self, err: &RuntimeError) -> Option<Obj> {
        let (class, msg) = match err {
            RuntimeError::Thrown(Value::Object(o)) => return Some(o.clone()),
            RuntimeError::Error(m) => ("Error", m.clone()),
            RuntimeError::TypeError(m) => ("TypeError", m.clone()),
            RuntimeError::ValueError(m) => ("ValueError", m.clone()),
            RuntimeError::Custom { class, message } => (
                &*Box::leak(class.to_string_lossy().into_boxed_str()),
                message.clone(),
            ),
            _ => return None,
        };
        let o = self.new_object(class);
        self.write_slot(&o, "message", Value::from(msg.as_str()));
        Some(o)
    }

    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value> {
        if let Value::Array(a) = callable.deref() {
            if let (Some(Value::Object(o)), Some(Value::Str(m))) = (
                a.get_int(0).map(Value::deref),
                a.get_int(1).map(Value::deref),
            ) {
                if let Some(f) =
                    self.find_method(&o.class_name().to_string_lossy(), &m.to_string_lossy())
                {
                    return f(self, &o, args);
                }
            }
        }
        let name = callable.to_zstr(self)?;
        match self.call_function(name.as_bytes(), args)? {
            Some(v) => Ok(v),
            None => Err(RuntimeError::Error(format!(
                "Invalid callback {}, function \"{}\" not found or invalid function name",
                name.to_string_lossy(),
                name.to_string_lossy()
            ))),
        }
    }

    fn call_function(&mut self, name: &[u8], args: &[Value]) -> RtResult<Option<Value>> {
        let key = String::from_utf8_lossy(name).to_ascii_lowercase();
        let Some((_, f)) = self.functions.get(&key).cloned() else {
            if let Some(hook) = self.builtin_hook {
                return hook(self, &key, args).transpose();
            }
            return Ok(None);
        };
        f(self, args).map(Some)
    }

    fn function_exists(&mut self, name: &[u8]) -> Option<ZStr> {
        let key = String::from_utf8_lossy(name).to_ascii_lowercase();
        if let Some((n, _)) = self.functions.get(&key) {
            return Some(ZStr::from(n.as_str()));
        }
        const BUILTINS: &[&str] = &[
            "strlen",
            "strrev",
            "count",
            "var_dump",
            "gzinflate",
            "gzdeflate",
            "gzuncompress",
            "gzcompress",
            "gzencode",
            "gzdecode",
            "zlib_decode",
            "str_repeat",
            "trim",
            "strtolower",
            "strtoupper",
            "base64_encode",
            "md5",
        ];
        BUILTINS.iter().find(|b| **b == key).map(|b| ZStr::from(*b))
    }

    fn defined_functions(&mut self) -> Vec<ZStr> {
        self.functions
            .values()
            .map(|(n, _)| ZStr::from(n.as_str()))
            .collect()
    }

    fn class_methods(&mut self, class: &ZStr) -> Vec<(ZStr, bool)> {
        let mut out = Vec::new();
        let mut cur = Some(class.to_string_lossy());
        while let Some(c) = cur {
            if let Some(uc) = self.classes.get(&c.to_ascii_lowercase()) {
                for (n, _) in &uc.methods {
                    out.push((ZStr::from(n.as_str()), true));
                }
            }
            cur = self.parent_of(&c);
        }
        out
    }

    fn call_method(&mut self, obj: &Obj, name: &[u8], args: &[Value]) -> RtResult<MethodCall> {
        let n = String::from_utf8_lossy(name).into_owned();
        let class = obj.class_name().to_string_lossy();
        if let Some(m) = self.find_method(&class, &n) {
            return m(self, obj, args).map(MethodCall::Returned);
        }
        if n.eq_ignore_ascii_case("__doRequest") && self.is_a(&class, "SoapClient") {
            let s = |i: usize| -> Vec<u8> {
                match args.get(i).map(Value::deref) {
                    Some(Value::Str(s)) => s.as_bytes().to_vec(),
                    _ => Vec::new(),
                }
            };
            let version = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => 1,
            };
            let one_way = matches!(args.get(4), Some(Value::Bool(true)));
            let r = php_soap::client::do_request_method(
                self,
                obj,
                &s(0),
                &s(1),
                &s(2),
                version,
                one_way,
                None,
            )?;
            return Ok(MethodCall::Returned(r));
        }
        if let Some(hook) = self.method_hook {
            if !n.eq_ignore_ascii_case("__doRequest") {
                if let Some(r) = hook(self, obj, &n, args) {
                    return r.map(MethodCall::Returned);
                }
            }
        }
        if let Some(m) = self.find_method(&class, "__call") {
            let a = ZArray::from_values(args.iter().cloned());
            return m(self, obj, &[Value::from(n.as_str()), Value::Array(a)])
                .map(MethodCall::Returned);
        }
        if n.eq_ignore_ascii_case("getTraceAsString") {
            return Ok(MethodCall::Returned(Value::from("#0 {main}")));
        }
        Ok(MethodCall::NotCallable(format!(
            "class {class} does not have a method \"{n}\""
        )))
    }

    fn load_resource(&mut self, url: &str, context: Option<&Value>) -> Result<Vec<u8>, String> {
        if let Some(f) = self.files.get(url) {
            return Ok(f.clone());
        }
        if let Some(f) = self.net.server_files.borrow().get(url) {
            return Ok(f.clone());
        }
        if url.starts_with("http://") {
            if let Some(body) = self.http_get(url, context) {
                return Ok(body);
            }
        }
        let path = url.strip_prefix("file://").unwrap_or(url);
        if !url.contains("://") || url.starts_with("file://") {
            if let Ok(d) = std::fs::read(path) {
                return Ok(d);
            }
        }
        Err(format!("failed to load external entity \"{url}\"\n"))
    }

    fn realpath(&mut self, path: &str) -> Option<String> {
        if self.files.contains_key(path) {
            return Some(path.to_owned());
        }
        let p = format!("/tests/{path}");
        if self.files.contains_key(&p) {
            return Some(p);
        }
        let joined = self.base_dir.join(path);
        std::fs::canonicalize(joined)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }

    fn now(&mut self) -> i64 {
        self.now
    }

    fn write_output(&mut self, data: &[u8]) {
        match self.ob.last_mut() {
            Some(b) => b.extend_from_slice(data),
            None => self.output.extend_from_slice(data),
        }
    }

    fn add_header(&mut self, line: &str) {
        self.headers.push(line.to_owned());
    }

    fn ob_start(&mut self) -> bool {
        self.ob.push(Vec::new());
        true
    }

    fn ob_contents(&mut self) -> Option<Vec<u8>> {
        self.ob.last().cloned()
    }

    fn ob_discard(&mut self) {
        self.ob.pop();
    }

    fn server_var(&mut self, name: &str) -> Option<Vec<u8>> {
        self.server_vars.get(name).cloned()
    }

    fn request_line(&mut self) -> (Option<String>, Option<String>) {
        (
            self.server_vars
                .get("REQUEST_METHOD")
                .map(|v| String::from_utf8_lossy(v).into_owned()),
            self.server_vars
                .get("QUERY_STRING")
                .map(|v| String::from_utf8_lossy(v).into_owned()),
        )
    }

    fn request_body(&mut self) -> Option<Vec<u8>> {
        self.request_body.clone()
    }

    fn ini_get(&mut self, name: &str) -> Option<Vec<u8>> {
        self.ini.get(name).cloned()
    }

    fn context_create(&mut self) -> Value {
        Value::Resource(php_runtime::Resource::new(
            "stream-context",
            Some(Box::new(StreamContext::default())),
        ))
    }

    fn context_get_option(
        &mut self,
        context: &Value,
        wrapper: &str,
        option: &str,
    ) -> Option<Value> {
        let Value::Resource(r) = context.deref() else {
            return None;
        };
        let data = r.data();
        let ctx = data.as_ref()?.downcast_ref::<StreamContext>()?;
        let opts = ctx.0.borrow();
        opts.iter()
            .find(|(w, o, _)| w == wrapper && o == option)
            .map(|(_, _, v)| v.clone())
    }

    fn context_set_option(&mut self, context: &Value, wrapper: &str, option: &str, value: Value) {
        let Value::Resource(r) = context.deref() else {
            return;
        };
        let data = r.data();
        let Some(ctx) = data
            .as_ref()
            .and_then(|d| d.downcast_ref::<StreamContext>())
        else {
            return;
        };
        let mut opts = ctx.0.borrow_mut();
        if let Some(slot) = opts
            .iter_mut()
            .find(|(w, o, _)| w == wrapper && o == option)
        {
            slot.2 = value;
        } else {
            opts.push((wrapper.to_owned(), option.to_owned(), value));
        }
    }

    fn connector(&mut self) -> Option<&mut dyn HttpConnector> {
        Some(&mut self.net)
    }

    fn bailout(&mut self) -> RuntimeError {
        RuntimeError::Custom {
            class: ZStr::from("__exit__"),
            message: String::new(),
        }
    }

    fn is_unwind_exit(&mut self, err: &RuntimeError) -> bool {
        matches!(err, RuntimeError::Custom { class, .. } if class.as_bytes() == b"__exit__")
    }
}

impl TestHost {
    /// Ends all output buffers (script end) and returns the output.
    /// The `http://` stream wrapper against an emulated server
    /// (`php_stream_url_wrap_http_ex` request layout).
    pub fn http_get(&mut self, url: &str, context: Option<&Value>) -> Option<Vec<u8>> {
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = rest
            .find('/')
            .map_or((rest, "/"), |p| (&rest[..p], &rest[p..]));
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().ok()?),
            None => (authority, 80u16),
        };
        let opt =
            |h: &mut Self, name: &str| context.and_then(|c| h.context_get_option(c, "http", name));
        let protocol = match opt(self, "protocol_version").map(|v| v.deref()) {
            Some(Value::Float(f)) => format!("{f:.1}"),
            Some(Value::Str(s)) => String::from_utf8_lossy(s.as_bytes()).into_owned(),
            _ => "1.1".to_owned(),
        };
        let mut user = Vec::new();
        match opt(self, "header").map(|v| v.deref()) {
            Some(Value::Str(s)) => user.extend_from_slice(s.as_bytes()),
            Some(Value::Array(a)) => {
                for v in a.values() {
                    if let Value::Str(s) = v.deref() {
                        user.extend_from_slice(s.as_bytes());
                        user.extend_from_slice(b"\r\n");
                    }
                }
            }
            _ => {}
        }
        let user = String::from_utf8_lossy(&user)
            .trim_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n' | '\0' | '\x0b'))
            .to_owned();
        let has = |name: &str| {
            user.split('\n').any(|l| {
                l.trim_start()
                    .to_ascii_lowercase()
                    .starts_with(&format!("{name}:"))
            })
        };
        let mut req = format!("GET {path} HTTP/{protocol}\r\n");
        if !has("host") {
            if port == 80 {
                let _ = write!(req, "Host: {host}\r\n");
            } else {
                let _ = write!(req, "Host: {host}:{port}\r\n");
            }
        }
        if !has("connection") {
            req.push_str("Connection: close\r\n");
        }
        if !user.is_empty() {
            req.push_str(&user);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        let resp = self.net.exchange(host, port, req.into_bytes())?;
        let split = resp.windows(4).position(|w| w == b"\r\n\r\n")?;
        let status = resp.split(|&c| c == b' ').nth(1)?;
        if !status.starts_with(b"2") {
            return None;
        }
        Some(resp[split + 4..].to_vec())
    }

    pub fn finish(&mut self) -> Vec<u8> {
        while let Some(b) = self.ob.pop() {
            self.write_output(&b);
        }
        std::mem::take(&mut self.output)
    }

    /// `ob_get_clean`.
    pub fn ob_get_clean(&mut self) -> Vec<u8> {
        self.ob.pop().unwrap_or_default()
    }

    /// Prints an uncaught error like the CLI does.
    pub fn report(&mut self, e: &RuntimeError) {
        let file = if self.script.is_empty() {
            "test.php".to_owned()
        } else {
            self.script.clone()
        };
        match e {
            RuntimeError::Fatal(m) => {
                let s = format!("\nFatal error: {m} in {file} on line 1\n");
                self.echo(s.as_bytes());
            }
            RuntimeError::Custom { class, .. } if class.as_bytes() == b"__exit__" => {}
            other => {
                let o = self.exception_object(other);
                let text = match &o {
                    Some(o) => {
                        let class = o.class_name().to_string_lossy();
                        let msg = self
                            .read_slot(o, "message")
                            .map(|m| {
                                m.to_zstr(self)
                                    .map(|z| z.to_string_lossy())
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        if self.is_a(&class, "SoapFault") {
                            let code = self
                                .read_slot(o, "faultcode")
                                .map(|m| {
                                    m.to_zstr(self)
                                        .map(|z| z.to_string_lossy())
                                        .unwrap_or_default()
                                })
                                .unwrap_or_default();
                            format!("SoapFault exception: [{code}] {msg} in {file}:1\nStack trace:\n#0 {{main}}")
                        } else {
                            format!("{class}: {msg} in {file}:1\nStack trace:\n#0 {{main}}")
                        }
                    }
                    None => format!("Error: {other:?} in {file}:1\nStack trace:\n#0 {{main}}"),
                };
                let s =
                    format!("PHP Fatal error:  Uncaught {text}\n  thrown in {file} on line 1\n");
                let _ = s;
                let s = format!("\nFatal error: Uncaught {text}\n  thrown in {file} on line 1\n");
                self.echo(s.as_bytes());
            }
        }
    }

    /// `new SoapClient($wsdl, $options)`.
    pub fn new_client(
        &mut self,
        class: &str,
        wsdl: Option<&str>,
        options: &ZArray,
    ) -> RtResult<Obj> {
        let o = self.new_object(class);
        php_soap::client::construct(self, &o, wsdl.map(str::as_bytes), Some(options))?;
        Ok(o)
    }

    /// `new SoapServer($wsdl, $options)`.
    pub fn new_server(&mut self, wsdl: Option<&str>, options: &ZArray) -> RtResult<Obj> {
        let o = self.new_object("SoapServer");
        php_soap::server::construct(self, &o, wsdl.map(str::as_bytes), Some(options))?;
        Ok(o)
    }
}
