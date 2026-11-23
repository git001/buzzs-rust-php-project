# php-memlimit integration guide

For the php-vm / php-cli owner and for extension-binding authors.
API docs: `cargo doc -p php-memlimit`. Reference: `Zend/zend_alloc.c`
(`zend_mm_check_limit`, `zend_mm_delayed_error`, `zend_handle_delayed_oom`,
`zend_set_memory_limit`), `main/main.c` (`OnChangeMemoryLimit`,
`php_request_shutdown`).

## Model in one paragraph

`php_memlimit::GLOBAL` is a `CountingAlloc<MiMalloc>` that counts every live
byte. Crossing `memory_limit` never fails an ordinary allocation (Rust would
abort); it **records** an `OverLimit { limit, tried }` (first one wins) that
the VM turns into the PHP fatal at its next safe point — exactly PHP's
"delayed OOM". Builtins that are about to allocate a size the script controls
call `try_reserve(bytes)` first (PHP's fallible `zend_*_falloc` +
`RETURN_OOM()`), so the fatal is raised before anything is allocated. Only an
allocation crossing the **hard ceiling** (2x the limit, or limit + 1 GiB above
1 GiB; `memory_hard_limit_percent` tunes it) fails: it calls the hard-limit
handler if installed, otherwise Rust prints
`memory allocation of N bytes failed` and aborts (SIGABRT).

## Where the memory comes from

Every block comes from mimalloc (a heap per thread, built for many small
blocks; frees from any thread). `zend_mm` is not ported: php-src built
without the Zend allocator behaves the same way (`gc_mem_caches()`
returns 0, the cycle GC is unaffected).

Each thread counts on one `CountingAlloc`: by default the process-wide
`GLOBAL`; a worker thread of a threaded server takes its own with
`php_memlimit::attach_thread()` before its first request (php-src ZTS: a
heap per thread), so its usage, peak and `memory_limit` are its own. The
installed `ThreadAlloc` forwards each call to the calling thread's
instance (`php_memlimit::current()`); the instance goes back to a pool
when the thread ends. Process-wide settings (hard-limit handler, pending
hook, heap observer, tracked mode) are set on `GLOBAL` before the threads
attach. A block freed through another thread's instance than the one that
counted it lowers that instance's count, clamped at 0.

Within an instance the request thread is the *owner*: it counts with plain
loads and stores, every other thread with locked read-modify-writes.
`request_start` binds the calling thread and `request_end` unbinds a
thread that `request_start` bound. Ownership is exclusive and never taken
from a live owner: a thread that calls `request_start` while another
thread still owns the instance counts on the shared path, correctly but
more slowly.

A SAPI that runs its requests on one thread binds that thread itself,
before module startup, so that startup allocations count on the owner path
too: `php_memlimit::bind_to_current_thread()` (php-cli: in `do_php_cli`,
after the built-in server check). A thread bound this way keeps the
ownership across `request_end`, so the teardown that follows (the `Vm`'s
drop or discard, TLS destructors at exit) stays on the owner path; it
gives it up with `php_memlimit::unbind_current_thread()` if another thread
is to take it. Multi-threaded SAPIs (`php -S` request threads, later the
FastCGI and HTTP workers) leave the binding to `request_start`/`request_end`.

## 1. php-cli: install the allocator (once)

`crates/php-cli/Cargo.toml`: `php-memlimit = { path = "../php-memlimit" }`
(also add it to php-vm and to every binding that calls `try_reserve`).

`crates/php-cli/src/main.rs`, at item level:

```rust
php_memlimit::install_global_allocator!();
```

Keep `limit_memory()` (`RLIMIT_AS`) as the outermost backstop. Install a
hard-limit handler early in `main()` to get PHP's output instead of Rust's
abort:

```rust
fn hard_limit(e: php_memlimit::HardLimitExceeded) -> ! {
    // PHP: "Hard memory limit of %zu bytes exhausted (tried to allocate %zu bytes) in %s on line %d\n"
    // on stderr, then _exit(137). Runs inside the allocator with the ceiling disabled:
    // allocation is allowed, but the Vm is not reachable here, so read the
    // location from a thread-local the executor keeps (or print "Unknown" / 0).
    let (file, line) = php_vm::current_location_snapshot(); // to be provided by the VM
    let _ = std::io::Write::write_all(&mut std::io::stderr(),
        format!("{e} in {file} on line {line}\n").as_bytes());
    unsafe { libc::_exit(137) } // SAFETY: terminates without running atexit/destructors, like PHP
}
php_memlimit::set_hard_limit_handler(hard_limit);
```

Only the binary may install the allocator. Library test binaries do not see
it: there `usage()` stays at `PHP_BASELINE` and only `try_reserve` enforces
the limit, which is what binding tests want.

## 2. Request lifecycle (php-vm, `Vm::run_script` / `Vm::shutdown`)

| When | Call |
|---|---|
| after INI parsing and extension startup, **before compiling the main script** (top of `run_script`) | `php_memlimit::request_start(php_memlimit::limit_from_ini(ini memory_limit as u64))`; on `Err`, `force_limit(Some(DEFAULT_LIMIT))` (the registry has already warned) |
| after `run_main` returns (also on `Ok`) | poll once more: `oom_after_main_script.phpt` |
| start of `Vm::shutdown`, **before shutdown functions** | `php_memlimit::clear()` (PHP resets `EG(delayed_oom)` here, so a shutdown function can raise its own OOM: `oom_pending_at_shutdown.phpt`) |
| after each shutdown function / destructor batch | poll |
| end of `Vm::shutdown` (after output flush) | `php_memlimit::request_end()` |

`request_start` binds the accounting fast path to the calling thread (no
locked instructions for its allocations), resets the baseline so
`memory_get_usage()` starts at `PHP_BASELINE` (401 408; PHP 8.3 CLI reports
402–404 KB), resets the peak and clears stale violations.

## 3. Safe points: raising the fatal

The poll is one relaxed atomic load. Put it in `Vm::run` (crates/php-vm/src/exec.rs)
next to the existing per-op `object::has_pending_destructors()` check, i.e.
after every opcode — this covers loop back-edges, returns from builtins (the
`DoFcall`-style op that invoked `call_builtin`) and user calls:

```rust
if php_memlimit::is_pending() {
    if let Some(over) = php_memlimit::take() {
        // E_ERROR, attributed to the op just executed (ip - 1):
        // "PHP Fatal error:  Allowed memory size of 10485760 bytes exhausted (tried to allocate 2097152 bytes) in %s on line %d"
        self.raise(E_ERROR, &over.to_string())?; // -> Throw::Fatal, unwinds
    }
}
```

`take()` moves the record to *handled*: until `clear()` no further violation
is recorded and `is_pending()` is false, so the unwinding after the fatal
cannot fire a second one (PHP's `ZEND_OOM_STATE_HANDLED`). The message has no
`" at file:line"` part (that is debug-build only; tests match it with `%S`).
`error_get_last()['message']` is `over.to_string()`.

Instead of polling on every opcode, the VM can install
`php_memlimit::set_pending_hook(hook)` once: `hook()` runs each time a
violation is recorded (an allocation over the soft limit or a failed
`try_reserve`), after `is_pending()`/`check()` can see it, on the recording
thread and possibly inside the allocator, so it should only set an atomic
flag (php-src's `EG(vm_interrupt)`); the VM then calls `take()` at its next
interrupt check.

Also poll before calling user callbacks from builtins if convenient
(`oom_soft_limit_in_callback.phpt`); not required for correctness.

## 4. Bindings: checks on the real allocations, no estimates

No binding reads the `memory_limit` INI or writes the "Allowed memory
size" message: `OverLimit`'s `Display` in this crate is the only text, and
the VM reports it with the file and line of the call. The per-binding
estimates (json, bcmath, iconv, uri, xml, head, soap, zlib/bz2, exif,
mbstring) were removed in memlimit-fallible stage 5; where they protected
the process from unchecked growth, that growth is now checked itself
(zlib/bz2 output buffers, uri path segments and IDNA buffers, exif stream
reads). gmp's address-space guard is not a limit check (DIFFERENCES.md).

Before an allocation whose size the script picks:

```rust
// str_repeat: PHP allocates zend_string header (24) + len + 1
if php_memlimit::try_reserve_n(input.len(), mult, 25).is_err() {
    return Ok(Value::Null); // RETURN_OOM(): the VM raises the fatal right after this call
}
let out = input.repeat(mult);
```

- `try_reserve(bytes)` / `try_reserve_n(count, size, extra)` compare current
  usage + bytes against the limit; on failure they **record** the violation
  (so the VM's post-call poll raises it, with the line of the call) and
  return `Err(OverLimit)`. Return any value; it is discarded. Overflow in
  `try_reserve_n` counts as `usize::MAX` bytes.
- Use PHP's own size formula (e.g. `HT_PACKED_SIZE_EX`, `zend_string` header
  24 + len + 1) where tests print exact numbers; most use `%d`.
- For Rust collections, `php_memlimit::TryGrow` (`vec.try_grow(n)`,
  `try_grow_exact(n)`) checks the growth first and returns `Err(OverLimit)`
  (recorded) instead of growing. In a builtin, `?` turns it into the fatal,
  which the VM reports as the error leaves the builtin.
- Incremental builders (json_encode, preg_replace, implode, unserialize…)
  need nothing: the allocator records the violation as soon as the buffer
  crosses the limit and the VM raises it after the builtin returns. Call
  `try_reserve` only where a single allocation could exceed the hard ceiling
  (any size the script picks).
- Keep the non-limit errors (`Possible integer overflow in memory allocation`)
  in the bindings; they are not limit checks.
- A library that allocates unconditionally (bcmath's `bc_num`, hash's
  `pbkdf2` output) checks the size first with
  `php_memlimit::emalloc_check(bytes)`: past the hard ceiling the block is
  really allocated, so the hard-limit handler reacts as php-src does;
  below it, the soft limit. A library that reports a failed block maps it
  with `Vm::out_of_memory(bytes)`: the memory fatal over the limit, else
  "Out of memory (tried to allocate N bytes)".
- A library that is not linked against the VM takes `php-memlimit` itself
  (php-iconv, php-uri, php-exif, php-zlib, php-bz2): its buffers grow with
  `TryGrow`, and past the limit it stops with what it has. The binding then
  calls `php_memlimit::poll()?` before it turns a library error into an
  exception, so the fatal wins.
- Memory that php-src obtains from a library's own `malloc` (not `emalloc`)
  is invisible to `memory_limit` and `memory_get_usage()`. Run such code in
  `php_memlimit::uncounted(|| ...)`: huge blocks (> 2 MiB − 4 KiB) allocated
  inside are registered by address and stay out of usage, peak, the soft
  limit and the hard ceiling until freed (wherever that happens); smaller
  blocks are counted as usual. php-ext-gmp wraps every handler this way
  (libgmp allocates limbs with the system `malloc`). Only `RLIMIT_AS`
  bounds uncounted blocks.

## 5. Userland functions (php-vm `builtins/core.rs`)

| PHP | Implementation |
|---|---|
| `memory_get_usage(bool $real_usage = false)` | `php_memlimit::memory_get_usage(real)` |
| `memory_get_peak_usage(bool $real_usage = false)` | `php_memlimit::memory_get_peak_usage(real)` |
| `memory_reset_peak_usage()` | `php_memlimit::reset_peak()` |
| `gc_mem_caches()` | `php_memlimit::gc_mem_caches()` (0, as without the Zend allocator; reported to the heap observer) |

`real_usage` is the usage rounded up to 2 MiB chunks (minimum one chunk), so
a fresh script reports `memory_get_usage(true) === 2097152`, like PHP.

## 6. INI (`memory_limit`, `max_memory_limit`, `memory_hard_limit_percent`)

php-support's `IniRegistry` already implements `OnChangeMemoryLimit` /
`OnChangeMaxMemoryLimit` messages, including the clamp to
`max_memory_limit` and "Failed to set memory limit to %zd bytes (Current
memory usage is %zd bytes)". Wire it to real numbers:

1. Before `vm.ini.ini_set(...)` / `ini_restore` of `memory_limit` or
   `max_memory_limit`: `vm.ini.memory_usage = php_memlimit::real_usage() as u64`.
2. In `Vm::sync_ini` for those names, after the registry accepted the value:
   `php_memlimit::GLOBAL.force_limit(php_memlimit::limit_from_ini(vm.ini.get_long("memory_limit") as u64))`
   (`-1` → `u64::MAX` → unlimited). `force_limit` because the registry has
   already done the usage check and warned.
   (Equivalently, without the registry: `apply_memory_limit_ini(value, max)`
   returns `MemoryLimitIni::{Applied, ClampedToMax { warning }}` or the
   `SetLimitError` whose `Display` is the warning.)
3. `memory_hard_limit_percent` (registry handler is `None` today): on change
   call `php_memlimit::set_hard_limit_percent(v)` (`v` parsed with
   `parse_quantity`; unset = -1); on `Err(e)` emit `E_WARNING` `e.to_string()`
   ("memory_hard_limit_percent must be >= 100, or -1" /
   "Failed to set memory_hard_limit_percent to %d") and reject the change.

Semantics (`zend_set_memory_limit`): a limit below the current *real* usage
fails (so `ini_set('memory_limit', '1M')` fails with "Current memory usage
is 2097152 bytes"); `-1` and anything at or above `LIMIT_MAX` mean unlimited;
the limit applies to reported usage (current − request baseline +
`PHP_BASELINE`), so VM structures allocated before `request_start` do not
count.

## Approximations (documented, intentional)

- Bytes are Rust `Layout` sizes; Zend rounds to bin sizes and adds chunk
  overhead, and our values are differently sized than zvals, so absolute
  numbers differ from PHP. Tests compare deltas or use `%d`.
- `memory_get_usage(true)` is chunk-rounded usage, not mapped memory
  (mimalloc keeps and returns pages on its own, see "Where the memory
  comes from").
- `tried to allocate` is the Rust allocation (or reserved) size, not Zend's
  chunk/huge-block size.
- On clamping to `max_memory_limit` we apply the max as the limit (PHP
  leaves the heap limit unchanged).
- After the memory fatal is taken, the request gets `SHUTDOWN_GRACE`
  (2 MiB) past the limit until `request_end`, standing in for the free
  part of php-src's last chunk, so shutdown functions can run; messages
  keep naming the limit.

## Stricter than php-src (intentional)

Listed with the other deliberate differences in `DIFFERENCES.md` (Memory).

php-src 8.6 delays the soft-limit error (`EG(delayed_oom)`): `emalloc` over
`memory_limit` still returns the memory and only sets `EG(vm_interrupt)`.
A builtin that grows in a loop of its own (no VM safe point inside) keeps
running past the limit, until it returns or until the hard limit
(`memory_hard_limit_percent`) ends the process. mysqlnd, for example, reads
a whole buffered result set this way.

Buzz runs many requests in one process, so a request must not grow
"until the hard ceiling". Growth loops in builtins therefore stop at the
soft limit: `php_memlimit::poll()` once per iteration, and the
collection's `try_*` growth (`HashTable::try_push`/`try_insert`,
`TryGrow::try_grow`) before it doubles. The overrun is at most one
iteration or one growth step; the contract test
`crates/php-extensions/tests/memlimit_p0.rs` holds every listed path to
2 MiB above the limit.

What a script observes:

- Where php-src returns from the call over the limit and then reports
  "Allowed memory size of … exhausted", Buzz reports the same fatal, only
  with a lower peak (`memory_get_peak_usage()` in a shutdown function).
- Where php-src reaches the hard limit and ends the process ("Hard memory
  limit … exhausted" on stderr, then signal 9, status 137 in a shell, no
  shutdown functions; `Zend/tests/oom_hard_limit_exceeded.phpt`), Buzz
  ends the request with the soft-limit fatal: exit status 255, shutdown
  functions run, the worker serves the next request.
- `tried to allocate` names the growth step that was refused, not the
  allocation php-src happened to be in.

Paths (branch `memlimit-fallible`): string and array builtins (also
`sprintf` field widths, `base64_encode`, `bin2hex`, `mb_detect_encoding`
with a long list, `output_add_rewrite_var`, deeply nested
`http_build_query`), `json_*`,
`serialize`/`unserialize`, `var_dump`/`print_r` (which also stream like
php-src), `http_build_query`, `PDOStatement::fetchAll` (all modes),
`SQLite3Result::fetchAll`, `mysqli_fetch_all`, and buffered mysqlnd
results (`mysqli_query`, `mysqli_stmt::store_result`, PDO MySQL). A
buffered mysqlnd result that hits the limit drops the rows read so far and
reads the rest of the set from the server without keeping it, so the
connection stays in step with the server (persistent connections, pools).
A single row is checked before each packet (up to 16 MiB) is appended;
past the limit the rest of the row is read and dropped
(`RowPacket::OverLimit`), with no mysqlnd client error. Reads of a chunk
or more go straight into the caller's buffer, as php-src's stream layer
grows its read buffer one chunk at a time. Test:
`crates/php-mysqlnd/tests/live_memlimit.rs` (the connection answers the
next query). Not covered: with the compressed protocol one envelope (at
most 16 MiB) is buffered and inflated unchecked.
