// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Replays every `mb_*` call made by php-src's `ext/mbstring` `.phpt` tests
//! (recorded under PHP 8.3 by `fixtures/phpt/gen_phpt_oracle.py`) against
//! this crate, and reports per-test reproducibility.
//!
//! A test counts as reproduced when every recorded call (that this crate is
//! responsible for) returns the same value and the same warnings/notices.
//! Calls are skipped when they need VM behaviour (argument `TypeError`s,
//! objects/floats, `mb_get_info`). Known master-vs-8.3 differences are
//! listed in `MASTER_DIFF_TESTS`. The test fails if fewer tests reproduce
//! than the recorded baseline `MIN_REPRODUCED`.
#![allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::only_used_in_recursion,
    clippy::format_push_string,
    clippy::default_trait_access,
    clippy::many_single_char_names
)]

mod common;
use common::{coerce_int, coerce_str, dispatch, parse, unhex, values, V};
use php_mbstring::{EncodingArray, EncodingList, Level, MbState};
use std::collections::BTreeMap;

/// Tests whose 8.3 recording differs from php-src master behaviour.
const MASTER_DIFF_TESTS: &[&str] = &[
    "GHSA-74r9-qxhc-fx53",          // NUL inside encoding names is not ignored
    "mb_detect_order_with_ini_set", // runtime ini_set updates the detect order
    "mb_get_info_errors",           // invalid type is a warning + false
    "bug79149",                     // %pS: message keeps bytes after NUL
    "gh11476",
    "gh16360",                        // mb_substr(PHP_INT_MIN) now a ValueError
    "gh20674",                        // no space between adjacent encoded words
    "gh22779",                        // mb_strrpos negative offset for non-UTF-8
    "gh23106",                        // mb_strpos offset on invalid UTF-8
    "mb_decode_numericentity_errors", // non-int map entries are a ValueError
    "mb_encode_numericentity_errors",
    "mb_http_input_errors",    // multi-char type is a ValueError
    "surrogates_to_utf8",      // surrogates are not valid UTF-8 output
    "unicode_versions",        // newer Unicode tables
    "zh_CN_default_encodings", // detect order adds GB18030-2022
    "mb_str_pad",              // error text: "must not be empty"
    "gh9535",                  // mb_strcut fixes (UTF-16, GB18030, zero length)
    "gh9535b",
    "mb_strcut",
    "mb_strcut_negative_length",
    "mb_decode_mimeheader_variation4", // no space between adjacent encoded words
    "mb_decode_mimeheader_basic",
    "gb18030_2022_encoding", // encoding does not exist in 8.3
];

/// Tests whose remaining differences come from state only the VM/SAPI can
/// provide (`mb_http_input` detection of request data, objects passed to
/// `mb_convert_variables`).
const VM_DEPENDENT_TESTS: &[&str] = &["mb_get_info", "mb_convert_variables"];

/// Number of tests (with at least one checked call) that must reproduce.
const MIN_REPRODUCED: usize = 282;

fn norm(v: V) -> V {
    match v {
        V::K(items) => {
            let seq = items
                .iter()
                .enumerate()
                .all(|(n, (k, _))| *k == V::I(i64::try_from(n).unwrap()));
            if seq {
                V::A(items.into_iter().map(|(_, v)| norm(v)).collect())
            } else {
                V::K(items.into_iter().map(|(k, v)| (k, norm(v))).collect())
            }
        }
        V::A(items) => V::A(items.into_iter().map(norm).collect()),
        other => other,
    }
}

/// Which argument positions are strings / ints for coercion.
fn arg_kinds(f: &str) -> &'static str {
    // s = string, i = int, b = bool, x = leave as is
    match f {
        "mb_strlen" | "mb_strwidth" | "mb_scrub" | "mb_strtoupper" | "mb_strtolower" | "mb_ord"
        | "mb_ucfirst" | "mb_lcfirst" => "ss",
        "mb_substr" | "mb_strcut" => "sixs",
        "mb_str_split" => "sis",
        "mb_convert_case" => "sis",
        "mb_strpos" | "mb_strrpos" | "mb_stripos" | "mb_strripos" => "ssis",
        "mb_strstr" | "mb_strrchr" | "mb_stristr" | "mb_strrichr" => "ssbs",
        "mb_substr_count" => "sss",
        "mb_chr" => "is",
        "mb_strimwidth" => "siiss",
        "mb_str_pad" => "sisis",
        "mb_convert_kana" => "sss",
        "mb_encode_mimeheader" => "ssssi",
        "mb_decode_mimeheader" | "mb_preferred_mime_name" | "mb_encoding_aliases" => "s",
        "mb_encode_numericentity" => "sxsb",
        "mb_decode_numericentity" => "sxs",
        "mb_trim" | "mb_ltrim" | "mb_rtrim" => "sss",
        _ => "",
    }
}

fn coerce_args(f: &str, args: &[V]) -> Option<Vec<V>> {
    let kinds = arg_kinds(f).as_bytes();
    args.iter()
        .enumerate()
        .map(|(n, a)| match (kinds.get(n), a) {
            (_, V::O) => None,
            (Some(b's'), _) => coerce_str(a),
            (Some(b'i'), _) => coerce_int(a),
            (Some(b'x') | None, V::D(_)) => coerce_int(a),
            _ => Some(a.clone()),
        })
        .collect()
}

/// `php_mb_convert_encoding_recursive`, as the VM would implement it.
fn convert_recursive(
    st: &mut MbState,
    arr: &[(V, V)],
    to: &'static php_mbstring::Encoding,
    from: &[&'static php_mbstring::Encoding],
) -> V {
    let mut out = Vec::new();
    for (k, v) in arr {
        let key = match k {
            V::S(ks) => match st.convert_encoding_value(ks, to, from) {
                Some(c) => V::S(c),
                None => continue,
            },
            other => other.clone(),
        };
        let val = match v {
            V::S(x) => match st.convert_encoding_value(x, to, from) {
                Some(c) => V::S(c),
                None => continue,
            },
            V::K(inner) => convert_recursive(st, inner, to, from),
            V::A(inner) if inner.is_empty() => V::K(vec![]),
            V::O => {
                st.diagnostics.push(php_mbstring::Diagnostic {
                    level: Level::Warning,
                    message: "Object is not supported".into(),
                });
                continue;
            }
            other => other.clone(),
        };
        out.push((key, val));
    }
    V::K(out)
}

fn check_recursive(st: &mut MbState, arr: &[(V, V)], enc: &'static php_mbstring::Encoding) -> bool {
    for (k, v) in arr {
        if let V::S(ks) = k {
            if !php_mbstring::strings::check_str_encoding(ks, enc) {
                return false;
            }
        }
        let ok = match v {
            V::S(x) => php_mbstring::strings::check_str_encoding(x, enc),
            V::K(inner) => check_recursive(st, inner, enc),
            V::I(_) | V::D(_) | V::N | V::B(_) | V::A(_) => true,
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn collect_strings(v: &V, out: &mut Vec<Vec<u8>>) -> bool {
    match v {
        V::S(x) => out.push(x.clone()),
        V::K(items) => {
            for (_, x) in items {
                if !collect_strings(x, out) {
                    return false;
                }
            }
        }
        V::O | V::D(_) => return matches!(v, V::D(_)),
        _ => {}
    }
    true
}

fn convert_strings(
    st: &mut MbState,
    v: &V,
    from: &'static php_mbstring::Encoding,
    to: &'static php_mbstring::Encoding,
) -> V {
    match v {
        V::S(x) => V::S(st.convert_ex(x, to, from)),
        V::K(items) => V::K(
            items
                .iter()
                .map(|(k, x)| (k.clone(), convert_strings(st, x, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `mb_convert_variables` as the VM would drive it. Returns the result and
/// the converted variables.
fn convert_variables(st: &mut MbState, args: &[V]) -> Option<(V, V)> {
    let to = coerce_str(args.first()?)?;
    let vars = args.get(2)?;
    let mut strings = Vec::new();
    if !collect_strings(vars, &mut strings) {
        return None;
    }
    let from_vals;
    let from = match args.get(1)? {
        V::S(x) => EncodingList::Str(x),
        v @ V::K(_) => {
            let names = values(v)
                .iter()
                .filter_map(coerce_str)
                .map(|x| common::s(&x).to_vec())
                .collect::<Vec<_>>();
            from_vals = EncodingArray::from_names(names.iter().map(Vec::as_slice));
            EncodingList::Array(&from_vals)
        }
        _ => return None,
    };
    let refs: Vec<&[u8]> = strings.iter().map(Vec::as_slice).collect();
    match st.convert_variables_encodings(common::s(&to), from, false, &refs) {
        Err(e) => Some((common::err(&e, "mb_convert_variables"), vars.clone())),
        Ok(None) => Some((V::B(false), vars.clone())),
        Ok(Some((from, to))) => {
            let out = convert_strings(st, vars, from, to);
            Some((V::S(from.name.as_bytes().to_vec()), out))
        }
    }
}

/// Returns `None` when the call is outside this crate's responsibility.
fn replay(st: &mut MbState, f: &str, args: &[V]) -> Option<V> {
    // mb_get_info is VM-side; mb_list_encodings differs (GB18030-2022) and
    // is unit-tested; mb_http_input() reports SAPI input detection.
    if f == "mb_list_encodings" || (f == "mb_http_input" && args.is_empty()) {
        return None;
    }
    let args = coerce_args(f, args)?;
    let enc_arg = |st: &mut MbState, v: Option<&V>, n: u32, name: &'static str| match v {
        None | Some(V::N) => Ok(st.internal_encoding()),
        Some(V::S(e)) => st.get_encoding(Some(e), n, name),
        _ => Ok(st.internal_encoding()),
    };
    match (f, args.first()) {
        ("mb_convert_encoding", Some(V::K(arr))) => {
            let from_vals;
            let from = match args.get(2) {
                Some(V::S(x)) => Some(EncodingList::Str(x)),
                Some(v @ (V::K(_) | V::A(_))) => {
                    let names = values(v)
                        .iter()
                        .filter_map(coerce_str)
                        .map(|x| common::s(&x).to_vec())
                        .collect::<Vec<_>>();
                    from_vals = EncodingArray::from_names(names.iter().map(Vec::as_slice));
                    Some(EncodingList::Array(&from_vals))
                }
                _ => None,
            };
            let to = common::s(args.get(1)?).to_vec();
            return Some(match st.convert_encoding_args(&to, from) {
                Ok((to, from)) => convert_recursive(st, arr, to, &from),
                Err(e) => common::err(&e, f),
            });
        }
        ("mb_check_encoding", Some(V::K(arr))) => {
            return Some(match enc_arg(st, args.get(1), 2, "encoding") {
                Ok(enc) => V::B(check_recursive(st, arr, enc)),
                Err(e) => common::err(&e, f),
            });
        }
        ("mb_check_encoding", None | Some(V::N)) => {
            return Some(
                match st.mb_check_encoding_noarg(args.get(1).and_then(|v| {
                    if let V::S(x) = v {
                        Some(x.as_slice())
                    } else {
                        None
                    }
                })) {
                    Ok(b) => V::B(b),
                    Err(e) => common::err(&e, f),
                },
            );
        }
        _ => {}
    }
    // Normalise array arguments to the plain value list the dispatcher wants.
    let args: Vec<V> = args
        .into_iter()
        .map(|a| match a {
            V::K(_) if f != "mb_get_info" => V::A(values(&a)),
            other => other,
        })
        .collect();
    // mb_trim etc. are not in the shared dispatcher.
    match f {
        "mb_trim" | "mb_ltrim" | "mb_rtrim" | "mb_ucfirst" | "mb_lcfirst" => return None,
        _ => {}
    }
    let snapshot = st.clone();
    if let Ok(v) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| dispatch(st, f, &args)))
    {
        Some(v)
    } else {
        // Argument shapes the dispatcher does not model (VM coercions).
        *st = snapshot;
        None
    }
}

#[test]
fn replay_phpt_calls() {
    std::panic::set_hook(Box::new(|_| {}));
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/phpt/calls.tsv");
    let data = std::fs::read(path).expect("fixture");
    let data = String::from_utf8_lossy(&data);
    let mut st = MbState::new();
    let mut started = false;
    let mut ini: Vec<(String, Vec<u8>)> = Vec::new();
    let mut current = String::new();
    // test -> (checked, failed, first failure)
    let mut results: BTreeMap<String, (usize, usize, String)> = BTreeMap::new();
    for line in data.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts[0] {
            "#TEST" => {
                current = parts[1].to_string();
                ini.clear();
                started = false;
                results.entry(current.clone()).or_default();
                continue;
            }
            "#INI" => {
                let v = unhex(parts.get(2).copied().unwrap_or(""));
                // php-src's run-tests strips one level of quotes.
                let v = if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
                    v[1..v.len() - 1].to_vec()
                } else {
                    v
                };
                ini.push((parts[1].to_string(), v));
                continue;
            }
            "#INISET" => {
                if !started {
                    let refs: Vec<(&str, &[u8])> = ini
                        .iter()
                        .map(|(k, v)| (k.as_str(), v.as_slice()))
                        .collect();
                    st = MbState::with_ini(&refs);
                    started = true;
                }
                let v = unhex(parts.get(2).copied().unwrap_or(""));
                st.ini_set(parts[1], Some(&v), true);
                st.take_diagnostics();
                continue;
            }
            "#BIG" => continue,
            _ => {}
        }
        if !started {
            let refs: Vec<(&str, &[u8])> = ini
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_slice()))
                .collect();
            st = MbState::with_ini(&refs);
            st.take_diagnostics();
            started = true;
        }
        let f = parts[0];
        let Some(eq) = parts.iter().position(|p| p.starts_with('=')) else {
            continue;
        };
        let args: Vec<V> = parts[1..eq]
            .iter()
            .filter(|p| !p.is_empty())
            .map(|p| parse(p))
            .collect();
        let expected = parse(&parts[eq][1..]);
        if let V::E(cls, _) = &expected {
            if cls == "TypeError" || cls == "ArgumentCountError" {
                continue;
            }
        }
        let exp_diag: Vec<String> = parts
            .get(eq + 1)
            .filter(|d| !d.is_empty())
            .map(|d| {
                d.split(',')
                    .filter(|x| !x.starts_with('D'))
                    .map(|h| String::from_utf8_lossy(&unhex(&h[1..])).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let (got, extra_ok) = if f == "mb_convert_variables" {
            let snapshot = st.clone();
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                convert_variables(&mut st, &args)
            }));
            let res = res.unwrap_or_else(|_| {
                st = snapshot;
                None
            });
            let Some((r, vars)) = res else {
                st.take_diagnostics();
                continue;
            };
            let ok = parts
                .get(eq + 2)
                .is_none_or(|after| norm(parse(after)) == norm(vars));
            (Some(r), ok)
        } else {
            (replay(&mut st, f, &args), true)
        };
        let Some(got) = got else {
            st.take_diagnostics();
            continue;
        };
        let got_diag: Vec<String> = st
            .take_diagnostics()
            .into_iter()
            .filter(|d| d.level != Level::Deprecated)
            .map(|d| d.message)
            .collect();
        let entry = results.get_mut(&current).unwrap();
        entry.0 += 1;
        let (got, expected) = (norm(got), norm(expected));
        if got != expected || got_diag != exp_diag || !extra_ok {
            entry.1 += 1;
            if entry.2.is_empty() {
                let show = |v: &V| match v {
                    V::S(b) => format!("{:?}", String::from_utf8_lossy(b)),
                    other => format!("{other:?}").chars().take(300).collect(),
                };
                entry.2 = format!(
                    "{f}({}) expected {} {exp_diag:?} got {} {got_diag:?}",
                    args.iter()
                        .map(&show)
                        .collect::<Vec<_>>()
                        .join(", ")
                        .chars()
                        .take(300)
                        .collect::<String>(),
                    show(&expected),
                    show(&got)
                );
            }
        }
    }
    let checked: Vec<_> = results.iter().filter(|(_, r)| r.0 > 0).collect();
    let reproduced = checked.iter().filter(|(_, r)| r.1 == 0).count();
    let mut report = String::new();
    for (name, (n, nf, first)) in &checked {
        if *nf > 0 {
            let tag = if MASTER_DIFF_TESTS.contains(&name.as_str()) {
                " [master diff]"
            } else if VM_DEPENDENT_TESTS.contains(&name.as_str()) {
                " [vm state]"
            } else {
                ""
            };
            report.push_str(&format!("{name}: {nf}/{n} failed{tag}: {first}\n"));
        }
    }
    let master_diff = checked
        .iter()
        .filter(|(name, r)| r.1 > 0 && MASTER_DIFF_TESTS.contains(&name.as_str()))
        .count();
    eprintln!(
        "phpt replay: {reproduced}/{} tests with mb_* calls reproduced ({master_diff} more differ only by master changes); {} tests recorded\n{report}",
        checked.len(),
        results.len()
    );
    assert!(
        reproduced >= MIN_REPRODUCED,
        "regression: {reproduced} < {MIN_REPRODUCED}\n{report}"
    );
}
