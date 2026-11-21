//! Environment, process-information and `phpinfo()` primitive tests
//! (expectations from `/usr/bin/php` and the `general_functions` phpt files).

mod common;

use common::export;
use php_runtime::{CollectingContext, Value, ZArray};
use php_stdmisc::info::{
    html_head, print_info, InfoPrinter, InfoSource, INFO_ENVIRONMENT, INFO_LICENSE, INFO_VARIABLES,
};
use php_stdmisc::system::{self, EnvState};

#[test]
fn phpinfo_text_tables() {
    // env -i A=1 'B=x=y' C= php -n -r 'phpinfo(INFO_ENVIRONMENT);'
    let mut p = InfoPrinter::new(true);
    p.print("phpinfo()\n");
    p.section("Environment");
    p.table_start();
    p.table_header(&[b"Variable", b"Value"]);
    p.table_row(&[b"A", b"1"]);
    p.table_row(&[b"B", b"x=y"]);
    p.table_row(&[b"C", b""]);
    p.table_end();
    assert_eq!(
        String::from_utf8(p.out).unwrap(),
        "phpinfo()\n\nEnvironment\n\nVariable => Value\nA => 1\nB => x=y\nC =>  \n"
    );
    let mut p = InfoPrinter::new(false);
    p.table_row(&[b"a<b", b""]);
    p.stream_hash("PHP Streams", Some(&[b"https".to_vec(), b"ftp".to_vec()]));
    assert_eq!(
        String::from_utf8(p.out).unwrap(),
        "<tr><td class=\"e\">a&lt;b </td><td class=\"v\"><i>no value</i> </td></tr>\n<tr><td class=\"e\">Registered PHP Streams</td><td class=\"v\">https, ftp</td></tr>\n"
    );
}

#[test]
fn phpinfo_variables_section() {
    // php -n -r '$_GET["a"]=1; $_GET[5]=[1]; phpinfo(INFO_VARIABLES);' (with
    // only $_GET shown)
    let mut ctx = CollectingContext::new();
    let get = ZArray::from_pairs([("a", Value::Int(1))]);
    let mut get = get;
    get.make_mut()
        .insert(5, Value::Array(ZArray::from_values([Value::Int(1)])));
    let src = InfoSource {
        php_version: "8.5.0-dev",
        uname: b"",
        build_date: "",
        build_rows: &[],
        server_api: None,
        config_file_path: "",
        loaded_ini: None,
        scan_dir: None,
        scanned_files: None,
        engine_rows: &[],
        stream_wrappers: &[],
        stream_transports: &[],
        stream_filters: &[],
        zend_version: "",
        core_ini: &[],
        modules: &[],
        php_vars: &[],
        gpcse: &[("_GET", get)],
    };
    let out = print_info(INFO_VARIABLES, &src, true, &mut ctx).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "phpinfo()\n\nPHP Variables\n\nVariable => Value\n$_GET['a'] => 1\n$_GET['5'] => Array\n(\n    [0] => 1\n)\n\n"
    );
    let env_only = print_info(INFO_ENVIRONMENT, &src, true, &mut ctx).unwrap();
    assert!(String::from_utf8_lossy(&env_only)
        .starts_with("phpinfo()\n\nEnvironment\n\nVariable => Value\n"));
}

#[test]
fn phpinfo_html_page() {
    // php-cgi -n -q: <?php phpinfo(INFO_LICENSE); (license text of php-src master)
    let mut ctx = CollectingContext::new();
    let src = InfoSource {
        php_version: "8.5.0-dev",
        uname: b"",
        build_date: "",
        build_rows: &[],
        server_api: Some("CGI/FastCGI"),
        config_file_path: "",
        loaded_ini: None,
        scan_dir: None,
        scanned_files: None,
        engine_rows: &[],
        stream_wrappers: &[],
        stream_transports: &[],
        stream_filters: &[],
        zend_version: "",
        core_ini: &[],
        modules: &[],
        php_vars: &[],
        gpcse: &[],
    };
    let out = String::from_utf8(print_info(INFO_LICENSE, &src, false, &mut ctx).unwrap()).unwrap();
    let head = String::from_utf8(html_head("8.5.0-dev", "phpinfo()")).unwrap();
    assert!(head.starts_with(
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \"DTD/xhtml1-transitional.dtd\">\n<html xmlns=\"http://www.w3.org/1999/xhtml\"><head>\n<style type=\"text/css\">\nbody {background-color: #fff; color: #222; font-family: sans-serif;}\n"
    ));
    assert!(head.ends_with(
        "  hr {background-color: #505153}\n}\n</style>\n<title>PHP 8.5.0-dev - phpinfo()</title><meta name=\"ROBOTS\" content=\"NOINDEX,NOFOLLOW,NOARCHIVE\" /></head>\n<body><div class=\"center\">\n"
    ));
    let body = out.strip_prefix(head.as_str()).unwrap();
    assert!(body.starts_with(
        "<h2>License</h2>\n<table>\n<tr class=\"v\"><td>\n<p>\nPHP is free software."
    ));
    assert!(body.ends_with(
        "POSSIBILITY OF SUCH DAMAGE.\n</p>\n</td></tr>\n</table>\n</div></body></html>"
    ));
}

#[test]
fn env_put_get_restore() {
    let mut st = EnvState::default();
    let name = b"PHPRUST_STDMISC_TEST_VAR";
    assert!(matches!(
        system::getenv(Some(name), false, None).unwrap(),
        Value::Bool(false)
    ));
    assert!(st.putenv(b"PHPRUST_STDMISC_TEST_VAR=a=b").unwrap());
    assert_eq!(
        export(&system::getenv(Some(name), false, None).unwrap()),
        "'a=b'"
    );
    assert_eq!(
        export(&system::getenv(Some(name), false, Some(b"sapi")).unwrap()),
        "'sapi'"
    );
    assert_eq!(
        export(&system::getenv(Some(name), true, Some(b"sapi")).unwrap()),
        "'a=b'"
    );
    let all = system::getenv_all();
    assert!(all.get_str(name).is_some());
    // "NAME" without '=' unsets.
    assert!(st.putenv(name).unwrap());
    assert!(matches!(
        system::getenv(Some(name), false, None).unwrap(),
        Value::Bool(false)
    ));
    assert!(st.putenv(b"PHPRUST_STDMISC_TEST_VAR=x").unwrap());
    st.request_shutdown();
    assert!(matches!(
        system::getenv(Some(name), false, None).unwrap(),
        Value::Bool(false)
    ));
    // putenv.phpt / putenv_and_getenv_reject_null_bytes.phpt
    let e = st.putenv(b"").unwrap_err();
    assert_eq!(
        e.message(),
        Some("putenv(): Argument #1 ($assignment) must have a valid syntax")
    );
    let e = st.putenv(b"=123").unwrap_err();
    assert_eq!(
        e.message(),
        Some("putenv(): Argument #1 ($assignment) must have a valid syntax")
    );
    let e = st.putenv(b"a\0b=c").unwrap_err();
    assert_eq!(
        e.message(),
        Some("putenv(): Argument #1 ($assignment) must not contain any null bytes")
    );
    let e = system::getenv(Some(b"a\0"), false, None).unwrap_err();
    assert_eq!(
        e.message(),
        Some("getenv(): Argument #1 ($name) must not contain any null bytes")
    );
}

#[test]
fn uname_and_process_info() {
    let a = system::php_uname(b"a").unwrap();
    let s = system::php_uname(b"s").unwrap();
    assert!(a.as_bytes().starts_with(s.as_bytes()));
    assert_eq!(s.as_bytes(), b"Linux");
    // php_uname_error.phpt
    assert_eq!(
        system::php_uname(b"").unwrap_err().message(),
        Some("php_uname(): Argument #1 ($mode) must be a single character")
    );
    assert_eq!(
        system::php_uname(b"z").unwrap_err().message(),
        Some("php_uname(): Argument #1 ($mode) must be one of \"a\", \"m\", \"n\", \"r\", \"s\", or \"v\"")
    );
    assert!(system::getmypid() > 0);
    let ru = export(&system::getrusage(0));
    assert!(
        ru.starts_with("array (\n  'ru_oublock' =>") && ru.contains("'ru_stime.tv_sec' =>"),
        "{ru}"
    );
    let la = system::sys_getloadavg();
    assert_eq!(la.as_array().map(|a| a.len()), Some(3));
    let page = system::PageInfo::for_script(None);
    assert!(matches!(page.getmyinode(), Value::Bool(false)));
    assert!(matches!(page.getmyuid(), Value::Int(_)));
    let page = system::PageInfo::for_script(Some(env!("CARGO_MANIFEST_DIR").as_bytes()));
    assert!(matches!(page.getlastmod(), Value::Int(_)));
}

#[test]
fn time_functions() {
    let m = export(&system::microtime(false));
    // "0.12345600 1700000000"
    let inner = m.trim_matches('\'');
    let (frac, sec) = inner.split_once(' ').unwrap();
    assert!(frac.starts_with("0.") && frac.len() == 10, "{m}");
    assert!(sec.parse::<i64>().unwrap() > 1_600_000_000);
    assert_eq!(
        export(&system::microtime_from(5, 250, false)),
        "'0.00025000 5'"
    );
    assert!(matches!(system::microtime(true), Value::Float(_)));
    let h1 = system::hrtime(true).as_int().unwrap();
    let h2 = system::hrtime(true).as_int().unwrap();
    assert!(h2 >= h1);
    assert_eq!(system::hrtime(false).as_array().unwrap().len(), 2);
    let tod = export(&system::gettimeofday(false, |_| (3600, true)));
    assert!(
        tod.contains("'minuteswest' => -60") && tod.contains("'dsttime' => 1"),
        "{tod}"
    );

    // sleep_error.phpt, usleep_error.phpt, time_nanosleep errors
    assert_eq!(
        system::sleep(-1).unwrap_err().message(),
        Some("sleep(): Argument #1 ($seconds) must be between 0 and 4294967295")
    );
    assert_eq!(system::sleep(0).unwrap(), 0);
    assert_eq!(
        system::usleep(-1).unwrap_err().message(),
        Some("usleep(): Argument #1 ($microseconds) must be between 0 and 4294967295")
    );
    system::usleep(10).unwrap();
    assert_eq!(
        system::time_nanosleep(-1, 0).unwrap_err().message(),
        Some("time_nanosleep(): Argument #1 ($seconds) must be greater than or equal to 0")
    );
    assert_eq!(
        system::time_nanosleep(0, 1_000_000_000)
            .unwrap_err()
            .message(),
        Some("time_nanosleep(): Argument #2 ($nanoseconds) must be between 0 and 999999999")
    );
    assert!(matches!(
        system::time_nanosleep(0, 1000).unwrap(),
        Value::Bool(true)
    ));
    let mut ctx = CollectingContext::new();
    assert_eq!(
        system::time_sleep_until(-1.0, &mut ctx)
            .unwrap_err()
            .message(),
        Some("time_sleep_until(): Argument #1 ($timestamp) must be between 0 and 18446744073")
    );
    assert!(!system::time_sleep_until(1.0, &mut ctx).unwrap());
    assert_eq!(
        ctx.messages(),
        ["Warning: time_sleep_until(): Argument #1 ($timestamp) must be greater than or equal to the current time"]
    );
    #[allow(clippy::cast_precision_loss)]
    let soon = system::microtime(true).as_float().unwrap() + 0.01;
    assert!(system::time_sleep_until(soon, &mut ctx).unwrap());
}

#[test]
fn ftok_and_connection() {
    let mut ctx = CollectingContext::new();
    assert_eq!(
        system::ftok(b"", b"a", &mut ctx).unwrap_err().message(),
        Some("ftok(): Argument #1 ($filename) must not be empty")
    );
    assert_eq!(
        system::ftok(b"/", b"ab", &mut ctx).unwrap_err().message(),
        Some("ftok(): Argument #2 ($project_id) must be a single character")
    );
    assert!(system::ftok(b"/", b"a", &mut ctx).unwrap() != -1);
    assert_eq!(system::ftok(b"/nonexistent/x", b"a", &mut ctx).unwrap(), -1);
    assert_eq!(
        ctx.messages(),
        ["Warning: ftok(): ftok() failed - No such file or directory"]
    );
}
