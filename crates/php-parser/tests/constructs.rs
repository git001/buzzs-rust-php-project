// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Per-construct parser tests.
//!
//! Every snippet is parsed inside `assert(false && (...))` and the AST is
//! exported back to PHP code; the result must equal the `AssertionError`
//! message PHP prints for the same code, which pins down both the parse
//! (precedence, associativity, node kinds) and the exporter. Expected
//! messages are cached in `tests/fixtures/construct_exports.tsv`
//! (regenerate with `cargo test -p php-parser --test constructs -- --ignored`).

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_parser::ast::{ExprKind, StmtKind};
use php_parser::export::assert_message;
use php_parser::ParseOptions;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Expressions (wrapped as `false && (EXPR)`).
const EXPRS: &[&str] = &[
    // precedence and associativity
    "1 + 2 * 3",
    "(1 + 2) * 3",
    "1 - 2 - 3",
    "1 - (2 - 3)",
    "2 ** 3 ** 2",
    "(2 ** 3) ** 2",
    "-2 ** 2",
    "(-2) ** 2",
    "!$a instanceof B",
    "!$a + 1",
    "$a . $b + $c",
    "$a << 1 + 2",
    "$a & $b | $c ^ $d",
    "$a && $b || $c && $d",
    "$a and $b or $c xor $d",
    "$a ?? $b ?? $c",
    "($a ?? $b) ?? $c",
    "$a ? $b : ($c ? $d : $e)",
    "($a ? $b : $c) ? $d : $e",
    "$a ?: $b",
    "$a ? $b ?: $c : $d",
    "$a = $b = $c",
    "$a + $b = 3",
    "!$a = foo()",
    "$a += $b -= 1",
    "$a ??= []",
    "$a .= 'x'",
    "$a **= 2",
    "$a = &$b",
    "$a = &foo()",
    "$a == $b",
    "$a != $b",
    "$a <> $b",
    "$a === $b",
    "$a !== $b",
    "$a < $b",
    "$a <= $b",
    "$a > $b",
    "$a >= $b",
    "$a <=> $b",
    "$a == ($b == $c)",
    "$a < $b == $c > $d",
    "$a++ + ++$b",
    "$a-- - --$b",
    "-$a",
    "+$a",
    "~$a",
    "@foo()",
    "(int) $a",
    "(float) $a",
    "(string) $a",
    "(bool) $a",
    "(array) $a",
    "(object) $a",
    "(int) $a + 1",
    "print $a . $b",
    "print $a and $b",
    "include 'a.php'",
    "require_once __DIR__ . '/x.php'",
    "eval('1;')",
    "isset($a, $b[1], $c->d)",
    "empty($a)",
    "$a instanceof $b",
    "$a instanceof static",
    "$a instanceof \\Foo\\Bar",
    "clone $a",
    "clone $a->b",
    "throw new Exception('x')",
    "exit",
    "exit(1)",
    "die('x')",
    // literals
    "1.5",
    "1.0",
    "1e100",
    "0x1F + 0b11 + 0o17 + 017",
    "1_000_000",
    "9223372036854775807",
    "9223372036854775808",
    "'a\\'b\\\\c'",
    "\"a\\tb\\n\"",
    "\"a $b c\"",
    "\"a {$b->c} {$d['e']} ${f}\"",
    "\"$a[0] $a[x] $a[-1] $a->b $a?->c\"",
    "`ls $dir`",
    "<<<EOT\n  a $b\n  EOT",
    "<<<'EOT'\n  raw $x\n  EOT",
    "[1, 2, 'a' => 3, ...$x]",
    "array(1, 2)",
    "[[1, 2], [3]]",
    "[&$a, 'k' => &$b]",
    "__LINE__ . __FILE__ . __DIR__ . __FUNCTION__ . __CLASS__ . __METHOD__ . __NAMESPACE__ . __TRAIT__",
    "true && FALSE && null",
    "\\PHP_EOL",
    "namespace\\FOO",
    "Foo\\BAR",
    // variables and dereferencing
    "$$a",
    "${'a b'}",
    "${$a . 'b'}",
    "$a[1][2]",
    "$a[]",
    "$a->b->c",
    "$a->$b",
    "$a->{'b c'}",
    "$a->{$b . 'c'}",
    "$a?->b?->c()",
    "A::$b",
    "A::$$b",
    "A::B",
    "A::class",
    "$a::class",
    "A::{'B'}",
    "static::$a",
    "self::B",
    "parent::foo()",
    "A::B::C",
    "A::$b::$c",
    "A::b()::c()",
    "'abc'[0]",
    "[1, 2][0]",
    "FOO[0]",
    "FOO->bar",
    "(new A)->b",
    "new A()->b",
    "new A(1, 2)",
    "new A",
    "new $a",
    "new $a->b",
    "new $a['b']",
    "new (foo())",
    "new static",
    "new class {}",
    "new class(1) extends A implements B, C { public $a = 1; }",
    "new readonly class {}",
    // calls
    "foo()",
    "\\foo(1, 2)",
    "namespace\\foo()",
    "$f()",
    "$f()()",
    "(function () {})()",
    "'strlen'('x')",
    "foo(...$a)",
    "foo(a: 1, b: 2)",
    "foo(1, ...$a)",
    "foo(...)",
    "$a->b(...)",
    "A::b(...)",
    "$a->b()",
    "$a->$b()",
    "$a->{'b'}()",
    "A::$b()",
    "$a::b()",
    "$a['b']()",
    "foo(1)(2)",
    // closures and arrow functions
    "function () {}",
    "function &($a, &$b, ...$c) use ($d, &$e): ?int { return 1; }",
    "static function () {}",
    "fn($x) => $x + 1",
    "fn&($x) => $x",
    "static fn(): int => 1",
    "fn($x) => fn($y) => $x + $y",
    "(fn() => 1)()",
    "fn() => $a and $b",
    "function (int|string $a, A&B $b, ?C $c = null, (A&B)|null $d = null): static {}",
    "function (array $a = [], callable $b = null, self $c = null) {}",
    "#[A] function () {}",
    "#[A(1), B] fn() => 1",
    // match / yield
    "match ($a) { 1, 2 => 'a', default => 'b' }",
    "match (true) { $a > 1 => 1, }",
    "function () { yield; }",
    "function () { yield 1; }",
    "function () { yield 1 => 2; }",
    "function () { yield from foo(); }",
    "function () { $a = yield $b; }",
    "function () { $a = (yield $b) + 1; }",
    // list destructuring
    "list($a, $b) = $c",
    "[$a, [$b, $c]] = $d",
    "['a' => $a, 'b' => $b] = $c",
    "[, $b] = $c",
    "[$a, , $c] = $d",
    "list($a, list($b)) = $c",
    "[&$a, $b] = $c",
];

/// Statements (wrapped as `false && function () { STMTS }`).
const STMTS: &[&str] = &[
    "if ($a) { b(); } elseif ($c) { d(); } else { e(); }",
    "if ($a): b(); elseif ($c): d(); else: e(); endif;",
    "if ($a) b(); else if ($c) d();",
    "while ($a) { $a--; }",
    "while ($a): $a--; endwhile;",
    "do { $a++; } while ($a < 10);",
    "for ($i = 0, $j = 1; $i < 10; $i++, $j++) {}",
    "for (;;) { break; }",
    "for ($i = 0; $i < 3; $i++): continue; endfor;",
    "foreach ($a as $v) {}",
    "foreach ($a as $k => $v) {}",
    "foreach ($a as &$v) {}",
    "foreach ($a as [$x, $y]) {}",
    "foreach ($a as $k => list($x, $y)): endforeach;",
    "switch ($a) { case 1: case 2: b(); break; default: c(); }",
    "switch ($a): case 1; b(); endswitch;",
    "while (1) { while (2) { break 2; } continue; }",
    "return;",
    "return $a;",
    "global $a, $$b;",
    "static $a, $b = 1;",
    "echo 1, 2;",
    "print 1;",
    "unset($a, $b[1]);",
    "try { a(); } catch (A|B $e) { b(); } catch (C) { } finally { c(); }",
    "try { a(); } finally { b(); }",
    "goto a; a: echo 1;",
    "declare(ticks=1) { a(); }",
    "throw $e;",
    "$a; $b;",
    "{ a(); { b(); } }",
    ";",
    "function f() {} class A {}",
    "class A extends B implements C, D { const X = 1, Y = 2; public $a; protected static ?int $b = 1; private readonly array $c; public function __construct(public int $x = 0) {} abstract protected function f(); final public static function &g(int ...$a): static { return $this; } }",
    "abstract class A { abstract function f(); }",
    "final class A {}",
    "interface I extends J, K { const A = 1; function f(); }",
    "trait T { use U, V { U::f insteadof V; V::f as g; f as protected; f as private h; } }",
    "enum E: string implements I { case A = 'a'; case B = 'b'; const C = self::A; public function f() {} }",
    "enum F { case A; case B; }",
    "class A { #[X] public $a; #[Y(1)] public function f(#[Z] $a) {} }",
    "class A { public const int X = 1; final public const Y = 2; }",
    "class A { var $a = 1; }",
];

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/construct_exports.tsv")
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn wrap(snippet: &str, is_stmt: bool) -> String {
    if is_stmt {
        format!("assert(false && function () {{ {snippet}\n}});")
    } else {
        format!("assert(false && ({snippet}\n));")
    }
}

fn all_snippets() -> Vec<(String, bool)> {
    EXPRS
        .iter()
        .map(|s| ((*s).to_owned(), false))
        .chain(STMTS.iter().map(|s| ((*s).to_owned(), true)))
        .collect()
}

/// Our `assert()` message for the wrapped snippet.
fn ours(code: &str) -> String {
    let src = format!("<?php {code}");
    let program = match php_parser::parse(src.as_bytes(), &ParseOptions::default()) {
        Ok(p) => p,
        Err(e) => return format!("ERROR: {e}"),
    };
    let Some(StmtKind::Expr(e)) = program.statements.first().map(|s| &s.kind) else {
        return "ERROR: not an expression statement".into();
    };
    let ExprKind::Call { args, .. } = &e.kind else {
        return "ERROR: not a call".into();
    };
    String::from_utf8_lossy(&assert_message(&args.args[0])).into_owned()
}

#[test]
#[ignore = "needs /usr/bin/php; regenerates tests/fixtures/construct_exports.tsv"]
fn regenerate() {
    use std::process::Command;
    let tmp =
        std::env::temp_dir().join(format!("php-parser-constructs-{}.php", std::process::id()));
    let mut out = String::new();
    for (s, is_stmt) in all_snippets() {
        let code = wrap(&s, is_stmt);
        let script = format!(
            "<?php\ntry {{ {code} }} catch (Throwable $e) {{ echo \"MSG:\", $e->getMessage(); }}"
        );
        std::fs::write(&tmp, script).unwrap();
        let o = Command::new("php")
            .args(["-n", "-d", "zend.assertions=1", "-d", "display_errors=1"])
            .arg(&tmp)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&o.stdout);
        let msg = stdout
            .split_once("MSG:")
            .map_or_else(|| format!("PHP: {}", stdout.trim()), |(_, m)| m.to_owned());
        let _ = writeln!(out, "{}\t{}", escape(&s), escape(&msg));
    }
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(fixture(), out).unwrap();
}

/// Snippets whose output differs between the oracle PHP (8.3) and php-src
/// master, with the master output (`tests/fixtures/construct_exports_master.tsv`,
/// reviewed against `zend_ast_export_ex` in php-src master: `clone`/`exit`
/// as functions, `(float)`, escaped control characters, braces around
/// interpolated variables, `.0` on integral floats, parenthesized dynamic
/// class names and callees, parenthesized arrow functions, promoted
/// property visibility, `final` constants, `readonly` anonymous classes).
fn master_overrides() -> Vec<(String, String)> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/construct_exports_master.tsv");
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (unescape(a), unescape(b)))
        .collect()
}

#[test]
fn exports_match_php() {
    let Ok(data) = std::fs::read_to_string(fixture()) else {
        eprintln!("no fixture; skipping");
        return;
    };
    let snippets = all_snippets();
    let overrides = master_overrides();
    let mut failures = String::new();
    let mut new_overrides = String::new();
    let mut n = 0;
    for line in data.lines() {
        let Some((snip, expected)) = line.split_once('\t') else {
            continue;
        };
        let snip = unescape(snip);
        let Some((s, is_stmt)) = snippets.iter().find(|(s, _)| *s == snip) else {
            continue;
        };
        let expected = overrides
            .iter()
            .find(|(o, _)| o == s)
            .map_or_else(|| unescape(expected), |(_, m)| m.clone());
        if expected.starts_with("PHP:") {
            // syntax PHP 8.3 rejects (newer features): only check it parses
            assert!(!ours(&wrap(s, *is_stmt)).starts_with("ERROR"), "{s}");
            continue;
        }
        n += 1;
        let got = ours(&wrap(s, *is_stmt));
        if got != expected && std::env::var_os("CONSTRUCTS_WRITE_OVERRIDES").is_some() {
            // Maintenance aid: record our output as the master override
            // (review the diff before committing it).
            let _ = writeln!(new_overrides, "{}\t{}", escape(s), escape(&got));
            continue;
        }
        if got != expected {
            let _ = writeln!(failures, "{s}\n    php:  {expected:?}\n    ours: {got:?}");
        }
    }
    if !new_overrides.is_empty() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/construct_exports_master.tsv");
        let mut all = std::fs::read_to_string(&path).unwrap_or_default();
        all.push_str(&new_overrides);
        std::fs::write(path, all).unwrap();
    }
    assert!(n > 100, "fixture too small ({n})");
    assert!(failures.is_empty(), "mismatches:\n{failures}");
}

#[test]
fn spans_and_lines() {
    let src = b"<?php\n$a = 1;\nfunction f(\n  $x\n) {\n  return $x;\n}\nclass C {\n}\n";
    let p = php_parser::parse(src, &ParseOptions::default()).unwrap();
    assert_eq!(p.statements.len(), 3);
    assert_eq!(p.statements[0].span.line, 2);
    let StmtKind::Function(f) = &p.statements[1].kind else {
        panic!("function expected")
    };
    assert_eq!(f.span.line, 3);
    assert_eq!(f.end_line, 7);
    assert_eq!(f.params[0].span.line, 4);
    let StmtKind::Class(c) = &p.statements[2].kind else {
        panic!("class expected")
    };
    assert_eq!((c.span.line, c.end_line), (8, 9));
    assert_eq!(&src[c.span.start..c.span.end], b"class C {\n}");
}

#[test]
fn generators_and_halt_compiler() {
    let src = b"<?php function g() { yield 1; } function h() { $f = function () { yield; }; }\n__halt_compiler(); garbage ?> <?php !!";
    let p = php_parser::parse(src, &ParseOptions::default()).unwrap();
    let StmtKind::Function(g) = &p.statements[0].kind else {
        panic!()
    };
    assert!(g.is_generator);
    let StmtKind::Function(h) = &p.statements[1].kind else {
        panic!()
    };
    assert!(!h.is_generator);
    let off = p.halt_compiler_offset.unwrap();
    assert_eq!(&src[off..off + 8], b" garbage");
}

#[test]
fn eval_mode_and_short_tags() {
    assert!(php_parser::parse_eval(b"return 1 + 2;").is_ok());
    let opts = ParseOptions {
        short_open_tag: true,
        ..ParseOptions::default()
    };
    let p = php_parser::parse(b"<? echo 1; ?>", &opts).unwrap();
    assert!(matches!(p.statements[0].kind, StmtKind::Echo(_)));
    let p = php_parser::parse(b"<? echo 1; ?>", &ParseOptions::default()).unwrap();
    assert!(matches!(p.statements[0].kind, StmtKind::InlineHtml(_)));
}

#[test]
fn doc_comments() {
    let src = b"<?php\n/** fdoc */\nfunction f() {}\n/** cdoc */\nclass C {\n  /** pdoc */ public $p;\n  /** mdoc */ public function m() {}\n}\n";
    let program = php_parser::parse(src, &ParseOptions::default()).unwrap();
    let StmtKind::Function(func) = &program.statements[0].kind else {
        panic!()
    };
    assert_eq!(func.doc_comment.as_deref(), Some(&b"/** fdoc */"[..]));
    let StmtKind::Class(class) = &program.statements[1].kind else {
        panic!()
    };
    assert_eq!(class.doc_comment.as_deref(), Some(&b"/** cdoc */"[..]));
    let php_parser::ast::ClassMemberKind::Property(group) = &class.members[0].kind else {
        panic!()
    };
    assert_eq!(
        group.props[0].doc_comment.as_deref(),
        Some(&b"/** pdoc */"[..])
    );
    let php_parser::ast::ClassMemberKind::Method(method) = &class.members[1].kind else {
        panic!()
    };
    assert_eq!(method.doc_comment.as_deref(), Some(&b"/** mdoc */"[..]));
}

#[test]
fn deep_nesting() {
    let n = 3000;
    let src = format!("<?php $a = {}1{};", "(".repeat(n), ")".repeat(n));
    assert!(php_parser::parse(src.as_bytes(), &ParseOptions::default()).is_ok());
    let n = 12000;
    let src = format!("<?php $a = {}1{};", "(".repeat(n), ")".repeat(n));
    let e = php_parser::parse(src.as_bytes(), &ParseOptions::default()).unwrap_err();
    assert_eq!(e.message, "memory exhausted");
}

#[test]
fn syntax_only_skips_compile_checks() {
    let src = b"<?php foreach ($a as &$k => $v) {}";
    let e = php_parser::parse(src, &ParseOptions::default()).unwrap_err();
    assert_eq!(e.message, "Key element cannot be a reference");
    assert_eq!(e.line, 1);
    let opts = ParseOptions {
        syntax_only: true,
        ..ParseOptions::default()
    };
    let p = php_parser::parse(src, &opts).unwrap();
    assert_eq!(p.foreach_ref_keys, vec![6]);
}

#[test]
fn encoding_declare_warning() {
    let opts = ParseOptions {
        syntax_only: true,
        ..ParseOptions::default()
    };
    let p = php_parser::parse(b"<?php declare(encoding='UTF-8'\n);", &opts).unwrap();
    assert_eq!(p.warnings.len(), 1);
    assert_eq!(
        p.warnings[0].message,
        "declare(encoding=...) ignored because Zend multibyte feature is turned off by settings"
    );
    assert_eq!(p.warnings[0].line, 2);
    let e = php_parser::parse(b"<?php declare(encoding=FOO);", &opts).unwrap_err();
    assert_eq!(e.message, "Encoding must be a literal");

    // With zend.multibyte on the host honours the declaration, so no warning.
    let opts = ParseOptions {
        multibyte: true,
        ..opts
    };
    let p = php_parser::parse(b"<?php declare(encoding='UTF-8');", &opts).unwrap();
    assert!(p.warnings.is_empty());
    let e = php_parser::parse(b"<?php declare(encoding=FOO);", &opts).unwrap_err();
    assert_eq!(e.message, "Encoding must be a literal");
}

#[test]
fn reserved_class_name_error_keeps_earlier_deprecation() {
    // GH-15976: `class _ {}` deprecates before `class bool {}` fails.
    let opts = ParseOptions::default();
    let e = php_parser::parse(b"<?php\nclass _ {}\nclass bool {}\n", &opts).unwrap_err();
    assert_eq!(
        e.message,
        "Cannot use \"bool\" as a class name as it is reserved"
    );
    assert_eq!(e.warnings.len(), 1);
    assert_eq!(
        e.warnings[0].message,
        "Using \"_\" as a class name is deprecated since 8.4"
    );
    assert_eq!(e.warnings[0].line, 2);
    // On success the compiler raises it, so the validator drops its copy.
    let p = php_parser::parse(b"<?php\ntrait _ {}\n", &opts).unwrap();
    assert!(p.warnings.is_empty());
}

#[test]
fn pipe_rhs_arrow_fn_must_be_parenthesized() {
    let opts = ParseOptions::default();
    let e = php_parser::parse(b"<?php 42 |> fn($x) => $x |> fn($x) => $x;", &opts).unwrap_err();
    assert_eq!(
        e.message,
        "Arrow functions on the right hand side of |> must be parenthesized"
    );
    assert!(php_parser::parse(b"<?php 42 |> (fn($x) => $x) |> (fn($x) => $x);", &opts).is_ok());
}

#[test]
fn identifier_tokens() {
    let src = "<?php namespace list; class A { const FOR = 1; function list() {} use T { T::if as public; } } A::class; A::FOR; $a->list(); f(array: 1); readonly(1); enum E { case Default; } function readonly() {} $x?->foreach; f(new: 1);";
    let opts = ParseOptions {
        record_identifier_tokens: true,
        ..ParseOptions::default()
    };
    let p = php_parser::parse(src.as_bytes(), &opts).unwrap();
    let words: Vec<&str> = p
        .identifier_tokens
        .iter()
        .map(|&o| {
            let rest = &src[o..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            &rest[..end]
        })
        .collect();
    assert_eq!(
        words,
        [
            "list", "FOR", "list", "if", "class", "FOR", "array", "readonly", "Default",
            "readonly", "new"
        ]
    );
    // not recorded unless asked for
    let p = php_parser::parse(src.as_bytes(), &ParseOptions::default()).unwrap();
    assert!(p.identifier_tokens.is_empty());
}

#[test]
fn stack_policies() {
    use php_parser::StackPolicy;
    let n = 3000;
    let src = format!("<?php $a = {}1{};", "[".repeat(n), "]".repeat(n));
    // A thread with a small stack: `Auto` moves to a helper thread,
    // `CurrentThread` gives up like bison's stack overflow would.
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let auto = php_parser::parse(src.as_bytes(), &ParseOptions::default()).is_ok();
            let opts = ParseOptions {
                stack: StackPolicy::CurrentThread,
                ..ParseOptions::default()
            };
            let current = php_parser::parse(src.as_bytes(), &opts).map_err(|e| e.message);
            let opts = ParseOptions {
                stack: StackPolicy::Spawn(64 * 1024 * 1024),
                ..ParseOptions::default()
            };
            let spawned = php_parser::parse(src.as_bytes(), &opts).is_ok();
            (auto, current, spawned)
        })
        .unwrap();
    let (auto, current, spawned) = h.join().unwrap();
    assert!(auto);
    assert_eq!(current.unwrap_err(), "memory exhausted");
    assert!(spawned);
}

/// `array_pair_list` rtrims only the implicit element after the last comma
/// (`zend_language_parser.y`), so an explicit trailing hole stays an error.
#[test]
fn array_trailing_holes() {
    let opts = ParseOptions::default();
    let e = php_parser::parse(b"<?php\nvar_dump([1, 2, ,]);\n", &opts).unwrap_err();
    assert_eq!(e.message, "Cannot use empty array elements in arrays");
    assert_eq!(e.line, 2);
    let e = php_parser::parse(b"<?php [,];", &opts).unwrap_err();
    assert_eq!(e.message, "Cannot use empty array elements in arrays");
    let e = php_parser::parse(b"<?php ['a' => $b, ,] = [];", &opts).unwrap_err();
    assert_eq!(
        e.message,
        "Cannot use empty array entries in keyed array assignment"
    );
    let ok = b"<?php [1, 2,]; [$a, , $b] = [1, 2, 3]; [$a,,] = [1];";
    assert!(php_parser::parse(ok, &opts).is_ok());
}
