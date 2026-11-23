// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Connection-level features: multi statements, charsets and escaping,
//! `change_user`, transactions, LOAD DATA LOCAL INFILE, init commands,
//! compression, async queries and statistics — compared with `/usr/bin/php`.
#![expect(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::doc_markdown,
    clippy::format_collect,
    clippy::items_after_statements
)]

mod common;

use common::*;
use php_mysqlnd::consts::*;
use php_mysqlnd::stats::Stat;
use php_mysqlnd::{CloseType, Connection, OptionValue};

#[test]
fn multi_statements() {
    let Some(c) = connect_flags(CLIENT_MULTI_STATEMENTS) else {
        return;
    };
    let t = table("multi");
    let sql = format!(
        "DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (a INT); INSERT INTO {t} VALUES (1),(2); SELECT * FROM {t}; UPDATE {t} SET a = a + 1; SELECT a * 10 AS x FROM {t}; SELEC broken; SELECT 'never'"
    );
    let mut out = Vec::new();
    let mut ok = c.query(sql.as_bytes()).is_ok();
    loop {
        if !ok {
            out.push(format!("error:{}|{}", c.errno(), c.sqlstate()));
            break;
        }
        match c.store_result().unwrap() {
            Some(mut r) => {
                out.push(format!("rows:{}", r.num_rows()));
                while let Some(row) = r.fetch_row().unwrap() {
                    out.push(ra(&row));
                }
            }
            None => out.push(format!(
                "aff:{}|info:{}",
                c.affected_rows() as i64,
                c.info().map_or(String::new(), |i| format!(
                    "Some([{}])",
                    i.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
                ))
            )),
        }
        if !c.more_results() {
            out.push("done".into());
            break;
        }
        ok = c.next_result().unwrap_or(false);
    }
    // the connection is usable again after the error
    c.query(b"SELECT 1").unwrap();
    let _ = c.store_result().unwrap();
    c.query(format!("DROP TABLE {t}").as_bytes()).unwrap();
    // without CLIENT_MULTI_STATEMENTS it is a syntax error; toggling the server option enables it
    let Some(c2) = connect() else { return };
    assert_eq!(c2.query(b"SELECT 1; SELECT 2").unwrap_err().errno, 1064);
    c2.set_server_option(OPTION_MULTI_STATEMENTS_ON).unwrap();
    c2.query(b"SELECT 1; SELECT 2").unwrap();
    let _ = c2.store_result().unwrap();
    assert!(c2.more_results());
    assert!(c2.next_result().unwrap());
    let _ = c2.store_result().unwrap();
    assert!(!c2.more_results());
    assert!(!c2.next_result().unwrap());
    let mine = out.join("\n");
    if !have_php() {
        return;
    }
    let code = format!(
        r#"$m = conn(65536); $out = []; $ok = $m->multi_query({sql:?});
        while (true) {{
            if (!$ok) {{ $out[] = "error:".$m->errno."|".$m->sqlstate; break; }}
            if ($r = $m->store_result()) {{ $out[] = "rows:".$r->num_rows; while ($row = $r->fetch_row()) $out[] = r($row); }}
            else $out[] = "aff:".$m->affected_rows."|info:".($m->info === null ? "" : "Some(".json_encode(array_map('ord', str_split($m->info))).")");
            if (!$m->more_results()) {{ $out[] = "done"; break; }}
            $ok = $m->next_result();
        }}
        echo implode("\n", $out);"#
    );
    assert_lines_eq(&mine, &php(&code));
}

#[test]
fn charsets_and_escaping() {
    let Some(c) = connect() else { return };
    let inputs: [&[u8]; 6] = [
        b"plain",
        b"it's \"q\" \\ \0 \n \r \x1a",
        "日本'語".as_bytes(),
        b"\xbf\x27 or 1=1",
        b"\xbf\x5c\x27",
        b"\x81\x27",
    ];
    let mut out = Vec::new();
    for cs in ["utf8mb4", "latin1", "gbk", "sjis", "big5", "utf8", "binary"] {
        c.set_charset(cs).unwrap();
        out.push(format!(
            "{}|{}",
            c.character_set_name().unwrap(),
            c.charset().unwrap().collation
        ));
        for i in inputs {
            out.push(h(&c.escape_string(i)));
        }
    }
    let e = c.set_charset("nosuchcs").unwrap_err();
    out.push(format!(
        "{}|{}",
        e.errno,
        String::from_utf8_lossy(&e.message)
    ));
    // NO_BACKSLASH_ESCAPES switches to quote doubling
    c.set_charset("utf8mb4").unwrap();
    c.query(b"SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'")
        .unwrap();
    out.push(h(&c.escape_string(b"it's \\ ok")));
    let mine = out.join("\n");
    if !have_php() {
        return;
    }
    let theirs = php(r#"$m = conn(); $out = [];
        $inputs = ["plain", "it's \"q\" \\ \0 \n \r \x1a", "日本'語", "\xbf\x27 or 1=1", "\xbf\x5c\x27", "\x81\x27"];
        foreach (["utf8mb4", "latin1", "gbk", "sjis", "big5", "utf8", "binary"] as $cs) {
            $m->set_charset($cs); $out[] = $m->character_set_name()."|".$m->get_charset()->collation;
            foreach ($inputs as $i) $out[] = bin2hex($m->real_escape_string($i));
        }
        try { $m->set_charset("nosuchcs"); } catch (Throwable $e) {}
        $out[] = $m->errno."|".$m->error;
        $m->set_charset("utf8mb4"); $m->query("SET SESSION sql_mode = 'NO_BACKSLASH_ESCAPES'");
        $out[] = bin2hex($m->real_escape_string("it's \\ ok"));
        echo implode("\n", $out);"#);
    assert_lines_eq(&mine, &theirs);
}

#[test]
fn charset_option_at_connect() {
    let p = params();
    let Some(_alive) = connect() else { return };
    let c = Connection::init();
    assert!(c.set_option(opt::MYSQL_SET_CHARSET_NAME, OptionValue::Str(b"latin1")));
    c.connect(
        Some(&p.host),
        Some(p.user.as_bytes()),
        Some(p.passwd.as_bytes()),
        Some(p.db.as_bytes()),
        p.port,
        None,
        0,
    )
    .unwrap();
    assert_eq!(c.character_set_name(), Some("latin1"));
    let mut r = query(&c, "SELECT @@character_set_client, @@character_set_results").unwrap();
    let row = r.fetch_row().unwrap().unwrap();
    assert_eq!(
        ra(&row),
        format!("[i:0=>s:{},i:1=>s:{}]", h(b"latin1"), h(b"latin1"))
    );
}

#[test]
fn change_user_and_select_db() {
    let Some(c) = connect() else { return };
    let p = params();
    c.query(b"SET @v = 1").unwrap();
    c.change_user(
        p.user.as_bytes(),
        p.passwd.as_bytes(),
        Some(p.db.as_bytes()),
        false,
    )
    .unwrap();
    let mut r = query(&c, "SELECT @v, DATABASE(), CURRENT_USER() LIKE 'phptest%'").unwrap();
    assert_eq!(
        ra(&r.fetch_row().unwrap().unwrap()),
        format!("[i:0=>N,i:1=>s:{},i:2=>s:31]", h(p.db.as_bytes()))
    );
    drop(r);
    let e = c
        .change_user(p.user.as_bytes(), b"wrong", None, false)
        .unwrap_err();
    assert_eq!(e.errno, 1045);
    assert_eq!(e.sqlstate, "28000");
    let mine = format!(
        "{}|{}|{}",
        e.errno,
        e.sqlstate,
        String::from_utf8_lossy(&e.message)
    );
    if have_php() {
        let theirs = php(&format!(
            r#"$m = conn(); @$m->change_user("{}", "wrong", null); echo $m->errno, "|", $m->sqlstate, "|", $m->error;"#,
            p.user
        ));
        assert_eq!(mine, theirs);
    }
}

#[test]
fn transactions() {
    let Some(c) = connect() else { return };
    let t = table("tx");
    c.query(format!("DROP TABLE IF EXISTS {t}").as_bytes())
        .unwrap();
    c.query(format!("CREATE TABLE {t} (a INT) ENGINE=InnoDB").as_bytes())
        .unwrap();
    c.autocommit(false).unwrap();
    assert_eq!(c.server_status() & SERVER_STATUS_AUTOCOMMIT, 0);
    c.begin_transaction(TRANS_START_READ_WRITE, Some(b"my_tx"))
        .unwrap();
    c.query(format!("INSERT INTO {t} VALUES (1)").as_bytes())
        .unwrap();
    c.savepoint(Some(b"sp1")).unwrap();
    c.query(format!("INSERT INTO {t} VALUES (2)").as_bytes())
        .unwrap();
    c.query(b"ROLLBACK TO SAVEPOINT `sp1`").unwrap();
    c.release_savepoint(Some(b"sp1")).unwrap();
    c.commit(TRANS_COR_NO_OPT, Some(b"bad*name")).unwrap();
    c.begin_transaction(
        TRANS_START_WITH_CONSISTENT_SNAPSHOT | TRANS_START_READ_ONLY,
        None,
    )
    .unwrap();
    c.rollback(TRANS_COR_AND_NO_CHAIN, None).unwrap();
    assert!(c.savepoint(None).is_err());
    c.autocommit(true).unwrap();
    assert_ne!(c.server_status() & SERVER_STATUS_AUTOCOMMIT, 0);
    let mut r = query(&c, &format!("SELECT * FROM {t}")).unwrap();
    assert_eq!(r.num_rows(), 1);
    let _ = r.fetch_row();
    c.query(format!("DROP TABLE {t}").as_bytes()).unwrap();
}

#[test]
fn load_data_local_infile() {
    let Some(c) = connect() else { return };
    let t = table("infile");
    let dir = std::env::temp_dir().join(format!("mysqlnd_rs_infile_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("data.csv");
    std::fs::write(&file, "1,one\n2,two\n3,three\n").unwrap();
    let f = file.to_string_lossy().into_owned();
    let d = dir.to_string_lossy().into_owned();
    c.query(format!("DROP TABLE IF EXISTS {t}").as_bytes())
        .unwrap();
    c.query(format!("CREATE TABLE {t} (id INT, v VARCHAR(10))").as_bytes())
        .unwrap();
    let load = |c: &Connection, path: &str| {
        let ok = c
            .query(
                format!("LOAD DATA LOCAL INFILE '{path}' INTO TABLE {t} FIELDS TERMINATED BY ','")
                    .as_bytes(),
            )
            .is_ok();
        format!(
            "{}|{}|{}|{}",
            u8::from(ok),
            c.errno(),
            String::from_utf8_lossy(&c.error()),
            c.affected_rows() as i64
        )
    };
    let mut out = Vec::new();
    out.push(load(&c, &f)); // forbidden by default
    c.set_option(opt::MYSQL_OPT_LOCAL_INFILE, OptionValue::Int(1));
    out.push(load(&c, &f));
    out.push(format!(
        "info:{}",
        String::from_utf8_lossy(&c.info().unwrap_or_default())
    ));
    out.push(load(&c, "/nonexistent/x.csv"));
    c.set_option(opt::MYSQL_OPT_LOCAL_INFILE, OptionValue::Int(0));
    c.set_option(
        opt::MYSQL_OPT_LOAD_DATA_LOCAL_DIR,
        OptionValue::Str(d.as_bytes()),
    );
    out.push(load(&c, &f));
    out.push(load(&c, "/etc/hostname"));
    c.set_option(
        opt::MYSQL_OPT_LOAD_DATA_LOCAL_DIR,
        OptionValue::Str(b"/nonexistent-dir"),
    );
    out.push(load(&c, &f));
    let mut r = query(&c, &format!("SELECT COUNT(*) FROM {t}")).unwrap();
    out.push(ra(&r.fetch_row().unwrap().unwrap()));
    c.query(format!("DROP TABLE {t}").as_bytes()).unwrap();
    let mine = out.join("\n");
    if have_php() {
        let code = format!(
            r#"$m = conn(); $t = "{t}"; $f = "{f}"; $d = "{d}"; $out = [];
            $m->query("DROP TABLE IF EXISTS $t"); $m->query("CREATE TABLE $t (id INT, v VARCHAR(10))");
            $load = function($m, $p) use ($t) {{ $ok = $m->query("LOAD DATA LOCAL INFILE '$p' INTO TABLE $t FIELDS TERMINATED BY ','"); return ($ok?1:0)."|".$m->errno."|".$m->error."|".$m->affected_rows; }};
            $out[] = $load($m, $f);
            $m->options(MYSQLI_OPT_LOCAL_INFILE, 1); $out[] = $load($m, $f); $out[] = "info:".$m->info;
            $out[] = $load($m, "/nonexistent/x.csv");
            $m->options(MYSQLI_OPT_LOCAL_INFILE, 0); $m->options(MYSQLI_OPT_LOAD_DATA_LOCAL_DIR, $d);
            $out[] = $load($m, $f); $out[] = $load($m, "/etc/hostname");
            $m->options(MYSQLI_OPT_LOAD_DATA_LOCAL_DIR, "/nonexistent-dir"); $out[] = @$load($m, $f);
            $out[] = r($m->query("SELECT COUNT(*) FROM $t")->fetch_row()); $m->query("DROP TABLE $t");
            echo implode("\n", $out);"#
        );
        // php-src master renamed the PDO constants in this message (8.3 is the oracle)
        let theirs = php(&code).replace(
            "PDO::MYSQL_ATTR_LOCAL_INFILE|PDO::MYSQL_ATTR_LOCAL_INFILE_DIRECTORY",
            "Pdo\\Mysql::ATTR_LOCAL_INFILE|Pdo\\Mysql::ATTR_LOCAL_INFILE_DIRECTORY",
        );
        assert_lines_eq(&mine, &theirs);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn init_commands_and_options() {
    let p = params();
    let Some(_alive) = connect() else { return };
    let c = Connection::init();
    c.set_option(opt::MYSQL_INIT_COMMAND, OptionValue::Str(b"SET @a = 41"));
    c.set_option(opt::MYSQL_INIT_COMMAND, OptionValue::Str(b"SELECT 1"));
    c.set_option(
        opt::MYSQL_INIT_COMMAND,
        OptionValue::Str(b"SET @a = @a + 1"),
    );
    c.connect(
        Some(&p.host),
        Some(p.user.as_bytes()),
        Some(p.passwd.as_bytes()),
        Some(p.db.as_bytes()),
        p.port,
        None,
        0,
    )
    .unwrap();
    let mut r = query(&c, "SELECT @a").unwrap();
    assert_eq!(ra(&r.fetch_row().unwrap().unwrap()), "[i:0=>s:3432]");
    drop(r);
    let s = c.stats();
    assert_eq!(s.get(Stat::InitCommandExecutedCount), 3);
    // a failing init command fails the connect
    let c2 = Connection::init();
    c2.set_option(opt::MYSQL_INIT_COMMAND, OptionValue::Str(b"SELEC 1"));
    let e = c2
        .connect(
            Some(&p.host),
            Some(p.user.as_bytes()),
            Some(p.passwd.as_bytes()),
            Some(p.db.as_bytes()),
            p.port,
            None,
            0,
        )
        .unwrap_err();
    assert_eq!(e.errno, 1064);
    assert_eq!(c2.stats().get(Stat::InitCommandFailedCount), 1);
}

#[test]
fn compressed_protocol() {
    let Some(c) = connect_flags(CLIENT_COMPRESS) else {
        return;
    };
    assert_ne!(c.client_flags() & CLIENT_COMPRESS, 0);
    let mut r = query(
        &c,
        "SELECT REPEAT('abc', 100000) AS big, 42 AS n UNION ALL SELECT REPEAT('x', 10), 7",
    )
    .unwrap();
    let row = r.fetch_row().unwrap().unwrap();
    let php_runtime::Value::Str(big) = row.get_int(0).unwrap() else {
        panic!()
    };
    assert_eq!(big.len(), 300_000);
    assert!(big.as_bytes().starts_with(b"abcabc"));
    assert!(r.fetch_row().unwrap().is_some());
    // big statement sent through the compressed envelope
    let q = format!("SELECT LENGTH('{}')", "z".repeat(50_000));
    let mut r = query(&c, &q).unwrap();
    assert_eq!(
        ra(&r.fetch_row().unwrap().unwrap()),
        format!("[i:0=>s:{}]", h(b"50000"))
    );
    let st = c.stmt_init();
    st.prepare(b"SELECT REPEAT('q', 70000)").unwrap();
    struct Ctx;
    impl php_runtime::error::RuntimeContext for Ctx {
        fn diagnostic(
            &mut self,
            _: php_runtime::error::ErrorLevel,
            _: &str,
        ) -> php_runtime::error::RtResult<()> {
            Ok(())
        }
    }
    st.execute(&mut Ctx).unwrap();
    let row = st.fetch().unwrap().unwrap();
    assert!(matches!(&row[0], php_runtime::Value::Str(s) if s.len() == 70_000));
}

#[test]
fn async_send_and_reap() {
    let Some(c) = connect() else { return };
    c.send_query(b"SELECT SLEEP(0.1), 5").unwrap();
    assert_eq!(c.state(), php_mysqlnd::ConnState::QuerySent);
    assert_eq!(
        c.query(b"SELECT 1").unwrap_err().errno,
        CR_COMMANDS_OUT_OF_SYNC
    );
    c.reap_query().unwrap();
    let mut r = c.store_result().unwrap().unwrap();
    assert_eq!(
        ra(&r.fetch_row().unwrap().unwrap()),
        format!("[i:0=>s:{},i:1=>s:{}]", h(b"0"), h(b"5"))
    );
}

#[test]
fn big_packets_over_16mb() {
    let Some(c) = connect() else { return };
    let mut r = query(&c, "SELECT @@max_allowed_packet").unwrap();
    let row = r.fetch_row().unwrap().unwrap();
    let php_runtime::Value::Str(m) = row.get_int(0).unwrap() else {
        panic!()
    };
    let max: usize = m.to_string_lossy().parse().unwrap();
    if max < 40 * 1024 * 1024 {
        // the server can't return >16 MiB rows; still exercise a 16 MiB-1 boundary statement
        let q = format!("SELECT LENGTH('{}')", "a".repeat(1 << 20));
        assert!(query(&c, &q).is_some());
        return;
    }
    let mut r = query(&c, "SELECT REPEAT('x', 17000000)").unwrap();
    let row = r.fetch_row().unwrap().unwrap();
    assert!(matches!(row.get_int(0).unwrap(), php_runtime::Value::Str(s) if s.len() == 17_000_000));
}

#[test]
fn statistics_match_oracle() {
    // A fixed sequence of operations; every counter except the byte/memory
    // ones (connect attributes differ between clients) must match mysqlnd.
    let Some(c) = connect() else { return };
    let t = table("stats");
    let ops = |c: &Connection| {
        c.query(format!("DROP TABLE IF EXISTS {t}").as_bytes())
            .unwrap();
        c.query(format!("CREATE TABLE {t} (a INT, b VARCHAR(5), d DOUBLE)").as_bytes())
            .unwrap();
        c.query(format!("INSERT INTO {t} VALUES (1,'x',1.5),(2,NULL,2.5),(3,'z',NULL)").as_bytes())
            .unwrap();
        let mut r = query(c, &format!("SELECT * FROM {t}")).unwrap();
        while r.fetch_row().unwrap().is_some() {}
        r.free(false);
        c.query(format!("SELECT a FROM {t}").as_bytes()).unwrap();
        let mut r = c.use_result().unwrap().unwrap();
        let _ = r.fetch_row().unwrap();
        r.free(false);
        let _ = c.query(b"SELEC 1");
        c.ping().unwrap();
        c.query(format!("DROP TABLE {t}").as_bytes()).unwrap();
    };
    let before = c.stats();
    ops(&c);
    let after = c.stats();
    let skip = |n: &str| {
        n.starts_with("bytes_")
            || n.starts_with("mem_")
            || n.contains("protocol_overhead")
            || n == "packets_sent"
            || n == "packets_received"
            || n == "connect_success"
            || n == "active_connections"
    };
    let mut mine = Vec::new();
    for ((n, a), (_, b)) in after.to_pairs().into_iter().zip(before.to_pairs()) {
        if !skip(n) {
            let d = a.parse::<i128>().unwrap() - b.parse::<i128>().unwrap();
            mine.push(format!("{n}={d}"));
        }
    }
    let mine = mine.join("\n");
    c.close(CloseType::Explicit);
    if !have_php() {
        return;
    }
    let code = format!(
        r#"$m = conn(); $t = "{t}"; $before = mysqli_get_connection_stats($m);
        $m->query("DROP TABLE IF EXISTS $t"); $m->query("CREATE TABLE $t (a INT, b VARCHAR(5), d DOUBLE)");
        $m->query("INSERT INTO $t VALUES (1,'x',1.5),(2,NULL,2.5),(3,'z',NULL)");
        $r = $m->query("SELECT * FROM $t"); while ($r->fetch_row()); $r->free();
        $r = $m->query("SELECT a FROM $t", MYSQLI_USE_RESULT); $r->fetch_row(); $r->free();
        $m->query("SELEC 1"); $m->ping(); $m->query("DROP TABLE $t");
        $after = mysqli_get_connection_stats($m); $out = [];
        foreach ($after as $n => $v) {{
            if (str_starts_with($n, "bytes_") || str_starts_with($n, "mem_") || str_contains($n, "protocol_overhead") || $n == "packets_sent" || $n == "packets_received" || $n == "connect_success" || $n == "active_connections") continue;
            $out[] = "$n=".($v - $before[$n]);
        }}
        echo implode("\n", $out);"#
    );
    assert_lines_eq(&mine, &php(&code));
}

#[test]
fn global_close_statistics() {
    let Some(_alive) = connect() else { return };
    php_mysqlnd::ini::reset();
    let before = php_mysqlnd::stats::global();
    let c = connect().unwrap();
    c.close(CloseType::Explicit);
    drop(c);
    let after = php_mysqlnd::stats::global();
    // the explicit close and the destructor both call send_close: -2 (wraps)
    assert_eq!(
        after
            .get(Stat::OpenedConnections)
            .wrapping_sub(before.get(Stat::OpenedConnections)),
        u64::MAX
    );
    assert_eq!(
        after.get(Stat::CloseExplicit) - before.get(Stat::CloseExplicit),
        1
    );
    assert_eq!(after.get(Stat::ComQuit) - before.get(Stat::ComQuit), 1);
}
