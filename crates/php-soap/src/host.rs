// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! What the extension needs from the engine.
//!
//! [`SoapHost`] extends [`RuntimeContext`] with object access, calls into
//! user code, output/SAPI hooks and resource loading. Everything
//! object-model-ish (instantiating classmap classes, reading the private
//! properties of `SoapClient`, calling the handler of a `SoapServer`, …)
//! goes through it; the extension itself never touches the VM.
//!
//! Network I/O goes through [`HttpConnector`]/[`HttpStream`] so tests can
//! substitute canned responses.

use std::rc::Rc;

use php_runtime::{Obj, RtResult, RuntimeContext, Value, ZArray, ZStr};

use crate::state::SoapObject;

/// Broken-down local time (`struct tm` + `tm_gmtoff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalTime {
    /// Full year (e.g. 1976).
    pub year: i64,
    /// Month 1–12.
    pub month: u32,
    /// Day of month 1–31.
    pub day: u32,
    /// Hour 0–23.
    pub hour: u32,
    /// Minute 0–59.
    pub minute: u32,
    /// Second 0–60.
    pub second: u32,
    /// Offset from UTC in seconds.
    pub gmtoff: i64,
}

impl LocalTime {
    /// UTC breakdown of a Unix timestamp.
    #[must_use]
    pub fn utc(ts: i64) -> Self {
        let days = ts.div_euclid(86_400);
        let secs = ts.rem_euclid(86_400);
        // Civil-from-days (Howard Hinnant).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        let n = |v: i64| u32::try_from(v).unwrap_or(0);
        Self {
            year,
            month: n(m),
            day: n(d),
            hour: n(secs / 3600),
            minute: n(secs % 3600 / 60),
            second: n(secs % 60),
            gmtoff: 0,
        }
    }
}

/// How an enum case serializes (`get_serialization_*_from_zval`).
#[derive(Debug, Clone)]
pub enum EnumKind {
    /// Not an enum.
    NotEnum,
    /// A pure enum (no backing value).
    Pure,
    /// A backed enum case with its value.
    Backed(Value),
}

/// Result of `zend_call_method_if_exists_ex`.
#[derive(Debug, Clone)]
pub enum MethodCall {
    /// The method ran and returned a value.
    Returned(Value),
    /// The method is not callable; the reason completes
    /// `"Cannot call method C::m(): …"`.
    NotCallable(String),
}

/// Where a SOAP connection goes (`http_connect`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    /// `tcp`, `ssl`, `tls`, `sslv2`, `sslv3`.
    pub protocol: String,
    /// Host (or proxy host).
    pub host: String,
    /// Port.
    pub port: u16,
    /// Connection timeout in seconds, when set.
    pub timeout: Option<i64>,
    /// When tunnelling TLS through a proxy: the `CONNECT` request to send
    /// first and the crypto method to enable afterwards.
    pub proxy_tunnel: Option<(Vec<u8>, String)>,
}

/// A connected byte stream (a `php_stream` socket).
pub trait HttpStream {
    /// Writes all bytes; returns the number written.
    fn write(&mut self, data: &[u8]) -> usize;
    /// `php_stream_gets`: one line including its `\n`, `None` at EOF.
    fn read_line(&mut self) -> Option<Vec<u8>>;
    /// Reads up to `n` bytes (empty at EOF).
    fn read(&mut self, n: usize) -> Vec<u8>;
    /// `php_stream_eof`.
    fn eof(&mut self) -> bool;
}

/// Opens [`HttpStream`]s.
pub trait HttpConnector {
    /// `php_stream_xport_create`; `None` when the connection fails.
    fn connect(&mut self, target: &ConnectTarget) -> Option<Box<dyn HttpStream>>;
}

/// The engine interface used by ext/soap.
#[allow(unused_variables)]
pub trait SoapHost: RuntimeContext {
    // ---- object state ------------------------------------------------------

    /// The extension state attached to an object of a SOAP class
    /// (created with [`crate::state::create_state`] when the object was
    /// instantiated); `None` for other objects.
    fn soap_state(&mut self, obj: &Obj) -> Option<Rc<SoapObject>>;

    // ---- classes and objects -----------------------------------------------

    /// `instanceof_function(Z_OBJCE_P(obj), class)` (case-insensitive name).
    fn instance_of(&mut self, obj: &Obj, class: &str) -> bool;

    /// `zend_fetch_class(name, ZEND_FETCH_CLASS_AUTO)`: the canonical class
    /// name (autoloading). A missing class throws `Error` like PHP does.
    fn fetch_class(&mut self, name: &[u8]) -> RtResult<Option<ZStr>>;

    /// `object_init_ex`: a new instance without calling the constructor.
    fn create_object(&mut self, class: &ZStr) -> RtResult<Obj>;

    /// `object_init_ex` + constructor call with `args`.
    fn instantiate(&mut self, class: &ZStr, args: &[Value]) -> RtResult<Obj>;

    /// Changes the class of an object in place (`zobj->ce = ce`).
    fn set_object_class(&mut self, obj: &Obj, class: &ZStr);

    /// `zend_read_property(scope = class of obj, silent)`: `None` when the
    /// property is undefined (`&EG(uninitialized_zval)`).
    fn read_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<Option<Value>>;

    /// `zend_update_property(scope = class of obj)`.
    fn write_property(&mut self, obj: &Obj, name: &[u8], value: Value) -> RtResult<()>;

    /// `zend_unset_property`.
    fn unset_property(&mut self, obj: &Obj, name: &[u8]) -> RtResult<()>;

    /// `Z_OBJPROP_P`: the property table with mangled keys, without
    /// uninitialized typed properties.
    fn property_table(&mut self, obj: &Obj) -> ZArray;

    /// `OBJ_PROP_NUM` read of a declared property of an internal SOAP
    /// class (`SoapVar::$enc_type`, `SoapClient::$sdl`, …), ignoring
    /// visibility; `None` when uninitialized.
    fn read_slot(&mut self, obj: &Obj, name: &str) -> Option<Value>;

    /// `OBJ_PROP_NUM` write (no type checks, no hooks).
    fn write_slot(&mut self, obj: &Obj, name: &str, value: Value);

    /// Enum information of an object.
    fn enum_kind(&mut self, obj: &Obj) -> EnumKind {
        EnumKind::NotEnum
    }

    /// Iterates a `Traversable` object (`ce->get_iterator`): `(key, value)`
    /// pairs.
    fn iterate(&mut self, obj: &Obj) -> RtResult<Vec<(Value, Value)>> {
        Ok(Vec::new())
    }

    /// `php_format_date_obj` for a `DateTimeInterface`; `None` when the
    /// object is not initialized.
    fn format_date(&mut self, obj: &Obj, format: &str) -> Option<Vec<u8>> {
        None
    }

    /// `php_localtime_r` (system time zone).
    fn localtime(&mut self, ts: i64) -> Option<LocalTime> {
        Some(LocalTime::utc(ts))
    }

    /// The exception object carried by an error (`EG(exception)`): built-in
    /// variants are instantiated; `None` for fatal errors and exit.
    fn exception_object(&mut self, err: &php_runtime::RuntimeError) -> Option<Obj>;

    /// Whether an error is the engine's `exit()` unwinding
    /// (`zend_is_unwind_exit`).
    fn is_unwind_exit(&mut self, err: &php_runtime::RuntimeError) -> bool {
        let _ = err;
        false
    }

    // ---- calls -------------------------------------------------------------

    /// `call_user_function` with a callable value.
    fn call_callable(&mut self, callable: &Value, args: &[Value]) -> RtResult<Value>;

    /// Calls the global function `name` (case-insensitive); `None` when it
    /// does not exist.
    fn call_function(&mut self, name: &[u8], args: &[Value]) -> RtResult<Option<Value>>;

    /// The canonical name of a global function, if defined.
    fn function_exists(&mut self, name: &[u8]) -> Option<ZStr>;

    /// All defined function names (`EG(function_table)` order).
    fn defined_functions(&mut self) -> Vec<ZStr> {
        Vec::new()
    }

    /// Methods of a class: `(name, is_public)` in declaration order.
    fn class_methods(&mut self, class: &ZStr) -> Vec<(ZStr, bool)> {
        Vec::new()
    }

    /// `zend_call_method_if_exists_ex` (honours `__call`).
    fn call_method(&mut self, obj: &Obj, name: &[u8], args: &[Value]) -> RtResult<MethodCall>;

    /// `ZEND_HAS_OOM()` for long native loops: the engine returns its
    /// memory-limit fatal error (`Allowed memory size of N bytes
    /// exhausted`) once an allocation crossed the limit.
    fn memory_check(&mut self) -> RtResult<()> {
        Ok(())
    }

    /// A fallible allocation of `bytes` more (`zend_mm_alloc_huge` checking
    /// `memory_limit` before it allocates): the engine returns its
    /// memory-limit fatal error when the block would not fit, or when the
    /// limit was already crossed (`memory_reserve(0)` only polls).
    fn memory_reserve(&mut self, bytes: usize) -> RtResult<()> {
        let _ = bytes;
        Ok(())
    }

    // ---- I/O -----------------------------------------------------------------

    /// Reads a WSDL/XSD document through the stream layer (`url` may be a
    /// path). `Err` carries a libxml2-style message.
    fn load_resource(&mut self, url: &str, context: Option<&Value>) -> Result<Vec<u8>, String>;

    /// `VCWD_REALPATH`.
    fn realpath(&mut self, path: &str) -> Option<String> {
        None
    }

    /// `time(0)`.
    fn now(&mut self) -> i64 {
        0
    }

    /// `php_write`.
    fn write_output(&mut self, data: &[u8]);

    /// `sapi_add_header(line, replace = 1)`.
    fn add_header(&mut self, line: &str) {}

    /// `php_output_start_default`.
    fn ob_start(&mut self) -> bool {
        true
    }

    /// Contents of the active output buffer, when one is active and not
    /// empty.
    fn ob_contents(&mut self) -> Option<Vec<u8>> {
        None
    }

    /// `php_output_discard`: drops and closes the active buffer.
    fn ob_discard(&mut self) {}

    /// `$_SERVER[name]` as a string.
    fn server_var(&mut self, name: &str) -> Option<Vec<u8>> {
        None
    }

    /// `SG(request_info).request_method` and `query_string`.
    fn request_line(&mut self) -> (Option<String>, Option<String>) {
        (None, None)
    }

    /// The raw request body (`php://input`), when there is one.
    fn request_body(&mut self) -> Option<Vec<u8>> {
        None
    }

    /// An INI setting.
    fn ini_get(&mut self, name: &str) -> Option<Vec<u8>> {
        None
    }

    /// Creates a stream context (`php_stream_context_alloc`) as a resource
    /// value.
    fn context_create(&mut self) -> Value {
        Value::Null
    }

    /// `php_stream_context_get_option`.
    fn context_get_option(
        &mut self,
        context: &Value,
        wrapper: &str,
        option: &str,
    ) -> Option<Value> {
        None
    }

    /// `php_stream_context_set_option`.
    fn context_set_option(&mut self, context: &Value, wrapper: &str, option: &str, value: Value) {}

    /// The network layer.
    fn connector(&mut self) -> Option<&mut dyn HttpConnector> {
        None
    }

    /// `php_random_bytes_throw`.
    fn random_bytes(&mut self, n: usize) -> RtResult<Vec<u8>> {
        Ok(vec![0x5a; n])
    }

    /// The value the engine unwinds with after a fatal fault response was
    /// written (`zend_bailout()` inside `SoapServer`); typically the
    /// engine's `exit` signal.
    fn bailout(&mut self) -> php_runtime::RuntimeError {
        php_runtime::RuntimeError::Fatal(String::new())
    }

    // ---- sessions (SOAP_PERSISTENCE_SESSION) ---------------------------------

    /// Whether the session extension is available.
    fn session_available(&mut self) -> bool {
        false
    }

    /// Starts the session if needed and returns `$_SESSION['_bogus_session_name']`.
    fn session_get_object(&mut self) -> RtResult<Option<Value>> {
        Ok(None)
    }

    /// Stores `$_SESSION['_bogus_session_name']`.
    fn session_set_object(&mut self, value: Value) {}
}
