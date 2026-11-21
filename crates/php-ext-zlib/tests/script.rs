// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the streams and
//! zlib/bz2 extensions. Expected values come from php-src
//! (`/usr/bin/php`; bz2 results from the php-src test suite).

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let streams = php_ext_streams::Streams;
    let zlib = php_ext_zlib::Zlib;
    let exts: [&dyn Extension; 3] = [&php_ext_stdstr::StdStr, &streams, &zlib];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

/// A per-test temporary file path.
fn tmp(name: &str) -> String {
    let mut p = std::env::temp_dir();
    p.push(format!("php-ext-zlib-{}-{name}", std::process::id()));
    p.to_string_lossy().into_owned()
}

#[test]
fn one_shot_codecs() {
    assert_eq!(
        run(
            r#"echo bin2hex(gzcompress("hello")), "\n", bin2hex(gzdeflate("hello", 9)), "\n",
            gzuncompress(gzcompress("abc")), gzinflate(gzdeflate("def")), gzdecode(gzencode("ghi")),
            zlib_decode(zlib_encode("jkl", ZLIB_ENCODING_GZIP)), "\n";"#
        ),
        "789ccb48cdc9c90700062c0215\ncb48cdc9c90700\nabcdefghijkl\n"
    );
}

#[test]
fn codec_errors() {
    assert_eq!(
        run(r#"try { gzcompress("x", 10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
            var_dump(gzuncompress("garbage"));"#),
        "gzcompress(): Argument #2 ($level) must be between -1 and 9\n\nWarning: gzuncompress(): data error in test.php on line 2\nbool(false)\n"
    );
}

#[test]
fn incremental_contexts() {
    assert_eq!(
        run(r#"$d = deflate_init(ZLIB_ENCODING_DEFLATE, ['level' => 9]);
            $c = deflate_add($d, "abc", ZLIB_NO_FLUSH) . deflate_add($d, "def", ZLIB_FINISH);
            $i = inflate_init(ZLIB_ENCODING_DEFLATE);
            echo inflate_add($i, $c), " ", inflate_get_status($i), " ", inflate_get_read_len($i), "\n";
            var_dump($d);
            try { new InflateContext; } catch (Error $e) { echo $e->getMessage(), "\n"; }
            try { deflate_init(42); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }"#),
        "abcdef 1 14\nobject(DeflateContext)#1 (0) {\n}\nCannot directly construct InflateContext, use inflate_init() instead\ndeflate_init(): Argument #1 ($encoding) must be one of ZLIB_ENCODING_RAW, ZLIB_ENCODING_GZIP, or ZLIB_ENCODING_DEFLATE\n"
    );
}

#[test]
fn gz_streams() {
    let f = tmp("s.gz");
    let out = run(&format!(
        r#"$f = gzopen("{f}", "wb"); var_dump(gzwrite($f, "one\ntwo\n")); gzclose($f);
        $f = gzopen("{f}", "rb"); echo gzgets($f); var_dump(gztell($f), gzeof($f));
        echo gzread($f, 10); var_dump(gzeof($f)); gzrewind($f); echo gzgetc($f), "\n"; gzclose($f);
        print_r(gzfile("{f}")); var_dump(readgzfile("{f}"));
        echo file_get_contents("compress.zlib://{f}"); unlink("{f}");"#
    ));
    assert_eq!(
        out,
        "int(8)\none\nint(4)\nbool(false)\ntwo\nbool(true)\no\nArray\n(\n    [0] => one\n\n    [1] => two\n\n)\none\ntwo\nint(8)\none\ntwo\n"
    );
}

#[test]
fn gzopen_failures() {
    assert_eq!(
        run(
            r#"try { gzopen("a\0b", "r"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
            var_dump(@gzopen("/nonexistent/x", "r"));"#
        ),
        "gzopen(): Argument #1 ($filename) must not contain any null bytes\nbool(false)\n"
    );
}

#[test]
fn bz2_functions() {
    let f = tmp("s.bz2");
    let out = run(&format!(
        r#"echo bzdecompress(bzcompress("hello bz2")), "\n";
        $b = bzopen("{f}", "w"); var_dump(bzwrite($b, "data")); bzclose($b);
        $b = bzopen("{f}", "r"); var_dump(bzread($b), bzerrno($b), bzerrstr($b)); bzclose($b);
        echo file_get_contents("compress.bzip2://{f}"), "\n";
        try {{ bzopen("{f}", "x"); }} catch (ValueError $e) {{ echo $e->getMessage(), "\n"; }}
        try {{ bzopen(1, "r"); }} catch (TypeError $e) {{ echo $e->getMessage(), "\n"; }}
        var_dump(extension_loaded("bz2")); unlink("{f}");"#
    ));
    assert_eq!(
        out,
        "hello bz2\nint(4)\nstring(4) \"data\"\nint(0)\nstring(2) \"OK\"\ndata\nbzopen(): Argument #2 ($mode) must be either \"r\" or \"w\"\nbzopen(): Argument #1 ($file) must be of type string or file-resource, int given\nbool(true)\n"
    );
}

#[test]
fn filters_and_coding_type() {
    assert_eq!(
        run(r#"$fp = fopen("php://memory", "w+");
            $f = stream_filter_append($fp, "zlib.deflate", STREAM_FILTER_WRITE);
            fwrite($fp, "filtered"); stream_filter_remove($f); rewind($fp);
            echo gzinflate(stream_get_contents($fp)), "\n";
            var_dump(zlib_get_coding_type());
            foreach (stream_get_filters() as $n) { if (str_contains($n, "*") && $n[0] != "s" && $n[0] != "c") echo $n, "\n"; }"#),
        "filtered\nbool(false)\nzlib.*\nbzip2.*\n"
    );
}

#[test]
fn output_alias_is_internal_and_lookup_is_case_sensitive() {
    assert_eq!(
        run(r#"ob_start("ob_gzhandler");
            $s = ob_get_status();
            ob_end_clean();
            var_dump($s["name"], $s["type"], $s["flags"], $s["chunk_size"]);
            ob_start("OB_GZHANDLER");
            $s = ob_get_status();
            ob_end_clean();
            var_dump($s["name"], $s["type"], $s["flags"]);"#),
        "string(12) \"ob_gzhandler\"\nint(0)\nint(112)\nint(0)\n\
         string(12) \"OB_GZHANDLER\"\nint(1)\nint(113)\n"
    );
}

#[test]
fn output_alias_conflicts_and_does_not_add_a_buffer() {
    assert_eq!(
        run(r#"set_error_handler(function($n, $m) { echo "$n:$m\n"; });
            ob_start("ob_gzhandler");
            var_dump(ob_start("ob_gzhandler"), ob_get_level());
            ob_end_flush();"#),
        "2:ob_start(): Output handler 'ob_gzhandler' cannot be used twice\n\
         8:ob_start(): Failed to create buffer\nbool(false)\nint(1)\n"
    );
    assert_eq!(
        run(r#"ob_start(function($s) { return $s; });
            var_dump(ob_start("ob_gzhandler"), ob_get_level());
            ob_end_flush();
            ob_end_flush();"#),
        "bool(true)\nint(2)\n"
    );
}

#[test]
fn output_conflict_exception_keeps_the_original_handler() {
    assert_eq!(
        run(r#"ob_start("ob_gzhandler");
            $calls = [];
            set_error_handler(function($n, $m) use (&$calls) { $calls[] = $n; throw new Exception($m); });
            try { ob_start("ob_gzhandler"); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
            var_dump(ob_get_level(), $calls);
            ob_end_flush();"#),
        "ob_start(): Output handler 'ob_gzhandler' cannot be used twice\nint(1)\n\
         array(1) {\n  [0]=>\n  int(2)\n}\n"
    );
}

#[test]
// php-src's php_zlib_output_encoding/output_handler; the PHP 8.3 CLI oracle differs.
fn output_compression_negotiates_server_values_and_makes_the_handler_immutable() {
    let sink = Sink::default();
    let zlib = php_ext_zlib::Zlib;
    let mut vm = Vm::with_extensions(
        &VmOptions::default(),
        Box::new(sink.clone()),
        &[&php_ext_stdstr::StdStr, &zlib],
    );
    vm.run_script(
        br#"<?php
        $_SERVER['HTTP_ACCEPT_ENCODING'] = 'deflate,gzip';
        ob_start('ob_gzhandler', 1);
        echo 'x';
        $s = ob_get_status();
        echo $s['type'], ':', $s['flags'] & PHP_OUTPUT_HANDLER_STDFLAGS, ':';
        echo zlib_get_coding_type(), ':';
        foreach (headers_list() as $h) { echo 'unexpected'; }
        echo 'cli';"#,
        "test.php",
    );
    drop(vm);
    let bytes = sink.0.borrow().clone();
    assert_eq!(
        php_zlib::codec::gzdecode(&bytes, 0).expect("gzip"),
        b"x0:32:gzip:cli"
    );
}

#[test]
fn same_value_compression_ini_writes_run_the_modify_handler() {
    let sink = Sink::default();
    let zlib = php_ext_zlib::Zlib;
    let mut vm = Vm::with_extensions(
        &VmOptions::default(),
        Box::new(sink.clone()),
        &[&php_ext_stdstr::StdStr, &zlib],
    );
    vm.run_script(
        br#"<?php
        ini_set('zlib.output_compression', '1');
        $_SERVER['HTTP_ACCEPT_ENCODING'] = 'gzip';
        ini_set('zlib.output_compression', '1');
        echo implode(',', ob_list_handlers());"#,
        "test.php",
    );
    drop(vm);
    let bytes = sink.0.borrow().clone();
    assert_eq!(
        php_zlib::codec::gzdecode(&bytes, 0).expect("gzip"),
        b"zlib output compression"
    );
}

#[test]
fn zlib_ini_changes_are_rejected_after_output_without_changing_values() {
    assert_eq!(
        run(r#"ini_set('zlib.output_compression', '0');
            echo 'x';
            set_error_handler(function($n, $m) { echo "$n:$m\n"; });
            var_dump(ini_set('zlib.output_compression', '0'), ini_get('zlib.output_compression'));
            var_dump(ini_set('zlib.output_handler', 'foo'), ini_get('zlib.output_handler'));"#),
        "x2:ini_set(): Cannot change zlib.output_compression - headers already sent\n\
         bool(false)\nstring(1) \"0\"\n\
         2:ini_set(): Cannot change zlib.output_handler - headers already sent\n\
         bool(false)\nstring(0) \"\"\n"
    );
}

#[test]
fn content_length_disables_the_active_compression_handler() {
    assert_eq!(
        run(r#"$_SERVER['HTTP_ACCEPT_ENCODING'] = 'gzip';
            ob_start('ob_gzhandler');
            header('Content-Length: 1');
            echo 'x';"#),
        "x"
    );
}

#[test]
fn memory_limit_model() {
    let out =
        run(r#"ini_set("memory_limit", "4M"); gzdecode(gzencode(str_repeat("a", 8 << 20)));"#);
    assert!(
        out.contains("Fatal error: Allowed memory size of 4194304 bytes exhausted"),
        "{out}"
    );
}
