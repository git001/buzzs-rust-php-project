// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `zend_test_handle_post_data()`: runs the POST handler registered for a
//! content type over a stream, as if that stream were the request body
//! (`sapi_read_post_data()` + `sapi_handle_post()`).
//!
//! Ref: `ext/zend_test/test.c`, `main/SAPI.c` (`sapi_read_standard_form_data`),
//! `main/php_content_types.c` (the post entries and the default reader),
//! `main/php_variables.c` (`php_std_post_handler`) and `main/rfc1867.c`
//! (whose port lives in [`php_sapi_input::rfc1867`]).

use crate::util::{E_NOTICE, E_WARNING};
use php_ext_streams::res;
use php_runtime::{Value, ZArray, ZStr};
use php_sapi_input::body::{self, RequestBody, SapiInput};
use php_sapi_input::rfc1867::{self, RegValue, Target, UploadSettings};
use php_sapi_input::vars::{content_type_key, input_settings, upload_tmp_dir};
use php_stdmisc::query::{self, ParseKind, RegisterOptions};
use php_streams::stream::Stream;
use php_vm::ext::{Args, VmResult};
use php_vm::Vm;
use std::cell::RefCell;

/// The function name `php_error_docref()` prefixes warnings with.
const FUNC: &str = "zend_test_handle_post_data";

/// `DEFAULT_POST_CONTENT_TYPE`.
const FORM_URLENCODED: &[u8] = b"application/x-www-form-urlencoded";
/// `MULTIPART_CONTENT_TYPE`.
const MULTIPART: &[u8] = b"multipart/form-data";

/// The stream as the request body: `zend_test_read_post` as
/// `sapi_module.read_post`.
struct StreamBody<'a> {
    body: RequestBody,
    stream: RefCell<&'a mut Stream>,
}

impl SapiInput for StreamBody<'_> {
    fn body(&self) -> &RequestBody {
        &self.body
    }

    fn post(&self) -> &dyn php_sapi_input::SapiPost {
        self
    }
}

impl php_sapi_input::SapiPost for StreamBody<'_> {
    fn read_post(&self, _offset: u64, buf: &mut [u8]) -> usize {
        read_full(&mut self.stream.borrow_mut(), buf)
    }
}

/// `php_error_docref()` (prefixed with the function) or `sapi_error()`.
fn warn(vm: &mut Vm, message: &str, docref: bool) -> VmResult<()> {
    if docref {
        vm.raise(E_WARNING, &format!("{FUNC}(): {message}"))
    } else {
        vm.raise(E_WARNING, message)
    }
}

fn upload_settings(vm: &mut Vm) -> UploadSettings {
    UploadSettings {
        max_file_uploads: vm.ini_long("max_file_uploads"),
        max_multipart_body_parts: vm.ini_long("max_multipart_body_parts"),
        post_max_size: vm.ini_long("post_max_size"),
        max_input_vars: vm.ini_long("max_input_vars"),
        upload_max_filesize: vm.ini_long("upload_max_filesize"),
        file_uploads: vm.ini_bool("file_uploads"),
        upload_tmp_dir: upload_tmp_dir(vm),
        read_post_bytes: 0,
    }
}

/// `zend_test_publish_track_vars()`: points the superglobal at `arr`.
fn publish(vm: &mut Vm, name: &[u8], arr: ZArray) {
    let old = {
        let mut g = vm.globals.borrow_mut();
        let i = g.find_or_add(name) as usize;
        g.slots[i].replace(Value::Array(arr))
    };
    // Release the previous value outside the borrow (destructors).
    drop(old);
}

/// `sapi_read_post_data()` + `sapi_handle_post()` over `read`, filling
/// `post` and `files`.
fn handle(
    vm: &mut Vm,
    cell: &res::StreamCell,
    content_type: &[u8],
    content_length: i64,
    post: &mut ZArray,
    files: &mut ZArray,
) -> VmResult<()> {
    let key = content_type_key(content_type);
    if key == MULTIPART {
        // rfc1867_post_handler: no post reader, the handler reads the body.
        let upload = upload_settings(vm);
        let outcome = res::with_stream(vm, cell, |s| {
            let mut read = |buf: &mut [u8]| read_full(s, buf);
            rfc1867::parse(content_type, content_length, &upload, &mut read)
        })?;
        let Some(outcome) = outcome else {
            return Ok(());
        };
        vm.rfc1867_uploaded_files = outcome.uploaded;
        for w in &outcome.warnings {
            if w.notice {
                vm.raise(E_NOTICE, &format!("{FUNC}(): {}", w.message))?;
            } else {
                warn(vm, &w.message, w.docref)?;
            }
        }
        let settings = input_settings(vm);
        let prefix = format!("{FUNC}(): ");
        let opts = RegisterOptions {
            max_nesting_level: settings.max_input_nesting_level,
            display_errors: settings.display_errors,
            cookie: false,
            warning_prefix: &prefix,
        };
        for r in outcome.registrations {
            let v = match r.value {
                RegValue::Str(s) => Value::Str(ZStr::from(s)),
                RegValue::Long(l) => Value::Int(l),
            };
            let track = match r.target {
                Target::Post => &mut *post,
                Target::Files => &mut *files,
            };
            query::register_variable_ex(&r.name, v, track, &opts, vm)
                .map_err(|e| vm.throw_runtime_error(e))?;
        }
        return Ok(());
    }
    // The urlencoded entry's reader, or `php_default_post_reader()`
    // swallowing a body no handler is registered for.
    let post_max_size = vm.ini_long("post_max_size");
    if post_max_size > 0 && content_length > post_max_size {
        return warn(
            vm,
            &format!(
                "POST Content-Length of {content_length} bytes exceeds the limit of {post_max_size} bytes"
            ),
            true,
        );
    }
    let data = res::with_stream(vm, cell, |s| {
        let input = StreamBody {
            body: RequestBody::new(),
            stream: RefCell::new(s),
        };
        let exceeded = body::buffer_rest(&input, post_max_size);
        let buffered = body::request_body(&input).map(|b| b.to_vec());
        (buffered.unwrap_or_default(), exceeded)
    })?;
    let Some((data, exceeded)) = data else {
        return Ok(());
    };
    if exceeded {
        warn(
            vm,
            &format!(
                "Actual POST length does not match Content-Length, and exceeds {post_max_size} bytes"
            ),
            true,
        )?;
    }
    if key == FORM_URLENCODED {
        // php_std_post_handler over the buffered body.
        let settings = input_settings(vm);
        query::treat_data(ParseKind::Post, &data, post, &settings, FUNC, None, vm)
            .map_err(|e| vm.throw_runtime_error(e))?;
    }
    Ok(())
}

/// `sapi_module.read_post` over a stream (`zend_test_read_post`): fills
/// `buf` unless the stream ends first.
fn read_full(s: &mut Stream, buf: &mut [u8]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        match s.read(&mut buf[n..]) {
            Ok(got) if got > 0 => n += got,
            _ => break,
        }
    }
    n
}

/// `zend_test_handle_post_data(resource $stream, string $content_type,
/// int $content_length): void`.
pub(crate) fn handle_post_data(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let r = res::resource_arg(vm, a, 0)?;
    let content_type = a.str(1).as_bytes().to_vec();
    let content_length = a.int(2);
    let cell = res::stream_of_resource(vm, a, &r)?;
    // Tear down what a previous call set up, as the request shutdown would.
    rfc1867::cleanup(&std::mem::take(&mut vm.rfc1867_uploaded_files));
    let mut post = ZArray::new();
    let mut files = ZArray::new();
    let result = handle(
        vm,
        &cell,
        &content_type,
        content_length,
        &mut post,
        &mut files,
    );
    publish(vm, b"_POST", post);
    publish(vm, b"_FILES", files);
    result.map(|()| Value::Null)
}
