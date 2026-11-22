# buzzs-rust-php-project

David Buzz's Rust PHP project: an independent re-implementation of PHP in Rust whose goal is
to pass the php-src `.phpt` test suite, 100%, and have runtime performance as-good-or-better
than the classic C impl. Passing tests: 99.9% (100% on the engine suite);
Speed (as fast as C): 17 of 82 workloads (21%).

The original PHP is written in C. This project ports it to idiomatic, best-practice Rust
(clippy-pedantic clean, `unsafe` only in audited FFI modules), matching PHP's observable
behaviour byte for byte: error messages, line numbers, `var_dump` layout, float printing.

Current implementation coverage, test results, and remaining work are tracked in
[FEATURE_GAP.md](FEATURE_GAP.md); where Buzz deliberately differs from php-src is
listed in [DIFFERENCES.md](DIFFERENCES.md).

## Layout

A Cargo workspace; one crate per concern (engine) or per PHP extension (libraries). The full
list with the php-src files each one ports is in [CLAUDE.md](CLAUDE.md). The important ones:

| Crate | Role |
|---|---|
| `php-lexer`, `php-parser` | tokenizer and PHP 8.x parser/AST |
| `php-runtime` | zval, zend_string, ordered hash table, operators, type juggling, var_dump/serialize |
| `php-vm`, `php-cli` | compiler, executor, classes, exceptions, generators, fibers; the `php` binary |
| `phpt-runner` | Rust port of `run-tests.php`; runs `.phpt` files and reports pass rates |
| `php-*` (the rest) | one library per extension, each with its own oracle-driven test suite |

## Building and running

```sh
cargo build -p php-cli -p phpt-runner           # builds target/debug/php and the runner
target/debug/php -r 'echo 1+2, "\n";'           # 3
target/debug/php script.php
```

Run the php-src suite (or any subdirectory of it) against our binary:

```sh
env -u LANGUAGE target/debug/phpt-runner --php target/debug/php -j 8 -q --no-artifacts --summary-depth 2 modules/php-src/tests
target/debug/phpt-runner --php target/debug/php -j 8 -q --no-artifacts modules/php-src/Zend/tests
```

`env -u LANGUAGE` matters only if your shell exports `LANGUAGE` (glibc's gettext lets it override
`LC_ALL`, which fails six ext/gettext tests under any PHP, including `/usr/bin/php`). Drop `-q` to list every test; add `--show-diff` to see expected-vs-actual for failures.
`--json report.json` plus `scripts/update_feature_gap.py report.json` rewrites the `%` column
of FEATURE_GAP.md from a run. `phpt-baseline.txt` lists known failures so CI only fails on
regressions (`--baseline phpt-baseline.txt`, refresh with `--update-baseline`).

Per-crate checks, as required before any work is declared done:

```sh
cargo fmt && cargo clippy --all-targets -p <crate> && cargo test -p <crate>
```

Use these incremental dev-profile builds for correctness checks and scoped PHPT
runs. Reserve `--release` for performance measurements: the release profile uses
thin LTO and one codegen unit, which makes rebuilding test executables expensive.
Keep build directories consistent and avoid `cargo clean` during edit/test loops.
For multiple affected crates, combine their `-p` selectors in one command.

## Performance

The goal is to be as fast as, or faster than, the C implementation on every workload. It is
measured per workload: each one may cost at most 1.10x the C implementation's measured
user-space instruction count (the goal recorded in `perf/bench/goal.tsv`, checked by
`scripts/acceptance.sh`). **17 of the 82 workloads meet the criterion** (release build of
`3abd4d3f`, 16 Nov 2026): best are `zend:simplecall` at 0.67x, `ops:empty_loop` at 0.73x and
`ops:int_add` at 0.74x. The other 65 are slower than C PHP, from 1.28x on
`micro:self_cc_x_eq_0` to 10.39x on `zend:ackermann`, with user function calls at 5.41x.
The performance work is in progress.

## Reference source

`modules/php-src/` is a READ-ONLY copy of https://github.com/php/php-src — the specification
this port is written against, and the source of the `.phpt` tests. Only the test directories
are kept (`tests/`, `Zend/tests/`, `ext/**/tests/`, `sapi/**/tests/`: 23,792 `.phpt` files,
against 237 MB for a full clone), and it is gitignored. Fetch it with

```sh
scripts/fetch-phpt-tests.sh
```

The tests are under the PHP License 3.01. The script pins the revision the conformance numbers
below were measured against — `0c7255dc521` ("Merge branch 'PHP-8.6'", 2 Nov 2026) — because
`phpt-baseline.txt` and the pass rate are only meaningful for one php-src revision;
`scripts/fetch-phpt-tests.sh --verify` checks an existing checkout against that pin. Moving to
a newer php-src means editing the pin and re-baselining.

`/usr/bin/php` (PHP 8.3.6) is used as an oracle by the library test suites. Where 8.3 and
php-src master differ, the port follows master.

## License

buzzs-rust-php-project is licensed under the BSD-3-Clause, the same license PHP master is
distributed under, with the copyright held by David Buzz:

```
Copyright © 2025–2026, David Buzz <davidbuzz@gmail.com>
```

Third-party components keep their own licenses (the oracle tree under
`modules/php-src` is PHP-3.01, © The PHP Group; `tools/lcov` is GPL-2.0-or-later;
linked system libraries keep theirs). See [LICENSE](LICENSE).

If you want to reuse files from this repository, the [LICENSE](LICENSE) file
includes a plain-language addendum listing exactly what compliance requires:
keep the per-file copyright header, ship the license text, don't use the
project or author name for endorsement, and honor the original notices on
the few ported PHP/Zend files. Attribution of the notices is the whole of
the burden — no copyleft, no share-alike, no fee.

## Related work

Projects with a related thesis or implementation:

- **[francescotinti/php-rust](https://github.com/francescotinti/php-rust)** /
  [phprust.com](https://phprust.com/) — a Rust PHP implementation targeting PHP 8.5,
  using the mago parser and a HIR-to-bytecode pipeline. Its methodology is very
  different from ours: it does not pass the `.phpt` test suite and does not target
  performance or completeness, so the two projects could not interoperate.
- **[jolicode/rphp](https://github.com/jolicode/rphp)** — AI-assisted experiment
  ([write-up](https://jolicode.com/blog/writing-the-php-virtual-machine-in-rust-with-a-lot-of-help-from-ai))
  exploring a Rust PHP VM and copy-on-write request forking.
- Earlier/smaller efforts: `rust-php/php`, `tagua-vm` (archived), various experimental
  interpreters and parsers (`wudi/php-parser`, `jorgsowa/rust-php-parser`);
  [mago](https://github.com/carthage-software/mago) is a PHP toolchain (not an engine)
  in Rust.

This project follows php-src `master`, organizes extensions as per-extension crates,
and uses `.phpt` tests and per-workload instruction counts as its compatibility and
performance measures.

## Resource notes

Many agents build concurrently on one machine, so `.cargo/config.toml` caps `cargo` at 6
jobs, every oracle `php` invocation passes `-d memory_limit=1G`, and the `php` binary caps its
own address space (at the larger of 4 GiB and 75% of RAM plus swap; `PHPRUST_MAX_AS_MB`
overrides it). See the "Resource limits" section of CLAUDE.md.
