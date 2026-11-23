// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/hash` for the phprust VM: binds the `php-hash` library through the
//! `php_vm::ext` extension API.
//!
//! * `hash()`, `hash_file()`, `hash_hmac()`, `hash_hmac_file()`,
//!   `hash_init()`, `hash_update()`, `hash_update_stream()`,
//!   `hash_update_file()`, `hash_final()`, `hash_copy()`, `hash_algos()`,
//!   `hash_hmac_algos()`, `hash_pbkdf2()`, `hash_equals()`, `hash_hkdf()`;
//! * the `HashContext` class (private constructor, cloning,
//!   `__serialize()`/`__unserialize()`, `__debugInfo()`);
//! * the `mhash*()` compatibility functions and the `HASH_HMAC` /
//!   `MHASH_*` constants;
//! * `md5()`, `md5_file()`, `sha1()`, `sha1_file()` and `crc32()` of
//!   `ext/standard`, which share the algorithms.
//!
//! Files and stream resources go through `php-ext-streams` (see
//! [`files`]).
//!
//! The `mhash*()` functions and `MHASH_*` constants carry their
//! `#[\Deprecated]` notices.

// Native handlers and object hooks must match the VM's fn-pointer
// signatures (`NativeFn`, `ClassHooks`), even when they cannot fail.
#![allow(clippy::unnecessary_wraps)]

mod context;
mod files;

use php_hash::{
    Diagnostic, DiagnosticLevel, ErrorClass, HashError, HashOptions, SecretOption, SeedOption,
};
use php_runtime::{Value, ZArray, ZStr};
use php_vm::ext::{Args, Extension, ModuleBuilder, Throw, VmResult};
use php_vm::Vm;

/// The `hash` extension.
#[derive(Debug, Clone, Copy, Default)]
pub struct Hash;

/// ext/standard's digests (`md5.c`, `sha1.c`, `crc32.c`): `md5()`,
/// `sha1()`, `crc32()` and the `_file` variants, over the same library as
/// [`Hash`] but part of the `standard` module, as in PHP.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandardDigests;

/// Builds the throwable PHP raises for a library error inside `func`.
pub(crate) fn throw_error(vm: &mut Vm, func: &str, e: &HashError) -> Throw {
    let class = match e.class {
        ErrorClass::ValueError => "ValueError",
        ErrorClass::TypeError => "TypeError",
        ErrorClass::Error => "Error",
        ErrorClass::Exception => "Exception",
    };
    vm.throw(class, &e.message_for(func))
}

fn fail(vm: &mut Vm, a: &Args, e: &HashError) -> Throw {
    // Diagnostics PHP emits before throwing (none for most errors).
    if let Err(t) = emit(vm, e.diagnostics.clone()) {
        return t;
    }
    if let Some(bytes) = e.out_of_memory {
        return vm.out_of_memory(bytes);
    }
    throw_error(vm, &a.func_name(), e)
}

fn emit(vm: &mut Vm, diags: Vec<Diagnostic>) -> VmResult<()> {
    for d in diags {
        match d.level {
            DiagnosticLevel::Deprecated => vm.php_deprecated(&d.message)?,
            DiagnosticLevel::Warning => vm.php_warning(&d.message)?,
        }
    }
    Ok(())
}

fn bytes(b: impl AsRef<[u8]>) -> Value {
    Value::Str(ZStr::from(b.as_ref()))
}

/// Raw digest or its lowercase hex form.
fn digest_value(raw: &[u8], binary: bool) -> Value {
    if binary {
        bytes(raw)
    } else {
        bytes(php_hash::bin2hex(raw))
    }
}

/// Translates the `$options` array (`seed`, `secret`).
fn options(vm: &mut Vm, algo: &[u8], opts: Option<&ZArray>) -> VmResult<HashOptions> {
    let mut out = HashOptions::default();
    let Some(opts) = opts else {
        return Ok(out);
    };
    if let Some(seed) = opts.get_str(b"seed") {
        out.seed = Some(match seed.deref() {
            Value::Int(i) => SeedOption::Int(i),
            _ => SeedOption::NonInt,
        });
    }
    // Only the XXH3 family reads (and therefore converts) the secret.
    let lower = algo.to_ascii_lowercase();
    if matches!(lower.as_slice(), b"xxh3" | b"xxh128") {
        if let Some(secret) = opts.get_str(b"secret") {
            let secret = secret.deref();
            if out.seed.is_some() {
                // PHP rejects seed + secret before looking at the secret
                // (no deprecation, no string conversion); the library
                // raises that Error from the presence of both.
                out.secret = Some(SecretOption {
                    bytes: Vec::new(),
                    was_string: true,
                });
                return Ok(out);
            }
            // PHP emits this deprecation *before* converting the value (whose
            // `__toString()` may throw), so it is emitted here, exactly once,
            // and the library is told the value was a string.
            if !matches!(secret, Value::Str(_)) {
                vm.php_deprecated(
                    "Passing a secret of a type other than string is deprecated because it implicitly converts to a string, potentially hiding bugs",
                )?;
            }
            let s = vm.value_to_string(&secret)?;
            out.secret = Some(SecretOption {
                bytes: s.as_bytes().to_vec(),
                was_string: true,
            });
        }
    }
    Ok(out)
}

fn opt_array(a: &Args, n: usize) -> Option<ZArray> {
    match a.opt(n) {
        Some(Value::Array(arr)) => Some(arr),
        _ => None,
    }
}

// ------------------------------------------------------------ one-shot

fn hash(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let algo = a.str(0);
    let opts = options(vm, algo.as_bytes(), opt_array(a, 3).as_ref())?;
    match php_hash::hash(algo.as_bytes(), a.str(1).as_bytes(), &opts) {
        Ok(d) => {
            emit(vm, d.diagnostics.clone())?;
            Ok(digest_value(&d.bytes, a.bool(2)))
        }
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn hash_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let algo = a.str(0);
    if php_hash::Algorithm::find(algo.as_bytes()).is_none() {
        return Err(a.value_error(vm, 1, "must be a valid hashing algorithm"));
    }
    let path = a.str(1);
    files::check_path(vm, a, 2, path.as_bytes())?;
    let opts = options(vm, algo.as_bytes(), opt_array(a, 3).as_ref())?;
    let mut ctx = match php_hash::HashContext::new(algo.as_bytes(), 0, b"", &opts) {
        Ok(c) => c,
        Err(e) => return Err(fail(vm, a, &e)),
    };
    let read = files::read_path(
        vm,
        path.as_bytes(),
        files::default_context().as_ref(),
        |chunk| {
            let _ = ctx.update(chunk);
        },
    )?;
    emit(vm, ctx.take_diagnostics())?;
    if read.is_none_or(|r| r.error) {
        return Ok(Value::Bool(false));
    }
    match ctx.finalize() {
        Ok(d) => Ok(digest_value(&d, a.bool(2))),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn hash_hmac(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match php_hash::hash_hmac(
        a.str(0).as_bytes(),
        a.str(1).as_bytes(),
        a.str(2).as_bytes(),
    ) {
        Ok(d) => Ok(digest_value(&d, a.bool(3))),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn hash_hmac_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let mut ctx = match php_hash::hmac_context(a.str(0).as_bytes(), a.str(2).as_bytes()) {
        Ok(c) => c,
        Err(e) => return Err(fail(vm, a, &e)),
    };
    let path = a.str(1);
    files::check_path(vm, a, 2, path.as_bytes())?;
    let read = files::read_path(
        vm,
        path.as_bytes(),
        files::default_context().as_ref(),
        |chunk| {
            let _ = ctx.update(chunk);
        },
    )?;
    if read.is_none_or(|r| r.error) {
        return Ok(Value::Bool(false));
    }
    match ctx.finalize() {
        Ok(d) => Ok(digest_value(&d, a.bool(3))),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

// --------------------------------------------------------- incremental

fn hash_init(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let algo = a.str(0);
    let opts = options(vm, algo.as_bytes(), opt_array(a, 3).as_ref())?;
    let mut ctx =
        match php_hash::HashContext::new(algo.as_bytes(), a.int(1), a.str(2).as_bytes(), &opts) {
            Ok(c) => c,
            Err(e) => return Err(fail(vm, a, &e)),
        };
    emit(vm, ctx.take_diagnostics())?;
    context::new_object(vm, ctx)
}

fn hash_update(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let data = a.str(1);
    context::with_live(vm, a, 0, |c| c.update(data.as_bytes()))?.map_err(|e| fail(vm, a, &e))?;
    Ok(Value::Bool(true))
}

fn hash_update_stream(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // ZPP validates the stream resource before the context is verified.
    php_ext_streams::res::stream_arg(vm, a, 1)?;
    context::with_live(vm, a, 0, |_| ())?;
    let length = if a.has(2) { a.int(2) } else { -1 };
    let data = files::read_stream(vm, a, 1, length)?;
    context::with_live(vm, a, 0, |c| c.update(&data))?.map_err(|e| fail(vm, a, &e))?;
    Ok(Value::Int(i64::try_from(data.len()).unwrap_or(i64::MAX)))
}

fn hash_update_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let path = a.str(1);
    files::check_path(vm, a, 2, path.as_bytes())?;
    if let Some(ctx) = a.get(2) {
        if !matches!(ctx, Value::Resource(_) | Value::Null) {
            let msg = format!(
                "must be of type resource or null, {} given",
                ctx.value_name()
            );
            return Err(a.type_error(vm, 3, &msg));
        }
    }
    context::with_live(vm, a, 0, |_| ())?;
    // php_stream_context_from_zval(zcontext, 0): null means the default.
    let stream_context = match files::context_arg(vm, a, 2)? {
        Some(c) => Some(c),
        None => files::default_context(),
    };
    let mut data = Vec::new();
    let read = files::read_path(vm, path.as_bytes(), stream_context.as_ref(), |chunk| {
        data.extend_from_slice(chunk);
    })?;
    let Some(end) = read else {
        return Ok(Value::Bool(false));
    };
    context::with_live(vm, a, 0, |c| c.update(&data))?.map_err(|e| fail(vm, a, &e))?;
    Ok(Value::Bool(!end.error))
}

fn hash_final(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let d = context::with_live(vm, a, 0, php_hash::HashContext::finalize)?
        .map_err(|e| fail(vm, a, &e))?;
    Ok(digest_value(&d, a.bool(1)))
}

fn hash_copy(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let copy = context::with_live(vm, a, 0, |c| c.try_clone())?;
    match copy {
        Ok(c) => context::new_object(vm, c),
        Err(_) => Err(vm.throw("Error", "Cannot copy hash")),
    }
}

fn hash_algos(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Array(ZArray::from_values(
        php_hash::hash_algos().into_iter().map(Value::string),
    )))
}

fn hash_hmac_algos(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Array(ZArray::from_values(
        php_hash::hash_hmac_algos().into_iter().map(Value::string),
    )))
}

// ------------------------------------------------------- derivation

fn hash_pbkdf2(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    // PHP's first large allocation is `safe_emalloc(loops, digest_size, 0)`
    // (after argument validation, which the library repeats below).
    if a.int(3) > 0 {
        if let Some(size) =
            php_hash::hash_pbkdf2_alloc_size(a.str(0).as_bytes(), a.int(4), a.bool(5))
        {
            php_memlimit::emalloc_check(size)?;
        }
    }
    match php_hash::hash_pbkdf2(
        a.str(0).as_bytes(),
        a.str(1).as_bytes(),
        a.str(2).as_bytes(),
        a.int(3),
        a.int(4),
        a.bool(5),
    ) {
        Ok(out) => Ok(bytes(out)),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn hash_hkdf(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match php_hash::hash_hkdf(
        a.str(0).as_bytes(),
        a.str(1).as_bytes(),
        a.int(2),
        a.str(3).as_bytes(),
        a.str(4).as_bytes(),
    ) {
        Ok(out) => Ok(bytes(out)),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn hash_equals(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (known, user) = (a.at(0), a.at(1));
    for (n, v) in [(1, &known), (2, &user)] {
        if !matches!(v, Value::Str(_)) {
            let msg = format!("must be of type string, {} given", v.value_name());
            return Err(a.type_error(vm, n, &msg));
        }
    }
    let (Value::Str(k), Value::Str(u)) = (known, user) else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(php_hash::hash_equals(
        k.as_bytes(),
        u.as_bytes(),
    )))
}

// ------------------------------------------------------------- mhash

fn mhash(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let key = a.opt(2).map(|_| a.str(2));
    match php_hash::mhash(
        a.int(0),
        a.str(1).as_bytes(),
        key.as_ref().map(ZStr::as_bytes),
    ) {
        Ok(Some(d)) => Ok(bytes(d)),
        Ok(None) => Ok(Value::Bool(false)),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn mhash_get_hash_name(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(php_hash::mhash_get_hash_name(a.int(0)).map_or(Value::Bool(false), Value::string))
}

fn mhash_get_block_size(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(
        php_hash::mhash_get_block_size(a.int(0)).map_or(Value::Bool(false), |n| {
            Value::Int(i64::try_from(n).unwrap_or(i64::MAX))
        }),
    )
}

fn mhash_count(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(php_hash::mhash_count()))
}

fn mhash_keygen_s2k(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    match php_hash::mhash_keygen_s2k(a.int(0), a.str(1).as_bytes(), a.str(2).as_bytes(), a.int(3)) {
        Ok(Some(d)) => Ok(bytes(d)),
        Ok(None) => Ok(Value::Bool(false)),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

// ------------------------------------------------------ ext/standard

fn md5(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(digest_value(&php_hash::md5(a.str(0).as_bytes()), a.bool(1)))
}

fn sha1(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(digest_value(
        &php_hash::sha1(a.str(0).as_bytes()),
        a.bool(1),
    ))
}

fn crc32(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::from(php_hash::crc32(a.str(0).as_bytes()))))
}

/// `md5_file()` / `sha1_file()`: the digest of a file's contents.
fn file_digest(vm: &mut Vm, a: &Args, algo: &str) -> VmResult<Value> {
    let path = a.str(0);
    files::check_path(vm, a, 1, path.as_bytes())?;
    let mut ctx = match php_hash::HashContext::new(algo.as_bytes(), 0, b"", &HashOptions::default())
    {
        Ok(c) => c,
        Err(e) => return Err(fail(vm, a, &e)),
    };
    let read = files::read_path(
        vm,
        path.as_bytes(),
        files::default_context().as_ref(),
        |chunk| {
            let _ = ctx.update(chunk);
        },
    )?;
    // md5_file()/sha1_file() (unlike hash_file()) also fail when the
    // stream stopped before EOF.
    if read.is_none_or(|r| r.error || !r.eof) {
        return Ok(Value::Bool(false));
    }
    match ctx.finalize() {
        Ok(d) => Ok(digest_value(&d, a.bool(1))),
        Err(e) => Err(fail(vm, a, &e)),
    }
}

fn md5_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    file_digest(vm, a, "md5")
}

fn sha1_file(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    file_digest(vm, a, "sha1")
}

impl Extension for StandardDigests {
    fn name(&self) -> &'static str {
        "standard"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        m.function_static("md5(string $string, bool $binary = false): string", md5)
            .function_static(
                "md5_file(string $filename, bool $binary = false): string|false",
                md5_file,
            )
            .function_static("sha1(string $string, bool $binary = false): string", sha1)
            .function_static(
                "sha1_file(string $filename, bool $binary = false): string|false",
                sha1_file,
            )
            .function_static("crc32(string $string): int", crc32);
    }
}

impl Extension for Hash {
    fn name(&self) -> &'static str {
        "hash"
    }

    fn register(&self, m: &mut ModuleBuilder<'_>) {
        context::register(m);
        m.constant("HASH_HMAC", Value::Int(php_hash::HASH_HMAC));
        for (name, id) in php_hash::mhash_constants() {
            m.constant_deprecated(
                &format!("MHASH_{name}"),
                Value::Int(id),
                "8.5",
                "as the mhash*() functions were deprecated",
            );
        }
        m.function_static("hash(string $algo, string $data, bool $binary = false, array $options = []): string", hash)
            .function_static("hash_file(string $algo, string $filename, bool $binary = false, array $options = []): string|false", hash_file)
            .function_static("hash_hmac(string $algo, string $data, #[\\SensitiveParameter] string $key, bool $binary = false): string", hash_hmac)
            .function_static("hash_hmac_file(string $algo, string $filename, #[\\SensitiveParameter] string $key, bool $binary = false): string|false", hash_hmac_file)
            .function_static("hash_init(string $algo, int $flags = 0, #[\\SensitiveParameter] string $key = \"\", array $options = []): HashContext", hash_init)
            .function_static("hash_update(HashContext $context, string $data): true", hash_update)
            .function_static("hash_update_stream(HashContext $context, $stream, int $length = -1): int", hash_update_stream)
            .function_static("hash_update_file(HashContext $context, string $filename, $stream_context = null): bool", hash_update_file)
            .function_static("hash_final(HashContext $context, bool $binary = false): string", hash_final)
            .function_static("hash_copy(HashContext $context): HashContext", hash_copy)
            .function_static("hash_algos(): array", hash_algos)
            .function_static("hash_hmac_algos(): array", hash_hmac_algos)
            .function_static("hash_pbkdf2(string $algo, #[\\SensitiveParameter] string $password, string $salt, int $iterations, int $length = 0, bool $binary = false, array $options = []): string", hash_pbkdf2)
            .function_static("hash_equals(#[\\SensitiveParameter] $known_string, #[\\SensitiveParameter] $user_string): bool", hash_equals)
            .function_static("hash_hkdf(string $algo, #[\\SensitiveParameter] string $key, int $length = 0, string $info = \"\", string $salt = \"\"): string", hash_hkdf)
            .function_static("mhash_get_block_size(int $algo): int|false", mhash_get_block_size)
            .deprecated("8.1", "")
            .function_static("mhash_get_hash_name(int $algo): string|false", mhash_get_hash_name)
            .deprecated("8.1", "")
            .function_static("mhash_keygen_s2k(int $algo, #[\\SensitiveParameter] string $password, string $salt, int $length): string|false", mhash_keygen_s2k)
            .deprecated("8.1", "")
            .function_static("mhash_count(): int", mhash_count)
            .deprecated("8.1", "")
            .function_static("mhash(int $algo, string $data, #[\\SensitiveParameter] ?string $key = null): string|false", mhash)
            .deprecated("8.1", "");
    }
}
