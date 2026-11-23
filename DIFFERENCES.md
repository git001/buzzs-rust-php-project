<!-- buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
     Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE. -->

# DIFFERENCES

Where Buzz behaves differently from php-src on purpose. The goal is still
PHP's observable behaviour byte for byte (README.md); every entry here is a
decision, not a bug and not missing work. Missing work belongs in
[FEATURE_GAP.md](FEATURE_GAP.md).

- **Reference:** php-src master (Buzz reports `PHP_VERSION` 8.7.0-dev,
  `ZEND_VERSION` 4.7.0-dev, `crates/php-vm/src/lib.rs`). Comparisons run
  against PHP 8.6 as the oracle; where 8.6 and master differ, master wins
  and that is not listed here.
- **An entry** says what Buzz does, what php-src does, why, where it lives
  and which test holds it. A new deviation is agreed first, then added
  here in the same commit as the code.
- **Observable** means visible to a script, an INI setting, the command
  line, output, exit status or an operator. Internal design (Rust types,
  data layout) is not listed.

## Memory

Details: [crates/php-memlimit/INTEGRATION.md](crates/php-memlimit/INTEGRATION.md).

- **No Zend allocator.** Memory comes from mimalloc with a counter per
  thread; `zend_mm` is not ported. php-src with `USE_ZEND_ALLOC=0` behaves
  alike: `gc_mem_caches()` returns 0. Where: `crates/php-memlimit`.
- **Different byte counts.** `memory_get_usage()`/`memory_get_peak_usage()`
  count Rust allocation sizes, so absolute values differ from PHP;
  `memory_get_usage(true)` is the usage rounded up to 2 MiB chunks, not
  mapped memory; "tried to allocate N" names Buzz's allocation (or the
  growth step that was refused). Tests compare deltas or use `%d`.
  Where: `crates/php-memlimit/src/counting.rs`.
- **`memory_limit` above `max_memory_limit`.** Buzz applies the maximum as
  the limit; php-src updates the INI value and leaves the heap limit
  unchanged. Where: `crates/php-memlimit/src/counting.rs`.
- **Growth loops stop at the soft limit (stricter than php-src).** php-src
  delays the soft-limit error (`EG(delayed_oom)`), so a builtin that grows
  in its own loop runs on until it returns or the hard limit kills the
  process (signal 9, status 137, no shutdown functions;
  `Zend/tests/oom_hard_limit_exceeded.phpt`). Buzz checks once per
  iteration and before each growth step: the same "Allowed memory size"
  fatal with a lower peak, exit status 255, shutdown functions run, the
  worker serves the next request. Why: many requests share one process in
  the threaded servers. Paths: string and array builtins (also `sprintf`
  field widths, `base64_encode`, `bin2hex`, `mb_detect_encoding` with a
  long list, `output_add_rewrite_var`, deeply nested `http_build_query`),
  `json_*`,
  `serialize`/`unserialize`, `var_dump`/`print_r`, `http_build_query`,
  `PDOStatement::fetchAll`, `SQLite3Result::fetchAll`, `mysqli_fetch_all`,
  buffered mysqlnd results (the rows read so far are dropped and the rest
  of the set is read without keeping it, so the connection stays in step),
  and single mysqlnd rows, which grow by up to 16 MiB per packet (a row
  past the limit is read and dropped; php-src reports mysqlnd's "Out of
  memory" client error there, Buzz gives the memory fatal only). With the
  compressed protocol one envelope (at most 16 MiB) is still buffered
  unchecked.
  Test: `crates/php-extensions/tests/memlimit_p0.rs` (at most 2 MiB over
  the limit; the MySQL cases need `MYSQL_TEST_HOST`).
- **Room for shutdown after the memory fatal.** Once the "Allowed memory
  size" fatal is delivered, the request may use 2 MiB (one php-src chunk)
  past the limit until it ends, so shutdown functions and the teardown can
  run; the limit and its messages stay as they were. php-src gets this
  room from what is left free in its last chunk, which varies with
  fragmentation (often more than 1 MiB); Buzz counts the bytes in use and
  would otherwise stand exactly at the limit. Test:
  `crates/php-memlimit/tests/shutdown_grace.rs`, memlimit_p0's
  `fgets_loop_into_array`. Where: `crates/php-memlimit/src/counting.rs`
  (`SHUTDOWN_GRACE`).
- **Hard ceiling without a handler.** It defaults to twice the limit;
  where no SAPI handler is installed, Rust's "memory allocation of N bytes
  failed" ends the process (SIGABRT). Where: `crates/php-memlimit/src/lib.rs`.
- **Uncounted library memory is bounded.** Memory a library takes from its
  own `malloc` (libgmp) does not count, as in php-src, but only 64 such
  huge blocks can be uncounted at once; beyond that they count.
  Where: `crates/php-memlimit/src/counting.rs`.
- **libcurl counts (stricter than php-src).** libcurl runs on
  `php_memlimit::c_heap` (`curl_global_init_mem`); php-src leaves it on
  malloc. Its memory shows in `memory_get_usage()` and counts towards
  `memory_limit`; past the limit libcurl gets NULL (`CURLE_OUT_OF_MEMORY`)
  and the request ends with the "Allowed memory size" fatal. Why: many
  requests share one process in the threaded servers. Visible in
  `ext/curl/tests/bug45161.phpt`: a reused handle keeps about 8.7 KB of
  connection and DNS cache after its first transfer (constant over 1024
  transfers, not a leak), which the test's 1024-byte margin does not
  allow. Libraries below libcurl (OpenSSL, nghttp2, libssh) stay on
  malloc. Where: `crates/php-curl/src/easy.rs` (`global_init`). Test:
  `crates/php-extensions/tests/curl_alloc_faults.rs` (every allocation of
  a curl script fails in turn, each run ends cleanly).
- **lexbor (DOM) counts but does not fail.** Its allocations go through
  the Rust allocator and count towards `memory_limit` (php-src:
  `femalloc`); a soft-limit overrun is raised after the method returns,
  only the hard ceiling fails an allocation. Test: gh25118.
  Where: `crates/php-dom/src/lexbor.rs`.
- **exif keeps no JPEG sections.** `exif_read_data()` hands each JPEG
  section to its handler and frees it; php-src keeps every section of the
  file until the call ends. A file of many small sections
  (oom_exif_scan_jpeg_header: 2 MiB of empty markers, `memory_limit=8M`)
  ends with the "Allowed memory size" fatal in php-src and is read in Buzz.
  Where: `crates/php-exif/src/exif.rs`. Test: `oom_phpt_replays` in
  `crates/php-exif/tests/phpt_replay.rs`.
- **Address-space cap.** `php` and `php-cgi` set `RLIMIT_AS` to the larger
  of 4 GiB and 75 % of RAM plus swap (`PHPRUST_MAX_AS_MB` overrides), so
  runaway native code fails instead of exhausting the machine; C PHP has
  no such cap. Where: `crates/php-cli/src/memcap.rs`.
- **gmp past its limits.** Where libgmp cannot represent a result or the
  address space cannot hold it, php-src aborts the process ("overflow in
  mpz type", "Cannot allocate memory"); Buzz ends the request with the
  "Allowed memory size" fatal (`gmp_pow(2, 2**40)`).
  Where: `crates/php-ext-gmp/src/lib.rs`.
- **bcmath result size.** A result over 256 Mi digits is reported as the
  out-of-memory fatal whatever `memory_limit` is.
  Where: `crates/php-bcmath/src/functions.rs`.

## Stack and nesting limits

php-src checks the real native stack (`zend.max_allowed_stack_size`); the
Rust interpreter uses more native stack per PHP level, so Buzz adds fixed
backstops. The errors are php-src's; only the depth at which they appear
differs.

- **Calls:** more than 50,000 PHP frames give "Maximum call stack size …
  reached". Where: `crates/php-vm/src/call.rs`.
- **Nested executions** (callbacks, destructors, magic methods): at most
  20,000. Where: `crates/php-vm/src/exec.rs`.
- **Fibers:** `fiber.stack_size` is scaled by 4; nesting inside a fiber is
  budgeted at 64 KiB per level. Where: `crates/php-vm/src/fiber.rs`.
- **Array comparison and the dump functions** turn the stack into a depth
  from assumed C frame sizes (256 B and 128 B per level, 8 MiB stack).
  Where: `crates/php-vm/src/include.rs`, `crates/php-vm/src/builtins/dump.rs`.
- **Parser:** bison's `YYMAXDEPTH` becomes a recursion limit of 9,990;
  deeply nested code is parsed again on a helper thread with a 512 MiB
  stack, and with the `CurrentThread` policy it can fail with "memory
  exhausted" earlier than PHP. Where: `crates/php-parser/src/parser/mod.rs`,
  `crates/php-parser/src/stack.rs`.
- **Fixed depths in extensions** standing in for stack checks: JSON
  encoder 3,000 (`JSON_ERROR_DEPTH`), mbstring arrays 4,096, query/URL
  encoding 4,096, session serializer 4,096; the recursive array functions
  measure depth at 64 B per level and enforce it only when
  `zend.max_allowed_stack_size` is set. Where: `crates/php-json/src/host.rs`,
  `crates/php-ext-mbstring/src/arrays.rs`, `crates/php-stdmisc/src/query.rs`,
  `crates/php-session/src/serializer.rs`, `crates/php-ext-stdarr/src/stack.rs`.
- **CLI stack:** the CLI raises `RLIMIT_STACK` to 256 MiB and may re-exec
  itself once; otherwise the VM runs on a thread with a large stack.
  Where: `crates/php-cli/src/main.rs`.

## Engine

- **Hash flooding.** A hash table whose collision chain grows past 16
  switches that table to SipHash-1-3 with a per-process key; php-src keeps
  the unseeded DJB hash (DJBX33A) and relies on `max_input_vars`. Order
  and results are unchanged. Where: `crates/php-runtime/src/array.rs`
  (`MAX_CHAIN`, `flood`).
- **Cycle collector.** `gc_collect_cycles()` also scans every live object,
  not only the root buffer, so a missed release site cannot leak a
  cycle; trial deletion counts references instead of php-src's grey
  marking. Collected counts can differ. Where: `crates/php-vm/src/gc.rs`.
- **Sorting a shared array.** In the sort swap path a copy with refcount
  above 1 is separated where php-src shares the live table; the copy keeps
  the reset position. Where: `crates/php-runtime/src/array.rs`.
- **Legacy fallback seed.** Without a CSPRNG the fallback seed mixes time,
  pid and a counter with splitmix64 instead of PHP's SHA-1 construction.
  Where: `crates/php-random/src/legacy.rs`.

## Extensions and loading

- **Everything is built in.** `extension=` accepts the name of a built-in
  module, an existing `.so` is skipped, and `dl()` fails like a missing
  shared object after php-src's checks. Binary Zend extensions (PECL, PIE)
  cannot be loaded; libraries get their own Rust extension instead.
  Where: `crates/php-cli/src/startup.rs`, `crates/php-ext-stdmisc/src/misc.rs`.
- **zend_test is always present.** It is in the default extension set of
  every binary (`zend_test_*` functions exist); php-src builds it only on
  request. Where: `crates/php-extensions/src/lib.rs`.
- **Startup diagnostics once.** The VM and its extensions are built per
  request or per thread, but MINIT INI diagnostics are reported for the
  first VM only. Where: `crates/php-cli/src/startup.rs`.
- **System libraries instead of bundled ones:**
  - fileinfo uses the system libmagic (5.45), not php-src's patched
    bundled 5.48; some descriptions differ. Test:
    `crates/php-fileinfo/tests/oracle.rs` (`known_divergences_are_stable`,
    `data/divergent.tsv`).
  - gd links the system libgd (2.3.3; Ubuntu builds it without GD2 and
    AVIF); what php-src's bundled libgd does differently is ported in Rust
    (`crates/php-gd`).
  - zip binds the system libzip (1.7.3); features that need a newer libzip
    (seekable streams, torrentzip, zstd) are absent.
  - pcre links the system PCRE2 when found, else the bundled one; the JIT
    stack is 192 KiB per thread.

## opcache

- **One cache per process, no shared memory.** Threads share the script
  cache in process memory; processes do not share anything. The cache
  holds a bytecode image that each thread decodes once.
  Where: `crates/php-ext-opcache/src/shared.rs`.
- **`opcache_reset()` takes effect at once;** php-src restarts the cache
  at the next request. Where: `crates/php-ext-opcache/src/lib.rs`.
- **No machine-code JIT.** The `opcache.jit*` settings are accepted and
  validated, `opcache_get_status()["jit"]` reports modelled buffer figures.
- **Own file-cache format.** `opcache.file_cache` stores Buzz's bytecode
  encoding (not interchangeable with php-src's); the checksum is XXH3, not
  Adler-32; entries are written to a temporary file and renamed into
  place, and an existing entry is replaced (php-src never replaces one).
  Where: `crates/php-opcache/src/file_cache.rs`.

## SAPIs and command line

- **No mod_php.** apache2handler was dropped (php.net advises PHP-FPM
  behind `mod_proxy_fcgi`); FastCGI/FPM and an HTTP server are planned.
- **Panics end only the request.** A bug in Buzz (a Rust panic) is logged
  as "Internal error (panic) at …", answered with a 500 page if nothing
  was sent (otherwise the connection closes), no shutdown functions,
  destructors, output handlers or session save handlers run, and the
  thread is replaced. A crash in C PHP takes the process down.
  Where: `crates/php-sapi/src/panic.rs`, `crates/php-cli-server/src/sapi.rs`.
- **Built-in server threads.** `php -S` runs each request on its own thread
  with a fresh VM; the event loop stays single-threaded.
  Where: `crates/php-cli-server/src/sapi.rs`.
- **Router `return`.** The router script's top-level `return <expr>` is
  rewritten to call `\__cli_server_router_return()`, a function that
  exists only there; lines and traces are unchanged.
  Where: `crates/php-cli-server/src/router.rs`.
- **Silent fallback for `php://input`'s buffer.** When `php://input` is
  what first moves a streamed request body to a temporary file and
  `upload_tmp_dir` is unusable, the file goes to the system's temporary
  directory without php-src's notice ("file created in the system's
  temporary directory"); when the POST handlers buffer the body, the
  notice is given as in php-src. Where: `crates/php-sapi-input/src/body.rs`.
- **Orphaned uploads are deleted.** Temporary files of uploads that a later
  `request_parse_body()` drops are deleted at request end; php-src leaks
  them. Where: `crates/php-sapi-input/src/body.rs`.
- **Process title.** The environment after `argv` is not moved, so the
  title in `ps` is limited to the original command line length;
  `cli_get_process_title()` still returns the full title.
  Where: `crates/php-cli/src/proctitle.rs`.

## Logging

- **One format language for all logs.** Error log, access log and slow log
  use FPM's `access.format` language (php-src uses it for FPM's access log
  only); the default formats reproduce php-src's lines byte for byte.
  Additional placeholders: `%{minutes|hours|days}d` and the us/ms/s/m/h/d
  units, `%{Name}i` (request header), `%{name}L` (message, level, type,
  file, line, kind, tz, request_id, trace_id, span_id); `%{NAME}e` falls
  back to the process environment where FPM prints `-`; times are always
  in the C locale. Where: `crates/php-observability`. Test:
  `crates/php-observability/src/format/tests.rs`.

## Test runner (phpt-runner, not the engine)

- A timed-out test is not retried as flaky; output is capped at 16 MiB per
  process; JUnit output lists every counted test, sorted by path, with
  millisecond times and without out-of-scope tests; `GZIP_POST`/
  `DEFLATE_POST` bodies use stored deflate blocks (different
  `CONTENT_LENGTH`); `--PHPDBG--` tests are skipped; no XLEAK, file-cache
  or preload modes. Where: `crates/phpt-runner/README.md`.
