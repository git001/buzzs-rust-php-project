// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Rows past `memory_limit` against a live server: the row is read and
//! dropped (the violation stays pending for the VM), and the connection
//! stays in step with the server, so the next query on it works
//! (persistent connections, pools). One `#[test]`, because the limit is
//! process-wide state here.

mod common;

use common::connect;
use php_mysqlnd::Connection;

php_memlimit::install_global_allocator!();

const LIMIT: usize = 16 << 20;
/// One row of 30 MB: two packets, the first 16 MiB.
const BIG_ROW: &[u8] = b"SELECT REPEAT('x', 30000000)";

/// Ends the over-limit phase: takes the violation, as the VM does when it
/// raises the fatal, and lifts the limit for the check that follows.
fn deliver() {
    assert!(php_memlimit::take().is_some(), "a violation is pending");
    php_memlimit::request_end();
}

/// The connection still answers in step.
fn still_in_step(c: &Connection) {
    c.query(b"SELECT 1, 'ok'").unwrap();
    let mut r = c.store_result().unwrap().expect("result set");
    let row = r.fetch_row_values().unwrap().expect("one row");
    assert_eq!(row.len(), 2);
    assert!(r.fetch_row_values().unwrap().is_none());
}

#[test]
fn a_row_past_the_limit_leaves_the_connection_in_step() {
    let Some(c) = connect() else { return };

    // Buffered: the set is stored without the row.
    php_memlimit::request_start(Some(LIMIT)).expect("limit");
    c.query(BIG_ROW).unwrap();
    let r = c.store_result().unwrap().expect("result set");
    assert_eq!(r.num_rows(), 0, "the row was dropped");
    drop(r);
    deliver();
    still_in_step(&c);

    // Unbuffered: no row, and freeing the set drains the rest.
    php_memlimit::request_start(Some(LIMIT)).expect("limit");
    c.query(BIG_ROW).unwrap();
    let mut r = c.use_result().unwrap().expect("result set");
    assert!(
        r.fetch_row_values().unwrap().is_none(),
        "no row past the limit"
    );
    drop(r);
    deliver();
    still_in_step(&c);

    // Many rows: buffering stops, the rest of the set is read.
    php_memlimit::request_start(Some(LIMIT)).expect("limit");
    c.query(b"SET SESSION cte_max_recursion_depth = 1000000")
        .unwrap();
    c.query(
        b"WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 100000) \
          SELECT REPEAT('y', 1000) FROM n",
    )
    .unwrap();
    let r = c.store_result().unwrap().expect("result set");
    assert!(r.num_rows() < 100_000, "buffering stopped");
    drop(r);
    deliver();
    still_in_step(&c);
}
