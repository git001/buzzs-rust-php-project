// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Oracle tests for `strip_tags()`: hand-picked inputs from the php-src
//! `.phpt` files plus a deterministic random corpus over the characters
//! the state machine cares about.
#![expect(clippy::disallowed_methods, reason = "test code")]

mod common;
use common::{check, lit, pure, regenerate, Case};
use php_stdstr::strip_tags::{allowed_tags_from_list, strip_tags};

const STRS: &[&[u8]] = &[
    b"",
    b"NEAT <? cool < blah ?> STUFF",
    b"NEAT <? cool > blah ?> STUFF",
    b"NEAT <!-- cool < blah --> STUFF",
    b"NEAT <!-- cool > blah --> STUFF",
    b"NEAT <? echo \\\"\\\\\\\"\\\"?> STUFF",
    b"NEAT <? echo '\\''?> STUFF",
    b"TESTS ?!!?!?!!!?!!",
    b"<html>hello</html>",
    b"<?php echo hello ?>",
    b"<% echo hello %>",
    b"<script language=\"PHP\"> echo hello </script>",
    b"<html><b>hello</b><p>world</p></html>",
    b"<html><!-- COMMENT --></html>",
    b"<html><a>hello</a></html><p>world</p><!-- COMMENT --><?php echo hello ?>",
    b"<abc>hello</abc> \t\tworld... <ppp>strip_tags_test</ppp>",
    b"<%?php hello\t world?%>",
    b"<<htmL>>hello<</htmL>>",
    b"<a.>HtMl text</.a>",
    b"<nnn>I am a quoted (\") string with special chars like $,!,@,%,&</nnn>",
    b"<html> I am html string </html>\0<?php I am php string ?>",
    b"<html> I am html string\0 </html><?php I am php string ?>",
    b"<html>I am html string</html>1000001<?php I am php string?>",
    b"a < b and c > d",
    b"a <b and c> d",
    b"<a href='x>y'>link</a>",
    b"<a href=\"x>y\">link</a>",
    b"<a title='it\"s'>x</a>",
    b"<br/><br /><BR>< br>",
    b"<p>Hi <a href='x'>l</a><br/></p>",
    b"<?xml version=\"1.0\"?><root>t</root>",
    b"<?xml version=\"1.0\"--><root>t</root>",
    b"<!DOCTYPE html><html>x</html>",
    b"<!doctype html PUBLIC \"-//W3C\"><b>x</b>",
    b"<!-->x-->y",
    b"<!--->x-->y",
    b"<!- not comment ->z",
    b"<?php if (a > b) { echo \"?>\"; } ?>after",
    b"<?php f(\")\"); ?>after",
    b"<?php (( ?> x ?>",
    b"<? ' ?> ' ?>tail",
    b"x<y<z>>w",
    b"x<y<z>w>v",
    b"<a <b>>text",
    b">>text<<",
    b"<",
    b">",
    b"<a",
    b"<!--",
    b"<?",
    b"'quoted' \"text\" outside",
    b"<p class=\"a\" id='b'>t</p>",
    b"<P>Upper</P><Br/>",
    b"<abbr>a</abbr><b>b</b>",
    b"<a\n href=x>multi\nline</a>",
    b"<a\thref=x>tab</a>",
    b"</ a>x</a >",
    b"<scr<script>ipt>alert(1)</script>",
    b"\xe4<b>\xf6</b>\xfc",
];

const ALLOWS: &[Option<&[u8]>] = &[
    None,
    Some(b""),
    Some(b"<a>"),
    Some(b"<b>"),
    Some(b"<a><b>"),
    Some(b"<p><a><?php<html>"),
    Some(b"<br>"),
    Some(b"<BR><P>"),
    Some(b"<abbr>"),
    Some(b"<root>"),
    Some(b"<!doctype>"),
    Some(b"a"),
    Some(b"<a\0><b>"),
    Some(b"<scr>"),
    Some(b"<script>"),
    Some(b"<html><nnn>"),
];

/// Small deterministic LCG for the random corpus.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

fn random_corpus() -> Vec<Vec<u8>> {
    const ALPHA: &[u8] = b"<<<>>>??!!--''\"\"\\\\abx lmLM/ =()\0\nDOCTYPEdoctype";
    let mut rng = Lcg(0x5eed);
    let mut out = Vec::new();
    for _ in 0..600 {
        let len = usize::try_from(rng.next() % 24).unwrap_or(0);
        let s: Vec<u8> = (0..len)
            .map(|_| ALPHA[usize::try_from(rng.next()).unwrap_or(0) % ALPHA.len()])
            .collect();
        out.push(s);
    }
    out
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    for s in STRS {
        for a in ALLOWS {
            let al = a.map_or_else(|| "null".to_owned(), lit);
            v.push(pure(
                &format!("strip_tags({}, {al})", lit(s)),
                strip_tags(s, *a),
            ));
        }
        // Array form.
        let list: [&[u8]; 2] = [b"a", b"BR"];
        let allow = allowed_tags_from_list(&list);
        v.push(pure(
            &format!("strip_tags({}, ['a', 'BR'])", lit(s)),
            strip_tags(s, allow.as_deref()),
        ));
        let empty: [&[u8]; 0] = [];
        v.push(pure(
            &format!("strip_tags({}, [])", lit(s)),
            strip_tags(s, allowed_tags_from_list(&empty).as_deref()),
        ));
    }
    for s in random_corpus() {
        for a in [None, Some(&b"<a><b>"[..]), Some(b"<x>")] {
            let al = a.map_or_else(|| "null".to_owned(), lit);
            v.push(pure(
                &format!("strip_tags({}, {al})", lit(&s)),
                strip_tags(&s, a),
            ));
        }
    }
    v
}

#[test]
fn oracle() {
    check("strip_tags", &cases());
}

#[test]
#[ignore = "runs /usr/bin/php to rebuild the fixture"]
fn regenerate_fixture() {
    regenerate("strip_tags", &cases());
}
