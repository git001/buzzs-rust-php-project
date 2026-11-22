// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The trans-sid URL rewriter (`ext/standard/url_scanner_ex.re`).
//!
//! The VM has no URL rewriter, and `session.use_trans_sid` needs one: the
//! session module calls `php_url_scanner_add_session_var()`, which starts
//! an internal `URL-Rewriter` output handler that appends `name=id` to
//! relative links and a hidden `<input>` after `<form>` tags. The same
//! scanner backs `output_add_rewrite_var()`/`output_reset_rewrite_vars()`
//! (the VM only has stubs), so this crate provides both.
//!
//! The scanner is a byte-exact port of the re2c state machine, including
//! its `YYFILL` behaviour: a token whose end cannot be decided within the
//! current buffer is kept for the next chunk (and emitted unmodified at a
//! flush), so e.g. a trailing `<form>` is not rewritten, exactly as in PHP.

use std::cell::RefCell;
use std::collections::HashSet;

use php_stdstr::url::{parse_url_ex, rawurlencode};

/// The default `session.trans_sid_tags`.
pub(crate) const DEFAULT_SESSION_TAGS: &str = "a=href,area=href,frame=src,form=";

/// Scanner states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Plain,
    Tag,
    NextArg,
    Arg,
    BeforeVal,
    Val,
}

/// `url_adapt_state_ex_t`.
#[derive(Debug, Default)]
struct Ctx {
    tag: Vec<u8>,
    arg: Vec<u8>,
    val: Vec<u8>,
    buf: Vec<u8>,
    result: Vec<u8>,
    form_app: Vec<u8>,
    url_app: Vec<u8>,
    active: bool,
    lookup_data: Option<Vec<u8>>,
    state: State,
    attr_val: Vec<u8>,
    tag_form: bool,
    attr_action: bool,
}

/// The tag table and host allow-list of one rewriter.
#[derive(Debug, Default, Clone)]
struct Config {
    /// `tag => attribute` (first definition wins).
    tags: Vec<(Vec<u8>, Vec<u8>)>,
    hosts: HashSet<Vec<u8>>,
}

/// Request settings the handler needs (it runs without VM access).
#[derive(Debug, Clone)]
struct Env {
    /// `PG(arg_separator).output`.
    separator: Vec<u8>,
    /// `$_SERVER['HTTP_HOST']`.
    http_host: Option<Vec<u8>>,
}

impl Default for Env {
    fn default() -> Self {
        Self {
            separator: b"&".to_vec(),
            http_host: None,
        }
    }
}

#[derive(Debug, Default)]
struct Scanners {
    session: Ctx,
    output: Ctx,
    session_cfg: Config,
    output_cfg: Config,
    env: Env,
}

thread_local! {
    static SCANNERS: RefCell<Scanners> = RefCell::new(Scanners::default());
}

/// Which rewriter: the session one (`session.trans_sid_*`) or the
/// `output_add_rewrite_var()` one (`url_rewriter.*`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Session,
    Output,
}

fn with<R>(f: impl FnOnce(&mut Scanners) -> R) -> R {
    SCANNERS.with(|s| f(&mut s.borrow_mut()))
}

fn split_tokens(v: &[u8]) -> impl Iterator<Item = &[u8]> {
    v.split(|&c| c == b',').filter(|t| !t.is_empty())
}

/// `php_ini_on_update_tags()`.
pub(crate) fn set_tags(kind: Kind, value: &[u8]) {
    let mut tags: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for tok in split_tokens(value) {
        if let Some(eq) = tok.iter().position(|&c| c == b'=') {
            let key = tok[..eq].to_ascii_lowercase();
            let val = tok[eq + 1..].to_vec();
            if !tags.iter().any(|(k, _)| *k == key) {
                tags.push((key, val));
            }
        }
    }
    with(|s| match kind {
        Kind::Session => s.session_cfg.tags = tags,
        Kind::Output => s.output_cfg.tags = tags,
    });
}

/// `php_ini_on_update_hosts()`.
pub(crate) fn set_hosts(kind: Kind, value: &[u8]) {
    let hosts: HashSet<Vec<u8>> = split_tokens(value)
        .map(<[u8]>::to_ascii_lowercase)
        .collect();
    with(|s| match kind {
        Kind::Session => s.session_cfg.hosts = hosts,
        Kind::Output => s.output_cfg.hosts = hosts,
    });
}

/// Records `arg_separator.output` and `$_SERVER['HTTP_HOST']`.
pub(crate) fn set_env(separator: Vec<u8>, http_host: Option<Vec<u8>>) {
    with(|s| {
        s.env = Env {
            separator,
            http_host,
        }
    });
}

fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic()
}

fn is_ws(c: u8) -> bool {
    matches!(c, b' ' | 0x0b | b'\r' | b'\t' | b'\n')
}

/// `append_modified_url()`.
fn append_modified_url(
    url: &[u8],
    dest: &mut Vec<u8>,
    url_app: &[u8],
    separator: &[u8],
    hosts: &HashSet<Vec<u8>>,
) {
    let Some(parts) = parse_url_ex(url) else {
        dest.extend_from_slice(url);
        return;
    };
    if parts.fragment.is_some() && url.first() == Some(&b'#') {
        dest.extend_from_slice(url);
        return;
    }
    if let Some(scheme) = &parts.scheme {
        if !scheme.eq_ignore_ascii_case(b"http") && !scheme.eq_ignore_ascii_case(b"https") {
            dest.extend_from_slice(url);
            return;
        }
    }
    if let Some(host) = &parts.host {
        if !hosts.contains(&host.to_ascii_lowercase()) {
            dest.extend_from_slice(url);
            return;
        }
    }
    if parts.path.is_none() && parts.query.is_none() && parts.fragment.is_none() {
        dest.extend_from_slice(url);
        dest.extend_from_slice(b"/?");
        dest.extend_from_slice(url_app);
        return;
    }
    if let Some(scheme) = &parts.scheme {
        dest.extend_from_slice(scheme);
        dest.extend_from_slice(b"://");
    } else if url.starts_with(b"//") {
        dest.extend_from_slice(b"//");
    }
    if let Some(user) = &parts.user {
        dest.extend_from_slice(user);
        if let Some(pass) = &parts.pass {
            // Sic: PHP appends the password before the colon.
            dest.extend_from_slice(pass);
            dest.push(b':');
        }
        dest.push(b'@');
    }
    if let Some(host) = &parts.host {
        dest.extend_from_slice(host);
    }
    if let Some(port) = parts.port {
        dest.push(b':');
        dest.extend_from_slice(port.to_string().as_bytes());
    }
    if let Some(path) = &parts.path {
        dest.extend_from_slice(path);
    }
    dest.push(b'?');
    if let Some(query) = &parts.query {
        dest.extend_from_slice(query);
        dest.extend_from_slice(separator);
    }
    dest.extend_from_slice(url_app);
    if let Some(fragment) = &parts.fragment {
        dest.push(b'#');
        dest.extend_from_slice(fragment);
    }
}

/// `check_http_host()`.
fn check_http_host(env: &Env, target: &[u8]) -> bool {
    let Some(host) = &env.http_host else {
        return false;
    };
    let host = match host.iter().position(|&c| c == b':') {
        Some(i) => &host[..i],
        None => &host[..],
    };
    host.eq_ignore_ascii_case(target)
}

/// `check_host_whitelist()`.
fn check_host_whitelist(ctx: &Ctx, cfg: &Config, env: &Env) -> bool {
    if ctx.attr_val.is_empty() {
        return true;
    }
    let Some(parts) = parse_url_ex(&ctx.attr_val) else {
        return false;
    };
    if let Some(scheme) = &parts.scheme {
        if !scheme.eq_ignore_ascii_case(b"http") && !scheme.eq_ignore_ascii_case(b"https") {
            return false;
        }
    }
    let Some(host) = &parts.host else {
        return true;
    };
    if cfg.hosts.is_empty() && check_http_host(env, host) {
        return true;
    }
    cfg.hosts.contains(host)
}

/// A token recognised by one scanner state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    /// Consumes `n` bytes.
    Take(usize, Act),
    /// Matches without consuming (re2c's `--YYCURSOR`).
    Peek(Act),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Lt,
    Text,
    TagName,
    PassToPlain,
    Close,
    Space,
    ToArg,
    ArgName,
    PassToNextArg,
    Eq,
    ToNextArg,
    Quoted(u8),
    Unquoted,
}

/// One re2c match at `pos`; `None` is `YYFILL` (more input needed).
fn scan(state: State, buf: &[u8], pos: usize) -> Option<Tok> {
    let len = buf.len();
    let run = |mut end: usize, pred: &dyn Fn(u8) -> bool| {
        while end < len && pred(buf[end]) {
            end += 1;
        }
        end
    };
    let first = *buf.get(pos)?;
    match state {
        State::Plain => {
            if first == b'<' {
                return Some(Tok::Take(1, Act::Lt));
            }
            let end = run(pos, &|ch| ch != b'<');
            (end < len).then_some(Tok::Take(end - pos, Act::Text))
        }
        State::Tag => {
            if is_alpha(first) || first == b':' {
                let end = run(pos, &|ch| is_alpha(ch) || ch == b':');
                (end < len).then_some(Tok::Take(end - pos, Act::TagName))
            } else {
                Some(Tok::Take(1, Act::PassToPlain))
            }
        }
        State::NextArg => {
            // re2c needs two bytes here (for "/>").
            if len - pos < 2 {
                return None;
            }
            if first == b'>' {
                Some(Tok::Take(1, Act::Close))
            } else if first == b'/' && buf[pos + 1] == b'>' {
                Some(Tok::Take(2, Act::Close))
            } else if is_ws(first) {
                let end = run(pos, &is_ws);
                (end < len).then_some(Tok::Take(end - pos, Act::Space))
            } else if is_alpha(first) {
                Some(Tok::Peek(Act::ToArg))
            } else {
                Some(Tok::Take(1, Act::PassToPlain))
            }
        }
        State::Arg => {
            if is_alpha(first) {
                let end = run(pos + 1, &|ch| is_alpha(ch) || ch == b'-');
                (end < len).then_some(Tok::Take(end - pos, Act::ArgName))
            } else {
                Some(Tok::Take(1, Act::PassToNextArg))
            }
        }
        State::BeforeVal => {
            let eq = run(pos, &|ch| ch == b' ');
            if eq == len {
                return None;
            }
            if buf[eq] == b'=' {
                let end = run(eq + 1, &|ch| ch == b' ');
                (end < len).then_some(Tok::Take(end - pos, Act::Eq))
            } else {
                Some(Tok::Peek(Act::ToNextArg))
            }
        }
        State::Val => {
            let unquoted =
                |ch: u8| !matches!(ch, b' ' | b'\r' | b'\t' | b'\n' | b'>' | b'\'' | b'"');
            if first == b'"' || first == b'\'' {
                let end = run(pos + 1, &|ch| ch != first && ch != b'>');
                if end == len {
                    return None;
                }
                if buf[end] == first {
                    Some(Tok::Take(end + 1 - pos, Act::Quoted(first)))
                } else {
                    Some(Tok::Take(1, Act::PassToNextArg))
                }
            } else if unquoted(first) {
                let end = run(pos, &unquoted);
                (end < len).then_some(Tok::Take(end - pos, Act::Unquoted))
            } else {
                Some(Tok::Take(1, Act::PassToNextArg))
            }
        }
    }
}

/// `xx_mainloop()`.
fn mainloop(ctx: &mut Ctx, cfg: &Config, env: &Env, data: &[u8]) {
    ctx.buf.extend_from_slice(data);
    let buf = std::mem::take(&mut ctx.buf);
    let mut i = 0;
    while let Some(tok) = scan(ctx.state, &buf, i) {
        let (len, act) = match tok {
            Tok::Take(n, a) => (n, a),
            Tok::Peek(a) => (0, a),
        };
        let text = &buf[i..i + len];
        i += len;
        match act {
            Act::Lt => {
                ctx.result.extend_from_slice(text);
                ctx.state = State::Tag;
            }
            Act::Text | Act::Space => ctx.result.extend_from_slice(text),
            Act::TagName => {
                handle_tag(ctx, cfg, text);
                ctx.result.extend_from_slice(text);
            }
            Act::PassToPlain => {
                ctx.result.extend_from_slice(text);
                ctx.state = State::Plain;
            }
            Act::Close => {
                ctx.result.extend_from_slice(text);
                handle_form(ctx, cfg, env);
                ctx.state = State::Plain;
            }
            Act::ToArg => ctx.state = State::Arg,
            Act::ArgName => {
                ctx.result.extend_from_slice(text);
                ctx.arg = text.to_vec();
                ctx.attr_action = ctx.tag_form
                    && b"action"
                        .get(..text.len())
                        .is_some_and(|p| p.eq_ignore_ascii_case(text));
                ctx.state = State::BeforeVal;
            }
            Act::PassToNextArg => {
                ctx.result.extend_from_slice(text);
                ctx.state = State::NextArg;
            }
            Act::Eq => {
                ctx.result.extend_from_slice(text);
                ctx.state = State::Val;
            }
            Act::ToNextArg => ctx.state = State::NextArg,
            Act::Quoted(q) => {
                handle_val(ctx, cfg, env, &text[1..text.len() - 1], Some(q));
                ctx.state = State::NextArg;
            }
            Act::Unquoted => {
                handle_val(ctx, cfg, env, text, None);
                ctx.state = State::NextArg;
            }
        }
    }
    ctx.buf = buf[i..].to_vec();
}

/// `handle_tag()`.
fn handle_tag(ctx: &mut Ctx, cfg: &Config, text: &[u8]) {
    ctx.tag = text.to_ascii_lowercase();
    ctx.lookup_data = cfg
        .tags
        .iter()
        .find(|(k, _)| *k == ctx.tag)
        .map(|(_, v)| v.clone());
    if ctx.lookup_data.is_some() {
        ctx.tag_form = ctx.tag == b"form";
        ctx.state = State::NextArg;
    } else {
        ctx.state = State::Plain;
    }
}

/// `handle_form()`.
fn handle_form(ctx: &mut Ctx, cfg: &Config, env: &Env) {
    if !ctx.form_app.is_empty()
        && ctx.tag.eq_ignore_ascii_case(b"form")
        && check_host_whitelist(ctx, cfg, env)
    {
        let app = ctx.form_app.clone();
        ctx.result.extend_from_slice(&app);
    }
}

/// `handle_val()` + `tag_arg()`.
fn handle_val(ctx: &mut Ctx, cfg: &Config, env: &Env, val: &[u8], quote: Option<u8>) {
    ctx.val = val.to_vec();
    if ctx.tag_form && ctx.attr_action {
        ctx.attr_val = val.to_vec();
    }
    let matches = ctx
        .lookup_data
        .as_ref()
        .is_some_and(|attr| attr.eq_ignore_ascii_case(&ctx.arg));
    if let Some(q) = quote {
        ctx.result.push(q);
    }
    if matches {
        let mut out = Vec::new();
        append_modified_url(val, &mut out, &ctx.url_app, &env.separator, &cfg.hosts);
        ctx.result.extend_from_slice(&out);
    } else {
        ctx.result.extend_from_slice(val);
    }
    if let Some(q) = quote {
        ctx.result.push(q);
    }
}

/// `url_adapt_ext()`.
fn adapt(ctx: &mut Ctx, cfg: &Config, env: &Env, src: &[u8], flush: bool) -> Vec<u8> {
    mainloop(ctx, cfg, env, src);
    let mut out = std::mem::take(&mut ctx.result);
    if flush {
        out.append(&mut ctx.buf);
        ctx.val.clear();
        ctx.attr_val.clear();
    }
    out
}

/// The `URL-Rewriter` output handler body
/// (`php_url_scanner_session_handler_impl`).
fn handler(kind: Kind, output: &[u8], mode: i32) -> Vec<u8> {
    use php_support::output::flags;
    with(|s| {
        let Scanners {
            session,
            output: out_ctx,
            session_cfg,
            output_cfg,
            env,
        } = s;
        let (ctx, cfg) = match kind {
            Kind::Session => (session, &*session_cfg),
            Kind::Output => (out_ctx, &*output_cfg),
        };
        if ctx.url_app.is_empty() {
            let mut r = std::mem::take(&mut ctx.buf);
            ctx.result.clear();
            r.extend_from_slice(output);
            r
        } else {
            let flush = mode & (flags::FLUSH | flags::FINAL) != 0;
            adapt(ctx, cfg, env, output, flush)
        }
    })
}

/// `php_escape_html_entities_ex(ENT_QUOTES | ENT_SUBSTITUTE, no double
/// encoding)`.
fn html_escape(s: &[u8]) -> Vec<u8> {
    let mut notices = Vec::new();
    php_stdstr::html::htmlspecialchars(s, 3 | 8, None, false, &mut notices)
}

/// `php_url_scanner_add_var_impl()`: returns `true` when the output
/// handler must be started.
///
/// Past `memory_limit` nothing is added (the VM raises the fatal):
/// `php_raw_url_encode()` and `php_escape_html_entities_ex()` take three and
/// two times their input up front, and the two buffers grow by the encoded
/// parts; both are checked before anything changes.
pub(crate) fn add_var(kind: Kind, name: &[u8], value: &[u8], encode: bool) -> bool {
    let input = name.len().saturating_add(value.len());
    if encode && php_memlimit::try_reserve_n(input, 5, 0).is_err() {
        return false;
    }
    let (sname, svalue, hname, hvalue) = if encode {
        (
            rawurlencode(name),
            rawurlencode(value),
            html_escape(name),
            html_escape(value),
        )
    } else {
        (name.to_vec(), value.to_vec(), name.to_vec(), value.to_vec())
    };
    with(|s| {
        let sep = s.env.separator.clone();
        let ctx = match kind {
            Kind::Session => &mut s.session,
            Kind::Output => &mut s.output,
        };
        let url_grows = sep.len() + sname.len() + 1 + svalue.len();
        let form_grows = hname.len() + hvalue.len() + 40;
        let fits = if ctx.active {
            php_memlimit::TryGrow::try_grow(&mut ctx.url_app, url_grows).is_ok()
                && php_memlimit::TryGrow::try_grow(&mut ctx.form_app, form_grows).is_ok()
        } else {
            // A fresh context starts with empty buffers.
            php_memlimit::try_reserve(url_grows.saturating_add(form_grows)).is_ok()
        };
        if !fits {
            return false;
        }
        let mut start = false;
        if !ctx.active {
            *ctx = Ctx::default();
            ctx.active = true;
            start = true;
        }
        if !ctx.url_app.is_empty() {
            ctx.url_app.extend_from_slice(&sep);
        }
        ctx.url_app.extend_from_slice(&sname);
        ctx.url_app.push(b'=');
        ctx.url_app.extend_from_slice(&svalue);
        ctx.form_app
            .extend_from_slice(b"<input type=\"hidden\" name=\"");
        ctx.form_app.extend_from_slice(&hname);
        ctx.form_app.extend_from_slice(b"\" value=\"");
        ctx.form_app.extend_from_slice(&hvalue);
        ctx.form_app.extend_from_slice(b"\" />");
        start
    })
}

/// The output handler closure for [`php_support::output::start`].
pub(crate) fn output_handler(kind: Kind) -> php_support::output::InternalHandler {
    Box::new(move |buf: &[u8], mode: i32| Some(handler(kind, buf, mode)))
}

/// `php_url_scanner_reset_vars_impl()`.
pub(crate) fn reset_vars(kind: Kind) {
    with(|s| {
        let ctx = match kind {
            Kind::Session => &mut s.session,
            Kind::Output => &mut s.output,
        };
        ctx.form_app.clear();
        ctx.url_app.clear();
    });
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `php_url_scanner_reset_var_impl()`.
pub(crate) fn reset_var(kind: Kind, name: &[u8], encode: bool) {
    with(|s| {
        let sep = s.env.separator.clone();
        let ctx = match kind {
            Kind::Session => &mut s.session,
            Kind::Output => &mut s.output,
        };
        if ctx.url_app.is_empty() {
            return;
        }
        let (sname, hname) = if encode {
            (rawurlencode(name), html_escape(name))
        } else {
            (name.to_vec(), name.to_vec())
        };
        let mut url_app = sname;
        url_app.push(b'=');
        let mut form_app = b"<input type=\"hidden\" name=\"".to_vec();
        form_app.extend_from_slice(&hname);
        form_app.extend_from_slice(b"\" value=\"");
        let Some(mut start) = find(&ctx.url_app, &url_app) else {
            return;
        };
        let limit = ctx.url_app.len();
        let mut end = start + url_app.len();
        let mut sep_removed = false;
        while end < limit {
            if !sep.is_empty() && ctx.url_app[end..].starts_with(&sep) {
                end += sep.len();
                sep_removed = true;
                break;
            }
            end += 1;
        }
        if ctx.url_app.len() == end - start {
            ctx.form_app.clear();
            ctx.url_app.clear();
            return;
        }
        if !sep_removed && start >= sep.len() && ctx.url_app[start - sep.len()..start] == sep[..] {
            start -= sep.len();
        }
        ctx.url_app.drain(start..end);
        let Some(fstart) = find(&ctx.form_app, &form_app) else {
            ctx.form_app.clear();
            ctx.url_app.clear();
            return;
        };
        let flimit = ctx.form_app.len();
        let mut fend = fstart + form_app.len();
        while fend < flimit {
            if ctx.form_app[fend] == b'>' {
                fend += 1;
                break;
            }
            fend += 1;
        }
        ctx.form_app.drain(fstart..fend);
    });
}

/// `RSHUTDOWN`: deactivates both rewriters.
pub(crate) fn request_shutdown() {
    with(|s| {
        s.session = Ctx::default();
        s.output = Ctx::default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(tags: &str) -> Config {
        let mut c = Config::default();
        for tok in tags.split(',') {
            if let Some((k, v)) = tok.split_once('=') {
                c.tags.push((k.as_bytes().to_vec(), v.as_bytes().to_vec()));
            }
        }
        c
    }

    fn rewrite(tags: &str, chunks: &[&str]) -> String {
        let c = cfg(tags);
        let env = Env::default();
        let mut ctx = Ctx {
            url_app: b"a=b".to_vec(),
            form_app: b"<input type=\"hidden\" name=\"a\" value=\"b\" />".to_vec(),
            active: true,
            ..Ctx::default()
        };
        let mut out = Vec::new();
        for (i, ch) in chunks.iter().enumerate() {
            let last = i + 1 == chunks.len();
            out.extend(adapt(&mut ctx, &c, &env, ch.as_bytes(), last));
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn rewrites_links_like_php() {
        let t = "a=href,form=";
        assert_eq!(rewrite(t, &["<a href=x>"]), "<a href=x?a=b>");
        assert_eq!(rewrite(t, &["<a href=x"]), "<a href=x");
        assert_eq!(rewrite(t, &["<a href=\"x\""]), "<a href=\"x?a=b\"");
        assert_eq!(rewrite(t, &["<a href=x "]), "<a href=x?a=b ");
        assert_eq!(rewrite(t, &["<a href = \"x\">"]), "<a href = \"x?a=b\">");
        assert_eq!(rewrite(t, &["<a href=x", "y>"]), "<a href=xy?a=b>");
        assert_eq!(
            rewrite(t, &["<a href=\"/p?q=1#f\">"]),
            "<a href=\"/p?q=1&a=b#f\">"
        );
        assert_eq!(rewrite(t, &["<a href=\"#top\">"]), "<a href=\"#top\">");
        assert_eq!(
            rewrite(t, &["<a href=\"http://php.net/\">"]),
            "<a href=\"http://php.net/\">"
        );
        assert_eq!(
            rewrite(t, &["<a href=\"mailto:x@y\">"]),
            "<a href=\"mailto:x@y\">"
        );
    }

    #[test]
    fn forms_need_lookahead() {
        let t = "a=href,form=";
        let input = "<input type=\"hidden\" name=\"a\" value=\"b\" />";
        assert_eq!(rewrite(t, &["<form>"]), "<form>");
        assert_eq!(rewrite(t, &["<form/>"]), format!("<form/>{input}"));
        assert_eq!(rewrite(t, &["<form>x"]), format!("<form>{input}x"));
        assert_eq!(rewrite(t, &["<form >"]), "<form >");
        assert_eq!(
            rewrite(t, &["<form action=\"http://bad.net/\">x"]),
            "<form action=\"http://bad.net/\">x"
        );
    }

    #[test]
    fn host_whitelist_for_links() {
        let mut c = cfg("a=href");
        c.hosts.insert(b"php.net".to_vec());
        let env = Env::default();
        let mut ctx = Ctx {
            url_app: b"s=1".to_vec(),
            active: true,
            ..Ctx::default()
        };
        let out = adapt(&mut ctx, &c, &env, b"<a href=\"http://php.net\"> ", true);
        assert_eq!(out, b"<a href=\"http://php.net/?s=1\"> ");
    }

    #[test]
    fn reset_var_removes_one_variable() {
        request_shutdown();
        set_env(b"&".to_vec(), None);
        assert!(add_var(Kind::Output, b"a", b"1", true));
        assert!(!add_var(Kind::Output, b"b", b"2", true));
        reset_var(Kind::Output, b"a", true);
        with(|s| {
            assert_eq!(s.output.url_app, b"b=2");
            assert_eq!(
                s.output.form_app,
                b"<input type=\"hidden\" name=\"b\" value=\"2\" />"
            );
        });
        reset_vars(Kind::Output);
        with(|s| assert!(s.output.url_app.is_empty()));
        request_shutdown();
    }
}
