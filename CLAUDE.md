# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

# buzzs-rust-php-project

A Rust re-implementation of PHP whose goal is to pass the php-src `.phpt` test suite, byte for byte,
and to run as fast as the C implementation or faster (see "Performance").
`modules/php-src/` is a READ-ONLY reference clone of https://github.com/php/php-src (never edit it; it is gitignored).
`modules/php-rust/` is a READ-ONLY clone of https://github.com/francescotinti/php-rust — the competing project (phprust.com); for study/comparison only, never edit it, never copy code from it (its license must be respected; use it for measured comparisons and ideas, not code).
`/usr/bin/php` (the C implementation, PHP 8.3.6) is installed and may be used as an oracle to check expected behaviour. Where 8.3 and php-src master differ, follow master.

## Commands

```sh
cargo build -p php-cli -p phpt-runner            # target/debug/php and target/debug/phpt-runner
target/debug/php -r 'echo 1+2, "\n";'            # smoke test

# Required before declaring work done (scope with -p; combine several -p in one command):
cargo fmt && cargo clippy --all-targets -p <crate> && cargo test -p <crate>
cargo test -p <crate> <test_name_substring>      # one Rust test

# .phpt tests, the real acceptance criterion (file or directory):
target/debug/phpt-runner --php target/debug/php modules/php-src/Zend/tests/foo.phpt
env -u LANGUAGE target/debug/phpt-runner --php target/debug/php -j 3 -q --no-artifacts \
    --summary-depth 2 modules/php-src/ext/json
```

- Runner flags worth knowing: `--show-diff` (expected vs actual), `--filter <substr>` (repeatable), `--list`, `-d NAME=VALUE`, `--env KEY=VALUE`, `--resume <file.jsonl>` for long runs. The runner snapshots the binary before running, so rebuilding mid-run is safe.
- `env -u LANGUAGE` matters if the shell exports `LANGUAGE`: it overrides `LC_ALL` in gettext and fails ext/gettext tests under any PHP.
- `phpt-baseline.txt` lists known failures; `--baseline phpt-baseline.txt` fails only on regressions, `--update-baseline` refreshes it.
- `phpt-scope.txt` lists out-of-scope path prefixes (FPM, phpdbg, pgsql, ffi, readline, …; php-cgi, pdo_mysql, mysqli and sqlite are in scope). The runner reads it by default; those tests report `OUT` and are excluded from pass rates and the baseline. `--no-scope` runs everything. opcache is in scope; its optimizer/JIT-dependent failures are tracked in the baseline.
- `phpt-environment.txt` lists tests that fail only for host reasons, identically under C PHP (Ubuntu libgd without GD2, MariaDB lacking a native JSON type). The runner reports them as PASS; `--no-environment` shows raw failures. Do not add them to the baseline.
- `--json report.json` then `scripts/update_feature_gap.py report.json` rewrites FEATURE_GAP.md's `%` column from a run.
- Use dev builds for correctness and scoped PHPT runs. Reserve `--release` (thin LTO, one codegen unit, slow to rebuild) for performance measurement. Avoid `cargo clean` in edit/test loops.
- When behaviour is unclear, run a small script through the oracle (`/usr/bin/php -d memory_limit=1G x.php`) rather than guessing.

## Architecture

The workspace has ~100 crates in strict layers:

1. **Engine pipeline**: `php-lexer` → `php-parser` (AST with exact parse-error messages) → `php-vm` (`compiler/` lowers AST to op arrays; `exec.rs` is the interpreter loop `execute_ex`; classes, exceptions, generators, fibers, GC, reflection live alongside) → `php-cli` (the `php` binary). `php-cli-server` (`php -S`) is the other SAPI; FastCGI/FPM and an HTTP server follow (mod_php was dropped: php.net advises against it for new installations).
2. **`php-runtime`** is the shared vocabulary: `Value` (zval), `ZStr` (zend_string), `ZArray` (ordered hash table), operators and type juggling, var_dump/serialize. Crates coordinate only through its types.
3. **Library / binding split.** Each PHP extension is two crates:
   - `php-<name>` holds the pure logic. It is VM-independent and has its own oracle-driven test suite.
   - `php-ext-<name>` is a thin binding through `php_vm::ext` (`Extension`, `ModuleBuilder`, `ClassBuilder`, `Args`/ZPP). Native functions are registered from PHP stub signatures. The library calls back into the VM through a host trait that the ext crate implements; `crates/php-ext-json/src/host.rs` is the reference example, including how VM throws and runtime errors convert both ways.
   - Per-request extension state lives in thread-locals reset at request shutdown.
   - `crates/php-extensions/src/lib.rs` lists every extension `php`, `php-cgi`, `php -S` and the servers load; `Vm::with_extensions` registers them. A new ext crate is not live until it is added there.
4. **`phpt-runner`** ports `run-tests.php`: all `.phpt` sections, parallel scheduling, baseline and scope handling.

Every crate's doc comments cite the php-src file(s) it ports (e.g. `ref: Zend/zend_hash.c`); match that C source's observable behaviour.

### Crates and the php-src code each one ports
- `crates/php-lexer`   — tokenizer (ref: Zend/zend_language_scanner.l)
- `crates/php-ext-tokenizer` — token_get_all/PhpToken over php-lexer and php-parser (ref: ext/tokenizer)
- `crates/php-parser`  — AST + parser (ref: Zend/zend_language_parser.y, zend_ast.c)
- `crates/php-runtime` — values, strings, ordered hash table, operators/type juggling (ref: zend_types.h, zend_hash.c, zend_operators.c)
- `crates/php-vm`      — compiler + executor, classes, exceptions, output (ref: zend_compile.c, zend_execute.c)
- `crates/php-cli`     — the `php` binary (ref: sapi/cli)
- `crates/phpt-runner` — runs `.phpt` files from modules/php-src against our binary (ref: run-tests.php)
- `crates/php-support` — INI parser, output-buffering stack, printf/sprintf format engine (ref: Zend/zend_ini_*, main/output.c, main/snprintf.c, ext/standard/formatted_print.c)
- `crates/php-hash`    — hash()/md5/sha*/crc32 etc. (ref: ext/hash, ext/standard/{md5,sha1,crc32}.c)
- `crates/php-random`  — Mt19937/PCG/Xoshiro/Secure engines, rand/mt_rand/lcg exact sequences (ref: ext/random)
- `crates/php-date`    — timelib port: strtotime parser, date() formatting, tz database, DateTime arithmetic (ref: ext/date/lib, ext/date/php_date.c)
- `crates/php-pcre`    — preg_* semantics over PCRE2: pattern/modifier parsing, replace/split/quote, error constants (ref: ext/pcre/php_pcre.c)
- `crates/php-stdstr`  — pure byte-level algorithms behind ext/standard string/html/url/math functions (ref: ext/standard/{string,html,url,base64,...}.c)
- `crates/php-streams` — stream layer: wrappers (file, php://, data:, glob), memory/temp streams, filters, fopen modes, stat (ref: main/streams/, ext/standard/file.c)
- `crates/php-stdarr`  — ext/standard array functions over php-runtime Value/ZArray (ref: ext/standard/array.c)
- `crates/php-mbstring` — libmbfl port: encodings, conversion, mb_* string algorithms (ref: ext/mbstring, ext/mbstring/libmbfl)
- `crates/php-json`     — json_encode/json_decode/json_validate over php-runtime Value (ref: ext/json)
- `crates/php-filter`   — filter_var/filter_input validation+sanitize filters (ref: ext/filter)
- `crates/php-ctype`    — ctype_* (ref: ext/ctype)
- `crates/php-uri`      — RFC 3986 + WHATWG URL parsers behind Uri\\Rfc3986\\Uri / Uri\\WhatWg\\Url (ref: ext/uri)
- `crates/php-bcmath`   — arbitrary-precision decimal arithmetic for bc* functions and BcMath\\Number (ref: ext/bcmath)
- `crates/php-gmp`      — big-integer engine behind GMP objects/functions (ref: ext/gmp)
- `crates/php-calendar` — calendar conversions (ref: ext/calendar)
- `crates/php-zlib`     — zlib binding with ext/zlib semantics (gz*, deflate/inflate contexts, zlib.* filters)
- `crates/php-bz2`      — libbz2 binding with ext/bz2 semantics
- `crates/php-iconv`    — glibc iconv binding with ext/iconv semantics
- `crates/php-posix`    — ext/posix over libc
- `crates/php-openssl`  — OpenSSL binding with ext/openssl semantics
- `crates/php-sqlite`   — libsqlite3 binding behind ext/sqlite3 and ext/pdo_sqlite
- `crates/php-libxml`   — libxml2 FFI base + ext/xml (expat-style) + ext/xmlwriter; dom/simplexml/xmlreader/xsl build on it
- `crates/php-intl`     — ICU binding with ext/intl semantics (ref: ext/intl)
- `crates/php-exif`     — EXIF/JPEG/TIFF metadata parser with ext/exif semantics (ref: ext/exif)
- `crates/php-sockets`  — ext/sockets over libc
- `crates/php-sysv`     — ext/sysvmsg, sysvsem, sysvshm, shmop over libc
- `crates/php-gettext`  — ext/gettext over libc
- `crates/php-pcntl`    — ext/pcntl over libc (signal dispatch hooks for the VM)
- `crates/php-spl`      — SPL data structures, iterators, ArrayObject core over php-runtime (ref: ext/spl)
- `crates/php-session`  — session core: serializers, save handlers, ID generation, INI semantics (ref: ext/session)
- `crates/php-mysqlnd`  — pure-Rust MySQL client protocol (ref: ext/mysqlnd); `php-ext-mysqli` binds ext/mysqli on it
- `crates/php-zip`      — libzip binding with ext/zip semantics
- `crates/php-fileinfo` — libmagic binding with ext/fileinfo semantics
- `crates/php-sodium`   — libsodium binding with ext/sodium semantics
- `crates/php-xsl`      — libxslt binding over php-libxml with ext/xsl semantics
- `crates/php-memlimit` — counting global allocator + memory_limit accounting for the "Allowed memory size exhausted" fatal (ref: Zend/zend_alloc.c semantics)
- `crates/php-cli-server` — the CLI built-in web server (`php -S`) (ref: sapi/cli/php_cli_server.c)
- `crates/php-curl` + `crates/php-ext-curl` — libcurl binding with ext/curl semantics (ref: ext/curl)
- `crates/php-ftp` + `crates/php-ext-ftp` — FTP client protocol with ext/ftp semantics (ref: ext/ftp)
- `crates/php-stdmisc`  — ext/standard: crypt/password, exec/proc_open, network/dns, env/info, highlight, getopt, http_build_query/parse_str, locale
- `crates/php-phar`     — phar/tar/zip archive formats, manifest, signatures, stub, phar:// semantics (ref: ext/phar)
- `crates/php-dom`      — ext/dom (legacy DOMDocument API + PHP 8.4 Dom\\* HTML5 API) over php-libxml (ref: ext/dom, ext/lexbor)
- `crates/php-simplexml` — SimpleXMLElement semantics over php-libxml (ref: ext/simplexml)
- `crates/php-xmlreader` — XMLReader pull parser over php-libxml (ref: ext/xmlreader)
- `crates/php-pdo`      — PDO core: PDO/PDOStatement semantics, fetch modes, error modes, driver trait (ref: ext/pdo)
- `crates/php-soap`     — SOAP core: WSDL/XSD parser, SOAP 1.1/1.2 encoding of values, packet build/parse, HTTP transport glue (ref: ext/soap)
- `crates/php-gd`       — libgd binding with ext/gd semantics (system libgd 2.3.3; GD_BUNDLED is false) (ref: ext/gd)
- `crates/php-ext-zendtest` — ext/zend_test: the engine test extension gating `--EXTENSIONS-- zend_test` tests (ref: ext/zend_test)
- `crates/php-opcache` + `crates/php-ext-opcache` — compiled-script cache, file cache, invalidation, status, JIT config (ref: ext/opcache)

## Rules
- Idiomatic, best-practice Rust: clippy pedantic clean, no `unsafe` unless justified in a comment (in practice only in FFI modules), no `unwrap()` in library code paths that can fail, thorough unit tests in every crate.
- Match PHP output byte-for-byte: error/warning message text, line numbers, `var_dump` formatting, float printing.
- Each crate owns its public API; coordinate via the types in `php-runtime`.
- Track progress in `FEATURE_GAP.md` (update the % column for rows you advance; keep rows at 100% as the completion record).
- A deliberate difference from php-src (agreed first) goes into `DIFFERENCES.md` in the same commit: what Buzz does, what php-src does, why, where, which test.
- Run `cargo fmt && cargo clippy --all-targets && cargo test` (scoped with `-p`) before declaring work done.

## Performance
The target is parity or better with `/usr/bin/php`. `scripts/acceptance.sh` builds a release `php` (in `~/.cache/phprust-perf-acceptance`, outside the repo) and runs `perf/bench/acceptance.py`, which counts user-space instructions per workload and compares each count against the **goal**: the instruction counts measured for the C implementation, recorded in `perf/bench/goal.tsv`. The acceptance criterion is one number per workload — candidate instructions ≤ 1.10 × goal — and the check passes only when **every** workload meets it. It currently fails on 71 of 82 workloads (11 meet the criterion). Regenerate the goal with `acceptance.py --record` (measures `/usr/bin/php`). Run the check after touching hot paths: the VM executor and php-runtime value/array/string code. Options: `--filter RX`, `ACCEPTANCE_SET=zend,micro,suite,ops`, `PHP_BIN=<path>` to skip the build. `perf/README.md` explains the measuring method (instruction counts, not wall clock, because the box is loaded). In hot paths, avoid needless allocation, cloning and indirection.

## Parallel work
Several agents work concurrently, one crate each. Only edit files inside your own crate (plus your row(s) in FEATURE_GAP.md). Never edit workspace Cargo.toml; crates are pre-registered. Commit your own crate's files with `git add crates/<yours>` only.

When coordinating subagents on a large task:
- Give each worker its own git worktree and branch, created from one agreed integration commit, and a bounded work package.
- Workers edit and review in parallel but do not build or test. The coordinator integrates all worker commits, then runs one consolidated build plus the affected crate, PHPT and perf checks.
- Only the coordinator updates FEATURE_GAP.md and `phpt-baseline.txt`, from verified results after integration.
- Remove a worktree only once its changes are integrated and it is clean. Never force-remove uncommitted work.
- Run long builds, test suites and phpt runs in the background instead of blocking on them. Never foreground-wait on builds, suites or agents; act on completion notifications and use the wait for reviews, docs and commits.

## Resource limits (the box has OOM-crashed once)
- **Use at most 3 CPUs in total** (the owner needs the rest of the box): `.cargo/config.toml` caps `cargo` at 3 jobs and test binaries at 3 threads (`RUST_TEST_THREADS`); never override it with `-j`. Run the PHPT runner with `-j 3` (its default is every CPU), and run one heavy job at a time: never a build, a test suite, a PHPT run or a profile alongside another.
- EVERY invocation of `/usr/bin/php` (oracle scripts, fixture generators) MUST pass `-d memory_limit=1G`. Ubuntu's CLI php.ini sets `memory_limit=-1`, and one unbounded script took the machine down. (Our own `php` caps its address space with `RLIMIT_AS` in `crates/php-cli/src/main.rs`.)
- Do not run more than one `cargo test`/`cargo build` at a time from a single agent; do not run the whole php-src suite with `--php /usr/bin/php` (use subsets).

## Test services
- MariaDB 10.11 is installed for ext/mysqli tests: database `test`, user `phptest` / password `phptest` on 127.0.0.1:3306 (set `MYSQL_TEST_HOST=127.0.0.1 MYSQL_TEST_USER=phptest MYSQL_TEST_PASSWD=phptest MYSQL_TEST_DB=test` when running those tests, e.g. via the runner's `--env`). Start/stop with `sudo systemctl start|stop mariadb`.
