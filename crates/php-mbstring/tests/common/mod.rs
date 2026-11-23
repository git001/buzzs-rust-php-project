// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Shared helpers for the oracle replay tests: value encoding (see
//! `fixtures/strings/gen_oracle.php`) and dispatch of `mb_*` calls to
//! `MbState` methods.
#![allow(
    clippy::too_many_lines,
    clippy::match_same_arms,
    clippy::only_used_in_recursion,
    clippy::format_push_string,
    clippy::default_trait_access,
    clippy::many_single_char_names
)]
#![allow(dead_code)]

use php_mbstring::state::SubstituteArg;
use php_mbstring::strings::StrstrVariant;
use php_mbstring::{EncodingArray, EncodingList, HttpInput, MbError, MbState, SubstituteChar};

#[derive(Debug, Clone, PartialEq)]
pub enum V {
    S(Vec<u8>),
    I(i64),
    B(bool),
    N,
    A(Vec<V>),
    /// Keyed array (`A[k=v,...]`).
    K(Vec<(V, V)>),
    D(String),
    O,
    E(String, String),
}

pub fn unhex(s: &str) -> Vec<u8> {
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

pub fn parse(s: &str) -> V {
    match s.as_bytes().first() {
        Some(b's') => V::S(unhex(&s[1..])),
        Some(b'i') => V::I(s[1..].parse().unwrap()),
        Some(b'T') => V::B(true),
        Some(b'F') => V::B(false),
        Some(b'N') => V::N,
        Some(b'A') if s.as_bytes().get(1) == Some(&b'[') => parse_keyed(&s[2..s.len() - 1]),
        Some(b'd') => V::D(String::from_utf8(unhex(&s[1..])).unwrap()),
        Some(b'O') => V::O,
        Some(b'A') => V::A(if s.len() == 1 {
            vec![]
        } else {
            s[1..].split(',').map(parse).collect()
        }),
        Some(b'E') => {
            let (cls, msg) = s[1..].split_once(':').unwrap();
            V::E(cls.to_string(), String::from_utf8(unhex(msg)).unwrap())
        }
        _ => panic!("bad value {s}"),
    }
}

pub fn s(v: &V) -> &[u8] {
    match v {
        V::S(b) => b,
        other => panic!("expected string, got {other:?}"),
    }
}

/// Coerce a scalar argument to a PHP string (as zpp would).
pub fn coerce_str(v: &V) -> Option<V> {
    Some(match v {
        V::S(_) | V::N => v.clone(),
        V::I(n) => V::S(n.to_string().into_bytes()),
        V::B(true) => V::S(b"1".to_vec()),
        V::B(false) => V::S(Vec::new()),
        _ => return None,
    })
}

/// Coerce a scalar argument to a PHP int (as zpp would, non-strict).
pub fn coerce_int(v: &V) -> Option<V> {
    Some(match v {
        V::I(_) | V::N => v.clone(),
        V::B(b) => V::I(i64::from(*b)),
        V::S(t) => V::I(std::str::from_utf8(t).ok()?.trim().parse().ok()?),
        V::D(d) => {
            let f: f64 = d.parse().ok()?;
            if f.fract() != 0.0 || !f.is_finite() {
                return None;
            }
            #[allow(clippy::cast_possible_truncation)]
            V::I(f as i64)
        }
        _ => return None,
    })
}
pub fn os(v: Option<&V>) -> Option<&[u8]> {
    match v {
        None | Some(V::N) => None,
        Some(V::S(b)) => Some(b),
        other => panic!("expected ?string, got {other:?}"),
    }
}
pub fn i(v: &V) -> i64 {
    match v {
        V::I(n) => *n,
        other => panic!("expected int, got {other:?}"),
    }
}
pub fn oi(v: Option<&V>) -> Option<i64> {
    match v {
        None | Some(V::N) => None,
        Some(V::I(n)) => Some(*n),
        other => panic!("expected ?int, got {other:?}"),
    }
}
pub fn b(v: &V) -> bool {
    match v {
        V::B(b) => *b,
        V::I(n) => *n != 0,
        V::S(t) => !(t.is_empty() || t == b"0"),
        _ => false,
    }
}

pub fn strs(v: &V) -> Vec<Vec<u8>> {
    match v {
        V::A(items) => items.iter().map(|x| s(x).to_vec()).collect(),
        V::K(items) => items.iter().map(|(_, x)| s(x).to_vec()).collect(),
        other => panic!("expected array, got {other:?}"),
    }
}

pub fn err(e: &MbError, func: &str) -> V {
    V::E(e.class().to_string(), e.message_for(func))
}

pub fn bytes_or_false(r: Option<Vec<u8>>) -> V {
    r.map_or(V::B(false), V::S)
}

pub fn int_or_false(r: Option<i64>) -> V {
    r.map_or(V::B(false), V::I)
}

#[allow(clippy::too_many_lines)]
pub fn dispatch(st: &mut MbState, f: &str, a: &[V]) -> V {
    macro_rules! r {
        ($e:expr, $ok:expr) => {
            match $e {
                Ok(x) => $ok(x),
                Err(e) => err(&e, f),
            }
        };
    }
    let list_arg = |v: &V| -> (Option<EncodingArray>, Option<Vec<u8>>) {
        match v {
            V::A(_) | V::K(_) => {
                let names = strs(v);
                (
                    Some(EncodingArray::from_names(names.iter().map(Vec::as_slice))),
                    None,
                )
            }
            V::S(x) => (None, Some(x.clone())),
            _ => (None, None),
        }
    };
    match f {
        "mb_strlen" => r!(st.mb_strlen(s(&a[0]), os(a.get(1))), V::I),
        "mb_strwidth" => r!(st.mb_strwidth(s(&a[0]), os(a.get(1))), V::I),
        "mb_check_encoding" => r!(st.mb_check_encoding(s(&a[0]), os(a.get(1))), V::B),
        "mb_scrub" => r!(st.mb_scrub(s(&a[0]), os(a.get(1))), V::S),
        "mb_substr" => r!(
            st.mb_substr(s(&a[0]), i(&a[1]), oi(a.get(2)), os(a.get(3))),
            V::S
        ),
        "mb_strcut" => r!(
            st.mb_strcut(s(&a[0]), i(&a[1]), oi(a.get(2)), os(a.get(3))),
            V::S
        ),
        "mb_str_split" => r!(
            st.mb_str_split(s(&a[0]), i(&a[1]), os(a.get(2))),
            |v: Vec<Vec<u8>>| V::A(v.into_iter().map(V::S).collect())
        ),
        "mb_strtoupper" => r!(st.mb_strtoupper(s(&a[0]), os(a.get(1))), V::S),
        "mb_strtolower" => r!(st.mb_strtolower(s(&a[0]), os(a.get(1))), V::S),
        "mb_convert_case" => r!(st.mb_convert_case(s(&a[0]), i(&a[1]), os(a.get(2))), V::S),
        "mb_convert_encoding" => {
            let (arr, st_) = a.get(2).map_or((None, None), list_arg);
            let from = match (&arr, &st_) {
                (Some(x), _) => Some(EncodingList::Array(x)),
                (_, Some(x)) => Some(EncodingList::Str(x)),
                _ => None,
            };
            r!(
                st.mb_convert_encoding(s(&a[0]), s(&a[1]), from),
                bytes_or_false
            )
        }
        "mb_ord" => r!(st.mb_ord(s(&a[0]), os(a.get(1))), int_or_false),
        "mb_chr" => r!(st.mb_chr(i(&a[0]), os(a.get(1))), bytes_or_false),
        "mb_strimwidth" => r!(
            st.mb_strimwidth(
                s(&a[0]),
                i(&a[1]),
                i(&a[2]),
                a.get(3).map_or(&b""[..], s),
                os(a.get(4))
            ),
            V::S
        ),
        "mb_strpos" => r!(
            st.mb_strpos(s(&a[0]), s(&a[1]), i(&a[2]), os(a.get(3))),
            int_or_false
        ),
        "mb_strrpos" => r!(
            st.mb_strrpos(s(&a[0]), s(&a[1]), i(&a[2]), os(a.get(3))),
            int_or_false
        ),
        "mb_stripos" => r!(
            st.mb_stripos(s(&a[0]), s(&a[1]), i(&a[2]), os(a.get(3))),
            int_or_false
        ),
        "mb_strripos" => r!(
            st.mb_strripos(s(&a[0]), s(&a[1]), i(&a[2]), os(a.get(3))),
            int_or_false
        ),
        "mb_strstr" | "mb_strrchr" | "mb_stristr" | "mb_strrichr" => {
            let v = match f {
                "mb_strstr" => StrstrVariant::Strstr,
                "mb_strrchr" => StrstrVariant::Strrchr,
                "mb_stristr" => StrstrVariant::Stristr,
                _ => StrstrVariant::Strrichr,
            };
            r!(
                st.mb_strstr_variant(v, s(&a[0]), s(&a[1]), b(&a[2]), os(a.get(3))),
                bytes_or_false
            )
        }
        "mb_substr_count" => r!(st.mb_substr_count(s(&a[0]), s(&a[1]), os(a.get(2))), V::I),
        "mb_substitute_character" => match a.first() {
            None => match st.mb_substitute_character_get() {
                SubstituteChar::None => V::S(b"none".to_vec()),
                SubstituteChar::Long => V::S(b"long".to_vec()),
                SubstituteChar::Entity => V::S(b"entity".to_vec()),
                SubstituteChar::Char(c) => V::I(i64::from(c)),
            },
            Some(V::S(x)) => r!(st.mb_substitute_character_set(SubstituteArg::Str(x)), V::B),
            Some(V::I(n)) => r!(st.mb_substitute_character_set(SubstituteArg::Int(*n)), V::B),
            other => panic!("{other:?}"),
        },
        "mb_detect_encoding" => {
            let (arr, st_) = a.get(1).map_or((None, None), list_arg);
            let l = match (&arr, &st_) {
                (Some(x), _) => Some(EncodingList::Array(x)),
                (_, Some(x)) => Some(EncodingList::Str(x)),
                _ => None,
            };
            let strict = a.get(2).map(b);
            r!(
                st.mb_detect_encoding(s(&a[0]), l, strict, false),
                |x: Option<&str>| x.map_or(V::B(false), |n| V::S(n.as_bytes().to_vec()))
            )
        }
        "mb_encode_numericentity" | "mb_decode_numericentity" => {
            let map: Vec<Option<i64>> = values(&a[1])
                .iter()
                .map(|x| match x {
                    V::I(n) => Some(*n),
                    V::S(t) => std::str::from_utf8(t).ok().and_then(|t| t.parse().ok()),
                    V::B(b) => Some(i64::from(*b)),
                    V::N => Some(0),
                    _ => None,
                })
                .collect();
            if f == "mb_encode_numericentity" {
                r!(
                    st.mb_encode_numericentity(
                        s(&a[0]),
                        &map,
                        os(a.get(2)),
                        a.get(3).is_some_and(b)
                    ),
                    V::S
                )
            } else {
                r!(
                    st.mb_decode_numericentity(s(&a[0]), &map, os(a.get(2))),
                    V::S
                )
            }
        }
        "mb_encode_mimeheader" => r!(
            st.mb_encode_mimeheader(
                s(&a[0]),
                os(a.get(1)),
                os(a.get(2)),
                a.get(3).map_or(&b"\r\n"[..], s),
                a.get(4).map_or(0, i)
            ),
            V::S
        ),
        "mb_decode_mimeheader" => V::S(st.mb_decode_mimeheader(s(&a[0]))),
        "mb_str_pad" => r!(
            st.mb_str_pad(s(&a[0]), i(&a[1]), s(&a[2]), i(&a[3]), os(a.get(4))),
            V::S
        ),
        "mb_convert_kana" => r!(
            st.mb_convert_kana(s(&a[0]), os(a.get(1)), os(a.get(2))),
            V::S
        ),
        "mb_preferred_mime_name" => r!(st.mb_preferred_mime_name(s(&a[0])), |x: Option<&str>| x
            .map_or(V::B(false), |n| V::S(n.as_bytes().to_vec()))),
        "mb_encoding_aliases" => r!(st.mb_encoding_aliases(s(&a[0])), |x: Vec<&str>| V::A(
            x.into_iter().map(|n| V::S(n.as_bytes().to_vec())).collect()
        )),
        "mb_internal_encoding" => match a.first() {
            None => V::S(st.mb_internal_encoding_get().as_bytes().to_vec()),
            Some(x) => r!(st.mb_internal_encoding_set(s(x)), V::B),
        },
        "mb_language" => match a.first() {
            None => V::S(st.mb_language_get().as_bytes().to_vec()),
            Some(x) => r!(st.mb_language_set(s(x)), V::B),
        },
        "mb_detect_order" => match a.first() {
            None => V::A(
                st.mb_detect_order_get()
                    .into_iter()
                    .map(|n| V::S(n.as_bytes().to_vec()))
                    .collect(),
            ),
            Some(x) => {
                let (arr, st_) = list_arg(x);
                let l = match (&arr, &st_) {
                    (Some(x), _) => EncodingList::Array(x),
                    (_, Some(x)) => EncodingList::Str(x),
                    _ => unreachable!(),
                };
                r!(st.mb_detect_order_set(l), V::B)
            }
        },
        "mb_http_output" => match a.first() {
            None => V::S(st.mb_http_output_get().as_bytes().to_vec()),
            Some(x) => r!(st.mb_http_output_set(s(x)), V::B),
        },
        "mb_http_input" => r!(st.mb_http_input(os(a.first())), |h| match h {
            HttpInput::False => V::B(false),
            HttpInput::Name(n) => V::S(n.into_bytes()),
            HttpInput::List(l) =>
                V::A(l.into_iter().map(|n| V::S(n.as_bytes().to_vec())).collect()),
        }),
        "mb_regex_encoding" => match a.first() {
            None => V::S(st.mb_regex_encoding_get().as_bytes().to_vec()),
            Some(x) => r!(st.mb_regex_encoding_set(s(x)), V::B),
        },
        "mb_get_info" => info_to_v(st.mb_get_info(os(a.first()))),
        "mb_list_encodings" => V::A(
            php_mbstring::state::mb_list_encodings()
                .into_iter()
                .map(|n| V::S(n.as_bytes().to_vec()))
                .collect(),
        ),
        _ => panic!("unknown function {f}"),
    }
}

/// Split a bracketed array body at top-level commas.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0usize);
    for (i, c) in s.char_indices() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn parse_keyed(body: &str) -> V {
    V::K(
        split_top(body)
            .into_iter()
            .map(|item| {
                let (k, v) = item.split_once('=').unwrap();
                (parse(k), parse(v))
            })
            .collect(),
    )
}

/// Values of an array (either form).
pub fn values(v: &V) -> Vec<V> {
    match v {
        V::A(x) => x.clone(),
        V::K(x) => x.iter().map(|(_, v)| v.clone()).collect(),
        _ => vec![],
    }
}

fn info_to_v(v: php_mbstring::info::InfoValue) -> V {
    use php_mbstring::info::InfoValue as I;
    match v {
        I::Str(s) => V::S(s.into_bytes()),
        I::Int(n) => V::I(n),
        I::List(l) => V::A(l.into_iter().map(|n| V::S(n.as_bytes().to_vec())).collect()),
        I::Assoc(a) => V::K(
            a.into_iter()
                .map(|(k, v)| (V::S(k.as_bytes().to_vec()), info_to_v(v)))
                .collect(),
        ),
        I::Null => V::N,
        I::False => V::B(false),
    }
}
