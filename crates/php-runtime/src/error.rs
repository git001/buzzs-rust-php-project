// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Error levels, thrown runtime errors and the diagnostic/object-hook context.
//!
//! The runtime never prints. Non-fatal diagnostics (warnings, notices,
//! deprecations) are reported through [`RuntimeContext::diagnostic`]; thrown
//! errors (`TypeError`, `DivisionByZeroError`, ...) are returned as
//! `Err(RuntimeError)`. Message texts match PHP byte for byte.

use crate::array::ZArray;
use crate::plain::PlainObject;
use crate::value::{Obj, Value};
use crate::ZStr;
use std::fmt;

/// PHP error levels (`E_*` constants).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorLevel {
    /// `E_ERROR` (fatal).
    Error,
    /// `E_WARNING`.
    Warning,
    /// `E_PARSE`.
    Parse,
    /// `E_NOTICE`.
    Notice,
    /// `E_CORE_ERROR`.
    CoreError,
    /// `E_CORE_WARNING`.
    CoreWarning,
    /// `E_COMPILE_ERROR`.
    CompileError,
    /// `E_COMPILE_WARNING`.
    CompileWarning,
    /// `E_USER_ERROR`.
    UserError,
    /// `E_USER_WARNING`.
    UserWarning,
    /// `E_USER_NOTICE`.
    UserNotice,
    /// `E_STRICT` (no longer emitted by PHP 8 itself).
    Strict,
    /// `E_RECOVERABLE_ERROR`.
    RecoverableError,
    /// `E_DEPRECATED`.
    Deprecated,
    /// `E_USER_DEPRECATED`.
    UserDeprecated,
}

impl ErrorLevel {
    /// The numeric value of the `E_*` constant.
    #[must_use]
    pub fn bits(self) -> i64 {
        match self {
            ErrorLevel::Error => 1,
            ErrorLevel::Warning => 2,
            ErrorLevel::Parse => 4,
            ErrorLevel::Notice => 8,
            ErrorLevel::CoreError => 16,
            ErrorLevel::CoreWarning => 32,
            ErrorLevel::CompileError => 64,
            ErrorLevel::CompileWarning => 128,
            ErrorLevel::UserError => 256,
            ErrorLevel::UserWarning => 512,
            ErrorLevel::UserNotice => 1024,
            ErrorLevel::Strict => 2048,
            ErrorLevel::RecoverableError => 4096,
            ErrorLevel::Deprecated => 8192,
            ErrorLevel::UserDeprecated => 16384,
        }
    }

    /// The label PHP prints for this level (`"Warning"`, `"Fatal error"`, ...).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ErrorLevel::Error
            | ErrorLevel::CoreError
            | ErrorLevel::CompileError
            | ErrorLevel::UserError => "Fatal error",
            ErrorLevel::RecoverableError => "Recoverable fatal error",
            ErrorLevel::Warning
            | ErrorLevel::CoreWarning
            | ErrorLevel::CompileWarning
            | ErrorLevel::UserWarning => "Warning",
            ErrorLevel::Parse => "Parse error",
            ErrorLevel::Notice | ErrorLevel::UserNotice => "Notice",
            ErrorLevel::Strict => "Strict Standards",
            ErrorLevel::Deprecated | ErrorLevel::UserDeprecated => "Deprecated",
        }
    }
}

/// A non-fatal diagnostic emitted by a runtime operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The error level.
    pub level: ErrorLevel,
    /// The message text (exactly as PHP prints it, without location).
    pub message: String,
}

/// An error thrown by a runtime operation (a PHP exception).
///
/// The built-in variants name the internal exception class to instantiate;
/// [`RuntimeError::Thrown`] carries an exception object that user code (for
/// example `__toString` or an error handler) already threw.
#[derive(Debug, Clone)]
pub enum RuntimeError {
    /// `\Error`.
    Error(String),
    /// `\TypeError`.
    TypeError(String),
    /// `\ValueError`.
    ValueError(String),
    /// `\ArithmeticError`.
    ArithmeticError(String),
    /// `\DivisionByZeroError`.
    DivisionByZeroError(String),
    /// `\ArgumentCountError`.
    ArgumentCountError(String),
    /// Any other exception class, named by its fully qualified name (e.g.
    /// `Random\BrokenRandomEngineError`, `JsonException`).
    Custom {
        /// Fully qualified class name (without leading backslash).
        class: ZStr,
        /// Exception message.
        message: String,
    },
    /// An uncatchable fatal error (`E_ERROR`), e.g. during compile-time evaluation.
    Fatal(String),
    /// An exception object already thrown by user code.
    Thrown(Value),
    /// A growth checked against `memory_limit` failed
    /// ([`php_memlimit::try_reserve`], [`php_memlimit::TryGrow`],
    /// [`HashTable::try_push`](crate::HashTable::try_push) …): the
    /// violation is recorded and pending, and the VM reports the fatal
    /// ("Allowed memory size of N bytes exhausted") where the error leaves
    /// the builtin. `?` converts the [`php_memlimit::OverLimit`].
    MemoryLimit(php_memlimit::OverLimit),
}

impl From<php_memlimit::OverLimit> for RuntimeError {
    fn from(over: php_memlimit::OverLimit) -> Self {
        RuntimeError::MemoryLimit(over)
    }
}

impl RuntimeError {
    /// The PHP class name of the exception, if built-in.
    #[must_use]
    pub fn class_name(&self) -> Option<&'static str> {
        match self {
            RuntimeError::Error(_) => Some("Error"),
            RuntimeError::TypeError(_) => Some("TypeError"),
            RuntimeError::ValueError(_) => Some("ValueError"),
            RuntimeError::ArithmeticError(_) => Some("ArithmeticError"),
            RuntimeError::DivisionByZeroError(_) => Some("DivisionByZeroError"),
            RuntimeError::ArgumentCountError(_) => Some("ArgumentCountError"),
            RuntimeError::Fatal(_)
            | RuntimeError::Thrown(_)
            | RuntimeError::Custom { .. }
            | RuntimeError::MemoryLimit(_) => None,
        }
    }

    /// The exception class name for every variant that names one, including
    /// [`RuntimeError::Custom`] (`None` for `Fatal`, `Thrown` and `MemoryLimit`).
    #[must_use]
    pub fn exception_class(&self) -> Option<String> {
        match self {
            RuntimeError::Custom { class, .. } => Some(class.to_string_lossy()),
            other => other.class_name().map(str::to_string),
        }
    }

    /// The message of a built-in error (`None` for [`RuntimeError::Thrown`] and
    /// [`RuntimeError::MemoryLimit`]).
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            RuntimeError::Error(m)
            | RuntimeError::TypeError(m)
            | RuntimeError::ValueError(m)
            | RuntimeError::ArithmeticError(m)
            | RuntimeError::DivisionByZeroError(m)
            | RuntimeError::ArgumentCountError(m)
            | RuntimeError::Custom { message: m, .. }
            | RuntimeError::Fatal(m) => Some(m),
            RuntimeError::Thrown(_) | RuntimeError::MemoryLimit(_) => None,
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::Thrown(v) => write!(f, "thrown {}", v.type_name()),
            RuntimeError::Fatal(m) => write!(f, "Fatal error: {m}"),
            RuntimeError::MemoryLimit(over) => write!(f, "Fatal error: {over}"),
            other => write!(
                f,
                "{}: {}",
                other
                    .exception_class()
                    .unwrap_or_else(|| "Error".to_string()),
                other.message().unwrap_or("")
            ),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// Shorthand result type for runtime operations.
pub type RtResult<T> = Result<T, RuntimeError>;

/// Target types for object casts (`cast_object` handler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastTarget {
    /// `IS_NULL`.
    Null,
    /// `_IS_BOOL`.
    Bool,
    /// `IS_LONG`.
    Int,
    /// `IS_DOUBLE`.
    Float,
    /// `IS_STRING`.
    String,
    /// `IS_ARRAY`.
    Array,
    /// `_IS_NUMBER` (int or float).
    Number,
}

impl CastTarget {
    /// The type name used in messages (`zend_get_type_by_const`).
    #[must_use]
    pub fn type_name(self) -> &'static str {
        match self {
            CastTarget::Null => "null",
            CastTarget::Bool => "bool",
            CastTarget::Int => "int",
            CastTarget::Float => "float",
            CastTarget::String => "string",
            CastTarget::Array => "array",
            CastTarget::Number => "int|float",
        }
    }
}

/// Binary operators, as passed to the `do_operation` object handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `**`
    Pow,
    /// `<<`
    ShiftLeft,
    /// `>>`
    ShiftRight,
    /// `|`
    BitOr,
    /// `&`
    BitAnd,
    /// `^`
    BitXor,
    /// `.`
    Concat,
    /// `xor`
    BoolXor,
    /// `~` (unary; second operand unused)
    BitNot,
    /// `!` (unary; second operand unused)
    BoolNot,
}

impl BinaryOp {
    /// The operator sigil used in "Unsupported operand types" messages.
    #[must_use]
    pub fn sigil(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
            BinaryOp::ShiftLeft => "<<",
            BinaryOp::ShiftRight => ">>",
            BinaryOp::BitOr => "|",
            BinaryOp::BitAnd => "&",
            BinaryOp::BitXor => "^",
            BinaryOp::Concat => ".",
            BinaryOp::BoolXor => "xor",
            BinaryOp::BitNot => "~",
            BinaryOp::BoolNot => "!",
        }
    }
}

/// Why an object's property table is requested (`zend_prop_purpose`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyPurpose {
    /// `var_dump`, `print_r`, `debug_zval_dump` (honours `__debugInfo`).
    Debug,
    /// `(array)` cast.
    ArrayCast,
    /// `serialize`.
    Serialize,
    /// `var_export`.
    VarExport,
    /// `json_encode`.
    Json,
    /// `get_object_vars` (filtered by the caller's property visibility).
    GetObjectVars,
}

/// What kind of object is being dumped, which changes the output format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectKind {
    /// A regular object; `lazy_prefix` is `""`, `"lazy ghost "` or `"lazy proxy "`.
    Regular {
        /// Prefix printed by `var_dump` before `object(`.
        lazy_prefix: &'static str,
    },
    /// An instance of `stdClass` (exported as `(object) array(...)`).
    StdClass,
    /// An enum case.
    Enum {
        /// The case name.
        case_name: ZStr,
        /// Backing type (`"int"`/`"string"`) for backed enums.
        backing_type: Option<&'static str>,
        /// Whether the enum class defines `__debugInfo` (then it is dumped like
        /// an object with an `enum(...)` header).
        has_debug_info: bool,
    },
}

/// The value of one property in a dump.
#[derive(Debug, Clone)]
pub enum PropertySlot {
    /// An initialized property.
    Value(Value),
    /// An uninitialized typed property; carries the declared type as printed
    /// (e.g. `"int"`, `"?Foo"`).
    Uninitialized(ZStr),
}

/// The description of an object needed by the dumpers.
#[derive(Debug, Clone)]
pub struct ObjectDump {
    /// The object kind.
    pub kind: ObjectKind,
    /// Properties in table order, with mangled keys (`"\0Class\0name"` for
    /// private, `"\0*\0name"` for protected). `None` when the object has no
    /// property table at all.
    pub properties: Option<Vec<(crate::array::ArrayKey, PropertySlot)>>,
}

/// The services the runtime needs from its embedder (the VM).
///
/// Every method has a default suitable for tests and for a VM without
/// objects; the VM overrides the object hooks to implement magic methods,
/// `__toString`, custom handlers (`compare`, `cast_object`, `do_operation`)
/// and property tables.
pub trait RuntimeContext {
    /// Reports a non-fatal diagnostic. Returning `Err` aborts the current
    /// operation (models a user error handler throwing an exception).
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()>;

    /// The `precision` ini setting (default 14).
    fn precision(&self) -> i64 {
        14
    }

    /// The `serialize_precision` ini setting (default -1).
    fn serialize_precision(&self) -> i64 {
        -1
    }

    /// The object `cast_object` handler. Returns `Ok(None)` when the cast is
    /// not supported (`FAILURE` without exception). The default behaves like
    /// `zend_std_cast_object_tostring` for a class without `__toString`.
    fn cast_object(&mut self, obj: &Obj, target: CastTarget) -> RtResult<Option<Value>> {
        let _ = obj;
        Ok(match target {
            CastTarget::Bool => Some(Value::Bool(true)),
            _ => None,
        })
    }

    /// The object `do_operation` handler (operator overloading, e.g. GMP).
    /// `Ok(None)` means "not handled".
    fn do_operation(&mut self, op: BinaryOp, op1: &Value, op2: &Value) -> RtResult<Option<Value>> {
        let _ = (op, op1, op2);
        Ok(None)
    }

    /// The `compare` handler, called by [`crate::ops::compare`] when at least
    /// one operand is an object (after the object-vs-bool and object-vs-null
    /// special cases). The default implements `zend_std_compare_objects`
    /// without access to property tables: identical objects compare equal,
    /// distinct objects are uncomparable (`1`).
    fn compare_objects(&mut self, op1: &Value, op2: &Value) -> RtResult<i32> {
        crate::ops::std_compare_objects(self, op1, op2, |_, _, _| Ok(1))
    }

    /// Returns the property table of an object for the given purpose
    /// (`zend_get_properties_for`). `None` means no table.
    fn object_properties(
        &mut self,
        obj: &Obj,
        purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        let _ = (obj, purpose);
        Ok(None)
    }

    /// Describes an object for `var_dump`/`print_r`/`var_export`.
    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        let props = self.object_properties(obj, purpose)?;
        Ok(ObjectDump {
            kind: ObjectKind::Regular { lazy_prefix: "" },
            properties: props.map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), PropertySlot::Value(v.clone())))
                    .collect()
            }),
        })
    }

    /// Decides how an object is serialized (`__serialize`, `__sleep`,
    /// `Serializable`, enums). The default writes the `Serialize`-purpose
    /// property table as `O:`.
    fn serialize_object(&mut self, obj: &Obj) -> RtResult<crate::serialize::SerializedObject> {
        let props = self
            .object_properties(obj, PropertyPurpose::Serialize)?
            .unwrap_or_default();
        Ok(crate::serialize::SerializedObject::Object {
            class: obj.class_name(),
            props,
        })
    }

    /// `unserialize()`: instantiates an object of `class` (without calling
    /// the constructor). Properties are supplied afterwards through
    /// [`RuntimeContext::unserialize_object_finish`], so that back-references
    /// (`r:`) to the object inside its own properties work.
    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        Err(RuntimeError::Error(format!(
            "Cannot unserialize object of class {class} without a VM"
        )))
    }

    /// `unserialize()`: fills the properties of an object created by
    /// [`RuntimeContext::unserialize_object_create`] (calling `__unserialize`
    /// or `__wakeup` as appropriate). `props` has symbol-table keys exactly as
    /// `__unserialize()` receives them (numeric keys are integers); for a plain
    /// property fill convert them with [`crate::ArrayKey::to_zstr`] (string keys are
    /// mangled property names).
    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        let _ = (obj, props);
        Ok(())
    }

    /// `unserialize()` of `E:` (enum case).
    fn unserialize_enum(&mut self, class: &ZStr, case: &ZStr) -> RtResult<Value> {
        Err(RuntimeError::Error(format!(
            "Cannot unserialize enum {class}::{case} without a VM"
        )))
    }

    /// `unserialize()` of `E:` while parsing: resolves `class::case`,
    /// distinguishing the failures `var_unserializer.re` reports (each with
    /// its own warning and error offset). The default delegates to
    /// [`RuntimeContext::unserialize_enum`] and reports a found case.
    fn unserialize_enum_case(
        &mut self,
        class: &ZStr,
        case: &ZStr,
    ) -> RtResult<crate::serialize::EnumCaseLookup> {
        self.unserialize_enum(class, case)
            .map(crate::serialize::EnumCaseLookup::Found)
    }

    /// `unserialize()` of `C:` (`Serializable::unserialize`).
    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        let _ = data;
        Err(RuntimeError::Error(format!(
            "Cannot unserialize object of class {class} without a VM"
        )))
    }

    /// Whether the object is an enum case (`ZEND_ACC_ENUM`). The default
    /// returns `false`.
    fn is_enum(&mut self, obj: &Obj) -> bool {
        let _ = obj;
        false
    }

    /// Whether the object is a `Closure`. `convert_to_array()` wraps a
    /// closure in a one-element array instead of exporting its properties.
    /// The default returns `false`.
    fn is_closure(&mut self, obj: &Obj) -> bool {
        let _ = obj;
        false
    }

    /// `count()` on an object: `Ok(Some(n))` for `Countable` objects (calling
    /// `count()`), `Ok(None)` when the object is not countable.
    fn count_object(&mut self, obj: &Obj) -> RtResult<Option<i64>> {
        let _ = obj;
        Ok(None)
    }

    /// Reads a property (`read_property`). `Ok(None)` means undefined. The
    /// default looks the (possibly mangled) name up in the property table.
    fn property_read(&mut self, obj: &Obj, name: &ZStr) -> RtResult<Option<Value>> {
        Ok(self
            .object_properties(obj, PropertyPurpose::Debug)?
            .and_then(|t| t.get_str(name.as_bytes()).cloned()))
    }

    /// Writes a property on an object (`zend_update_property` /
    /// `write_property`), in the order PHP would, e.g. while `json_decode` or
    /// `unserialize` fill an object. `name` is the (possibly mangled)
    /// property name. The default fails.
    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        let _ = value;
        Err(RuntimeError::Error(format!(
            "Cannot write property {}::${} without a VM",
            obj.class_name(),
            name
        )))
    }

    /// Creates a `stdClass` object whose properties are `props` (the
    /// `(object)` cast). The table already uses property-table (string) keys.
    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        let _ = props;
        Err(RuntimeError::Error(
            "Cannot create objects without a VM".to_string(),
        ))
    }
}

/// A [`RuntimeContext`] that records diagnostics in a vector. Useful for tests
/// and for constant evaluation.
///
/// Objects are supported through [`crate::plain::PlainObject`]: `(object)`
/// casts, `unserialize()` (`O:` and `C:` records), property writes,
/// property tables and dumps all work on plain objects; enums do not.
#[derive(Debug, Default)]
pub struct CollectingContext {
    /// All diagnostics emitted so far.
    pub diagnostics: Vec<Diagnostic>,
    /// The `precision` setting.
    pub precision: Option<i64>,
    /// The `serialize_precision` setting.
    pub serialize_precision: Option<i64>,
}

impl CollectingContext {
    /// Creates an empty context with default ini settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes the collected diagnostics, leaving the list empty.
    pub fn take(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    /// The collected messages formatted as `"Level: message"`.
    #[must_use]
    pub fn messages(&self) -> Vec<String> {
        self.diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.level.label(), d.message))
            .collect()
    }
}

impl RuntimeContext for CollectingContext {
    fn diagnostic(&mut self, level: ErrorLevel, message: &str) -> RtResult<()> {
        self.diagnostics.push(Diagnostic {
            level,
            message: message.to_string(),
        });
        Ok(())
    }

    fn precision(&self) -> i64 {
        self.precision.unwrap_or(14)
    }

    fn serialize_precision(&self) -> i64 {
        self.serialize_precision.unwrap_or(-1)
    }

    fn object_properties(
        &mut self,
        obj: &Obj,
        _purpose: PropertyPurpose,
    ) -> RtResult<Option<ZArray>> {
        Ok(obj.downcast_ref::<PlainObject>().map(PlainObject::props))
    }

    fn object_dump(&mut self, obj: &Obj, purpose: PropertyPurpose) -> RtResult<ObjectDump> {
        let props = self.object_properties(obj, purpose)?;
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

    fn serialize_object(&mut self, obj: &Obj) -> RtResult<crate::serialize::SerializedObject> {
        if let Some(data) = obj
            .downcast_ref::<PlainObject>()
            .and_then(PlainObject::custom_data)
        {
            return Ok(crate::serialize::SerializedObject::Custom {
                class: obj.class_name(),
                data,
            });
        }
        let props = self
            .object_properties(obj, PropertyPurpose::Serialize)?
            .unwrap_or_default();
        Ok(crate::serialize::SerializedObject::Object {
            class: obj.class_name(),
            props,
        })
    }

    fn unserialize_object_create(&mut self, class: &ZStr) -> RtResult<Value> {
        Ok(Value::Object(PlainObject::create(
            class.clone(),
            ZArray::new(),
        )))
    }

    fn unserialize_object_finish(&mut self, obj: &Value, props: ZArray) -> RtResult<()> {
        if let Value::Object(o) = obj {
            for (k, v) in props.iter() {
                self.property_write(o, &k.to_zstr(), v.clone())?;
            }
        }
        Ok(())
    }

    fn unserialize_custom(&mut self, class: &ZStr, data: &[u8]) -> RtResult<Value> {
        let o = PlainObject::create(class.clone(), ZArray::new());
        if let Some(p) = o.downcast_ref::<PlainObject>() {
            p.set_custom_data(Some(data.to_vec()));
        }
        Ok(Value::Object(o))
    }

    fn property_write(&mut self, obj: &Obj, name: &ZStr, value: Value) -> RtResult<()> {
        match obj.downcast_ref::<PlainObject>() {
            Some(p) => {
                p.write(name, value);
                Ok(())
            }
            None => Err(RuntimeError::Error(format!(
                "Cannot write property {}::${}",
                obj.class_name(),
                name
            ))),
        }
    }

    fn create_std_object(&mut self, props: ZArray) -> RtResult<Value> {
        Ok(Value::Object(PlainObject::create("stdClass", props)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exception_classes() {
        let e = RuntimeError::ArgumentCountError("x() expects exactly 1 argument, 0 given".into());
        assert_eq!(e.class_name(), Some("ArgumentCountError"));
        let c = RuntimeError::Custom {
            class: ZStr::from("Random\\BrokenRandomEngineError"),
            message: "A random engine must return a non-empty string".into(),
        };
        assert_eq!(c.class_name(), None);
        assert_eq!(
            c.exception_class().as_deref(),
            Some("Random\\BrokenRandomEngineError")
        );
        assert_eq!(
            c.message(),
            Some("A random engine must return a non-empty string")
        );
        assert_eq!(
            c.to_string(),
            "Random\\BrokenRandomEngineError: A random engine must return a non-empty string"
        );
    }
}
