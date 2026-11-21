# buzzs-rust-php-project — Copilot instructions

A Rust re-implementation of PHP whose goal is to pass the php-src `.phpt` test suite.
Success is defined by **byte-for-byte output matching** with PHP: error/warning message
text, line numbers, `var_dump` formatting, float printing — and by **performance: the Rust
implementation must be as fast as the C implementation (`/usr/bin/php`) or faster**.

## Ground rules

- `modules/php-src/` is a **READ-ONLY** reference clone of php-src (gitignored; the
  specification this port is written against and the source of the `.phpt` tests). Never edit it.
- `/usr/bin/php` (PHP 8.3.6) is an oracle for expected behaviour. **Every** invocation of it
  MUST pass `-d memory_limit=1G` (the system php.ini sets `memory_limit=-1` and an unbounded
  script has OOM-crashed the machine). Where 8.3 and php-src master differ, follow master.
- Track progress in `FEATURE_GAP.md`: update the `%` column for rows you advance; keep rows at
  100% as the completion record. The full crate → php-src source mapping is in `CLAUDE.md`.

## Build, test, lint

```sh
cargo build -p php-cli -p phpt-runner    # builds target/debug/php and the runner
target/debug/php -r 'echo 1+2, "\n";'    # quick smoke test

# Required before declaring any work done:
cargo fmt && cargo clippy --all-targets -p <crate> && cargo test -p <crate>

# Run one Rust unit test:
cargo test -p <crate> <test_name_substring>
```

Run `.phpt` tests (the real acceptance criterion) with the runner:

```sh
# A single test or directory:
target/debug/phpt-runner --php target/debug/php modules/php-src/Zend/tests/foo.phpt
target/debug/phpt-runner --php target/debug/php -j 8 -q --no-artifacts modules/php-src/ext/json

# Useful flags: --show-diff (expected vs actual), --filter <substr> (repeatable),
# --list, -d NAME=VALUE, --env KEY=VALUE
```

- `env -u LANGUAGE` before the runner if your shell exports `LANGUAGE` (breaks ext/gettext tests
  under any PHP).
- `phpt-baseline.txt` lists known failures; CI fails only on regressions
  (`--baseline phpt-baseline.txt`; refresh with `--update-baseline`).
- `phpt-scope.txt` lists out-of-scope path prefixes (opcache, FPM, pgsql, ffi, …) excluded from
  pass rates.
- `--json report.json` + `scripts/update_feature_gap.py report.json` rewrites FEATURE_GAP.md's
  `%` column from a run (`--summary` prints the headline table).
- `scripts/acceptance.sh` is the performance acceptance check: it builds a release `php` and
  compares user-space instruction counts per workload against the **goal** — the instruction
  counts measured for the C implementation, recorded in `perf/bench/goal.tsv`. The criterion
  is one number per workload: candidate ≤ 1.10 × goal. The check passes only when every
  workload meets it (the final deliverable); there are no progressive checkpoints. Regenerate
  the goal with `perf/bench/acceptance.py --record`. Run the check after changes to hot paths
  (VM executor, runtime value/array/string code).

## Architecture

Cargo workspace, ~100 crates, strictly layered:

1. **Engine pipeline**: `php-lexer` (tokenizer) → `php-parser` (AST, exact parse-error
   messages) → `php-vm` (compiler to op arrays + executor, classes, exceptions, generators,
   fibers) → `php-cli` (the `php` binary; also `php-cli-server` for `php -S`).
2. **`php-runtime`** is the shared vocabulary: `Value` (zval), `ZStr` (zend_string), `ZArray`
   (ordered hash table), operators/type juggling. All crates coordinate through its types.
3. **Library / binding split**: each PHP extension is two crates.
   - `php-<name>` — pure logic, VM-independent, with its own oracle-driven test suite
     (e.g. `php-json`, `php-pcre`, `php-date`).
   - `php-ext-<name>` — thin binding through `php_vm::ext` (`Extension`, `ModuleBuilder`,
     `ClassBuilder`, `Args`/ZPP). The library crate talks back to the VM via a host trait the
     ext crate implements (see `crates/php-ext-json/src/host.rs`). Per-request state lives in
     thread-locals reset at request shutdown. `php-cli` depends on every ext crate and
     registers them at startup (`crates/php-cli/src/startup.rs`).
4. **`phpt-runner`** — Rust port of `run-tests.php`; parses all `.phpt` sections, parallel
   scheduling, baseline/scope handling.

## Conventions

- Idiomatic, clippy-pedantic-clean Rust. No `unsafe` outside audited FFI modules (and only with
  a justifying comment); no `unwrap()` in library code paths that can fail.
- Every crate's doc comments cite the php-src file(s) it ports (e.g.
  `ref: Zend/zend_hash.c`); match that C source's observable behaviour exactly.
- When behaviour is unclear, write a small script and run it through the oracle
  (`/usr/bin/php -d memory_limit=1G`) rather than guessing.
- Don't trade speed for convenience in hot paths: avoid needless allocation, cloning, and
  indirection in the executor and `php-runtime` core types. The target is to match or beat
  the C implementation; `scripts/acceptance.sh` enforces this per workload.

## Workflow

- **Never foreground-wait on build/test/suite runs.** Start builds, test suites, and phpt
  runs as background tasks and use the wait time productively: update documentation, make
  pending git commits, review diffs, or plan the next work items. Check results when the
  background task completes, not by blocking on it.
- Keep chat responsive while background work runs. Do not hold a turn open waiting for
  workers or commands: launch asynchronously, report status, and return control to the user.
  Handle new requests while existing work continues, coordinating file ownership and
  resource limits. Read results after completion notifications rather than blocking or
  polling; a new message does not cancel existing work unless the user redirects it.

## Parallel agents

For substantial multi-part tasks, target **six concurrent subagents**, each with its own
Git worktree and branch:

- Create each worker's worktree from an agreed integration commit before launching it.
  Give the worker an explicit worktree path and bounded work package.
- Workers edit only their assigned crate(s) in their own worktree. Independent VM work
  packages may run concurrently in separate worktrees; do not serialize them merely
  because they touch the same crate. Coordinate shared APIs and overlapping changes.
- Use background agents and give regular user-facing progress updates throughout execution,
  not only a final summary. At each meaningful milestone, report each worker's work package,
  current status, verified test/performance results, blockers, and next step. Clearly
  distinguish implementation in progress from validated and integrated completion.
- Integrate worker commits deliberately, resolve conflicts, and rerun affected tests and
  performance workloads on the combined result. Worktree isolation does not remove the
  need for integration validation.
- Workers make edits and perform source/diff review in parallel, without per-worker
  builds or test runs. Integrate all worker changes first, then run one consolidated
  build and combined affected-crate/PHPT/performance validation at the end. Preserve
  resource limits. If validation fails, fix the combined result and rerun only the
  necessary checks; do not claim unvalidated worker commits are complete.
- The coordinating agent updates FEATURE_GAP.md and the baseline from verified results
  after integration; workers report evidence rather than competing to edit those files.
- Never edit the workspace `Cargo.toml`; crates are pre-registered.
- Commit with `git add crates/<yours>` only.
- Remove worker worktrees only after their changes are integrated or explicitly retained,
  and their worktrees are clean. Do not force-remove uncommitted work.

## Resource limits (the box has OOM-crashed once)

- `.cargo/config.toml` caps cargo at **6 jobs**; never override with `-j`.
- Run at most one `cargo test`/`cargo build` at a time per agent.
- Do not run the whole php-src suite with `--php /usr/bin/php`; use subsets.

## Test services

- MariaDB 10.11 for ext/mysqli tests: db `test`, user `phptest`/`phptest` on 127.0.0.1:3306.
  Set `MYSQL_TEST_HOST=127.0.0.1 MYSQL_TEST_USER=phptest MYSQL_TEST_PASSWD=phptest
  MYSQL_TEST_DB=test`. Start/stop: `sudo systemctl start|stop mariadb`.
