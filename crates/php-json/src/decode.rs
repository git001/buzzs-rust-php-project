// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `json_decode()` and `json_validate()` (`php_json_decode_ex`,
//! `php_json_validate_ex`).

use crate::error::{DecodeError, ErrorCode, JsonError};
use crate::flags::OBJECT_AS_ARRAY;
use crate::host::JsonHost;
use crate::parser::{ActionError, ParseError, Parser, Scalar, Sink, ValidateSink};
use php_runtime::{ArrayKey, HashTable, Value, ZArray, ZStr};

/// The decoding sink (`default_parser_methods`).
struct DecodeSink<'h> {
    host: &'h mut dyn JsonHost,
    assoc: bool,
}

/// An object under construction.
enum ObjectBuilder {
    /// `JSON_OBJECT_AS_ARRAY`: a symbol table.
    Assoc(HashTable),
    /// A `stdClass` created by the host.
    Std(Value),
}

impl Sink for DecodeSink<'_> {
    type Value = Value;
    type Array = HashTable;
    type Object = ObjectBuilder;

    fn scalar(&mut self, s: Scalar) -> Value {
        match s {
            Scalar::Null => Value::Null,
            Scalar::Bool(b) => Value::Bool(b),
            Scalar::Int(i) => Value::Int(i),
            Scalar::Double(d) => Value::Float(d),
            Scalar::Str(s) if s.is_empty() => Value::Str(ZStr::empty()),
            Scalar::Str(s) => Value::Str(ZStr::from(s)),
        }
    }

    fn empty_array(&mut self) -> Value {
        Value::empty_array()
    }

    fn array_create(&mut self) -> HashTable {
        HashTable::new()
    }

    fn array_append(&mut self, array: &mut HashTable, value: Value) {
        // A decoded array is a list, so the next index can never overflow.
        // A growth over `memory_limit` drops the value and leaves the
        // violation pending: the parser stops before the next value
        // (`aborted`).
        let _ = array.try_push(value);
    }

    fn array_finish(&mut self, array: HashTable) -> Value {
        Value::Array(ZArray::from_table(array))
    }

    fn empty_object(&mut self) -> Result<Value, ActionError> {
        if self.assoc {
            Ok(Value::empty_array())
        } else {
            self.host.new_std_class().map_err(ActionError::Exception)
        }
    }

    fn object_create(&mut self) -> Result<ObjectBuilder, ActionError> {
        if self.assoc {
            Ok(ObjectBuilder::Assoc(HashTable::new()))
        } else {
            self.host
                .new_std_class()
                .map(ObjectBuilder::Std)
                .map_err(ActionError::Exception)
        }
    }

    fn object_update(
        &mut self,
        object: &mut ObjectBuilder,
        key: ZStr,
        value: Value,
    ) -> Result<(), ActionError> {
        match object {
            ObjectBuilder::Assoc(table) => {
                // zend_symtable_update: numeric keys become integers. A
                // growth over `memory_limit` drops the member, as in
                // `array_append`.
                let _ = table.try_insert(ArrayKey::from_zstr(&key), value);
                Ok(())
            }
            ObjectBuilder::Std(obj) => {
                if key.as_bytes().first() == Some(&0) {
                    return Err(ActionError::Json(ErrorCode::InvalidPropertyName));
                }
                self.host
                    .std_class_write(obj, key, value)
                    .map_err(ActionError::Exception)
            }
        }
    }

    fn object_finish(&mut self, object: ObjectBuilder) -> Value {
        match object {
            ObjectBuilder::Assoc(table) => Value::Array(ZArray::from_table(table)),
            ObjectBuilder::Std(obj) => obj,
        }
    }

    fn aborted(&mut self) -> bool {
        php_memlimit::is_pending()
    }
}

/// Clamps a `zend_long` depth to the C `int` the parser stores.
fn int_depth(depth: i64) -> i64 {
    depth.clamp(i64::from(i32::MIN), i64::from(i32::MAX))
}

/// Decodes a JSON document (`php_json_decode_ex` behind `json_decode()`).
///
/// * `assoc`: the `$associative` argument; `Some(b)` overrides
///   `JSON_OBJECT_AS_ARRAY` in `flags`, `None` keeps it.
/// * `depth`: the nesting limit: a document whose arrays/objects nest `n`
///   levels deep needs `depth > n` (`"1"` decodes with depth 1, `"[1]"` needs
///   2), otherwise it fails with `JSON_ERROR_DEPTH`. The caller must have rejected `depth <= 0` and
///   `depth > i32::MAX` with the `ValueError`s of `json_decode()` (see
///   [`crate::functions::json_decode`]).
/// * `flags`: `JSON_OBJECT_AS_ARRAY`, `JSON_BIGINT_AS_STRING`,
///   `JSON_INVALID_UTF8_IGNORE`, `JSON_INVALID_UTF8_SUBSTITUTE`
///   (`JSON_THROW_ON_ERROR` is handled by the caller).
///
/// An empty input is a syntax error without location, like `json_decode()`.
pub fn decode(
    input: &[u8],
    assoc: Option<bool>,
    depth: i64,
    flags: i64,
    host: &mut dyn JsonHost,
) -> Result<Value, DecodeError> {
    if input.is_empty() {
        return Err(DecodeError::Json(JsonError::new(ErrorCode::Syntax)));
    }
    let mut options = flags;
    match assoc {
        Some(true) => options |= OBJECT_AS_ARRAY,
        Some(false) => options &= !OBJECT_AS_ARRAY,
        None => {}
    }
    let sink = DecodeSink {
        host,
        assoc: options & OBJECT_AS_ARRAY != 0,
    };
    Parser::new(input, options, int_depth(depth), sink, false)
        .parse()
        .map_err(|e| match e {
            ParseError::Json(j) => DecodeError::Json(j),
            ParseError::Exception(ex) => DecodeError::Exception(ex),
        })
}

/// Validates a JSON document (`php_json_validate_ex` behind
/// `json_validate()`). Returns the error that makes it invalid.
///
/// The only meaningful flag is `JSON_INVALID_UTF8_IGNORE` (`json_validate()`
/// rejects others with a `ValueError`, see
/// [`crate::functions::json_validate`]). An empty input is a syntax error
/// without location.
pub fn validate(input: &[u8], depth: i64, flags: i64) -> Result<(), JsonError> {
    if input.is_empty() {
        return Err(JsonError::new(ErrorCode::Syntax));
    }
    Parser::new(input, flags, int_depth(depth), ValidateSink, true)
        .parse()
        .map_err(|e| match e {
            ParseError::Json(j) => j,
            ParseError::Exception(_) => JsonError::new(ErrorCode::Syntax),
        })
}
