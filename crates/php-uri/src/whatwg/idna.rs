// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! UTS #46 processing as implemented by lexbor (`lexbor/unicode/idna.c`):
//! mapping, NFC normalization, label splitting, Punycode and the (partial)
//! validity criteria lexbor applies.

use super::unicode_tables::{
    CCC_RANGES, COMPOSITIONS, DECOMP_CPS, DECOMP_INDEX, IDNA_DEVIATION, IDNA_DISALLOWED,
    IDNA_ENTRIES, IDNA_IGNORED, IDNA_MAP, IDNA_MAPPED, IDNA_RANGES, IDNA_VALID,
};
use super::utf8::{decode_valid_utf8_single, encode_utf8};

/// Marker for an IDNA failure (lexbor reports no detail).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdnaError;

/// Room for `n` more elements in `v`, checked against `memory_limit`
/// (the input sizes these buffers): past it the violation is recorded and
/// processing stops (the caller raises the fatal).
fn grow<T>(v: &mut Vec<T>, n: usize) -> Result<(), IdnaError> {
    php_memlimit::TryGrow::try_grow(v, n).map_err(|_| IdnaError)
}

/// A buffer for `n` elements, checked as [`grow`].
fn buffer<T>(n: usize) -> Result<Vec<T>, IdnaError> {
    let mut v = Vec::new();
    php_memlimit::TryGrow::try_grow_exact(&mut v, n).map_err(|_| IdnaError)?;
    Ok(v)
}

/// The UTS #46 status of a code point and its mapping.
fn idna_entry(cp: u32) -> (u8, &'static [u32]) {
    if cp > 0x10_FFFE {
        return (IDNA_DISALLOWED, &[]);
    }
    let idx = match IDNA_RANGES.binary_search_by(|&(start, _)| start.cmp(&cp)) {
        Ok(i) => i,
        Err(i) => i - 1,
    };
    let entry = usize::from(IDNA_RANGES[idx].1);
    if entry == 0 {
        return (IDNA_DISALLOWED, &[]);
    }
    let (status, len, off) = IDNA_ENTRIES[entry];
    let map = if len > 0 {
        let off = usize::from(off);
        &IDNA_MAP[off..off + usize::from(len)]
    } else {
        &[]
    };
    (status, map)
}

fn ccc(cp: u32) -> u8 {
    match CCC_RANGES.binary_search_by(|&(a, b, _)| {
        if cp < a {
            std::cmp::Ordering::Greater
        } else if cp > b {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(i) => CCC_RANGES[i].2,
        Err(_) => 0,
    }
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

fn decompose_into(cp: u32, out: &mut Vec<u32>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let s = cp - S_BASE;
        out.push(L_BASE + s / N_COUNT);
        out.push(V_BASE + (s % N_COUNT) / T_COUNT);
        let t = s % T_COUNT;
        if t != 0 {
            out.push(T_BASE + t);
        }
        return;
    }
    if let Ok(i) = DECOMP_INDEX.binary_search_by(|&(c, _, _)| c.cmp(&cp)) {
        let (_, off, len) = DECOMP_INDEX[i];
        let off = usize::from(off);
        out.extend_from_slice(&DECOMP_CPS[off..off + usize::from(len)]);
    } else {
        out.push(cp);
    }
}

fn compose_pair(a: u32, b: u32) -> Option<u32> {
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return Some(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE) % T_COUNT == 0
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return Some(a + (b - T_BASE));
    }
    COMPOSITIONS
        .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
        .ok()
        .map(|i| COMPOSITIONS[i].2)
}

/// Normalization Form C.
///
/// # Errors
/// When a buffer would cross `memory_limit`.
pub fn nfc(input: &[u32]) -> Result<Vec<u32>, IdnaError> {
    if input.iter().all(|&c| c < 0x300) {
        let mut out = buffer(input.len())?;
        out.extend_from_slice(input);
        return Ok(out);
    }
    // Canonical decomposition (at most four code points each).
    let mut d = buffer(input.len())?;
    for &cp in input {
        grow(&mut d, 4)?;
        decompose_into(cp, &mut d);
    }
    // Canonical ordering (stable sort of non-starter runs).
    let mut pairs: Vec<(u32, u8)> = buffer(d.len())?;
    pairs.extend(d.into_iter().map(|c| (c, ccc(c))));
    let mut i = 0;
    while i < pairs.len() {
        if pairs[i].1 == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < pairs.len() && pairs[i].1 != 0 {
            i += 1;
        }
        pairs[start..i].sort_by_key(|p| p.1);
    }
    // Canonical composition.
    let mut out: Vec<(u32, u8)> = buffer(pairs.len())?;
    let mut starter: Option<usize> = None;
    for (cp, cc) in pairs {
        if let Some(si) = starter {
            let last_cc = out.last().map_or(0, |p| p.1);
            let blocked = out.len() - 1 != si && (last_cc == 0 || last_cc >= cc);
            if !blocked {
                if let Some(comp) = compose_pair(out[si].0, cp) {
                    out[si] = (comp, ccc(comp));
                    continue;
                }
            }
        }
        if cc == 0 {
            starter = Some(out.len());
        }
        out.push((cp, cc));
    }
    let mut cps = buffer(out.len())?;
    cps.extend(out.into_iter().map(|p| p.0));
    Ok(cps)
}

/// lexbor's `lxb_unicode_idna_validity_criteria_cp` with the flags the URL
/// parser uses (no hyphen, bidi or joiner checks).
fn valid_label(label: &[u32]) -> bool {
    if label.len() >= 4
        && matches!(label[0], 0x78 | 0x58)
        && matches!(label[1], 0x6E | 0x4E)
        && label[2] == 0x2D
        && label[3] == 0x2D
    {
        return false;
    }
    label.iter().all(|&cp| {
        cp != 0x2E && matches!(idna_entry(cp).0, s if s == IDNA_VALID || s == IDNA_DEVIATION)
    })
}

/// The mapping + normalization steps of UTS #46 processing.
fn process(input: &[u8]) -> Result<Vec<u32>, IdnaError> {
    let mut mapped = buffer(input.len())?;
    let mut p = 0;
    while p < input.len() {
        let cp = decode_valid_utf8_single(input, &mut p);
        if cp > 0x10_FFFF {
            return Err(IdnaError);
        }
        let (status, map) = idna_entry(cp);
        grow(&mut mapped, map.len().max(1))?;
        match status {
            s if s == IDNA_IGNORED => {}
            s if s == IDNA_MAPPED => mapped.extend_from_slice(map),
            _ => mapped.push(cp),
        }
    }
    nfc(&mapped)
}

/// Splits processed code points into labels the way lexbor's
/// `lxb_unicode_idna_norm_c_cb` does and runs `per_label` on each label
/// (after Punycode decoding of `xn--` labels and validation).
fn for_each_label(
    cps: &[u32],
    mut per_label: impl FnMut(&[u32]) -> Result<(), IdnaError>,
) -> Result<(), IdnaError> {
    let mut send = |label: &[u32]| -> Result<(), IdnaError> {
        if label.len() >= 4
            && matches!(label[0], 0x78 | 0x58)
            && matches!(label[1], 0x6E | 0x4E)
            && label[2] == 0x2D
            && label[3] == 0x2D
        {
            if let Ok(decoded) = punycode_decode(&label[4..]) {
                if valid_label(&decoded) {
                    return per_label(&decoded);
                }
            }
        }
        if !valid_label(label) {
            return Err(IdnaError);
        }
        per_label(label)
    };
    let mut start = 0;
    for (i, &cp) in cps.iter().enumerate() {
        if cp == 0x2E {
            send(&cps[start..i])?;
            start = i + 1;
        }
    }
    if cps.len() > start || cps.last() == Some(&0x2E) {
        send(&cps[start..])?;
    }
    Ok(())
}

/// lexbor's `lxb_unicode_idna_to_ascii` (UTF-8 in, ASCII out).
pub fn to_ascii(input: &[u8]) -> Result<Vec<u8>, IdnaError> {
    let cps = process(input)?;
    let mut out = Vec::new();
    for_each_label(&cps, |label| {
        let (encoded, unchanged) = punycode_encode(label)?;
        if !unchanged {
            out.extend_from_slice(b"xn--");
        }
        out.extend_from_slice(&encoded);
        out.push(b'.');
        Ok(())
    })?;
    out.pop();
    Ok(out)
}

/// lexbor's `lxb_unicode_idna_to_unicode` (UTF-8 in, UTF-8 out).
pub fn to_unicode(input: &[u8]) -> Result<Vec<u8>, IdnaError> {
    let cps = process(input)?;
    let mut out = Vec::new();
    for_each_label(&cps, |label| {
        for &cp in label {
            if cp > 0x10_FFFF {
                return Err(IdnaError);
            }
            encode_utf8(cp, &mut out);
        }
        out.push(b'.');
        Ok(())
    })?;
    out.pop();
    Ok(out)
}

// ---- Punycode (RFC 3492, lexbor/punycode/punycode.c) ----

const BASE: u32 = 36;
const TMIN: u32 = 1;
const TMAX: u32 = 26;
const SKEW: u32 = 38;
const DAMP: u32 = 700;
const INITIAL_BIAS: u32 = 72;
const INITIAL_N: u32 = 0x80;

// The Punycode routines keep RFC 3492's variable names (n, i, h, b, k, t, q, w).
fn adapt(delta: u32, numpoints: u32, first_time: bool) -> u32 {
    let mut delta = if first_time { delta / DAMP } else { delta >> 1 };
    delta += delta / numpoints;
    let mut k = 0;
    while delta > ((BASE - TMIN) * TMAX) / 2 {
        delta /= BASE - TMIN;
        k += BASE;
    }
    k + (BASE - TMIN + 1) * delta / (delta + SKEW)
}

fn encode_digit(d: u32) -> u8 {
    // 0..25 -> 'a'..'z', 26..35 -> '0'..'9'
    let v = if d < 26 { d + 97 } else { d + 22 };
    u8::try_from(v).unwrap_or(b'?')
}

fn decode_digit(cp: u32) -> u32 {
    if cp.wrapping_sub(48) < 10 {
        cp - 22
    } else if cp.wrapping_sub(65) < 26 {
        cp - 65
    } else if cp.wrapping_sub(97) < 26 {
        cp - 97
    } else {
        BASE
    }
}

/// Encodes a label; returns the encoded bytes and whether the label was all
/// ASCII (in which case no `xn--` prefix is needed).
#[allow(clippy::many_single_char_names)]
pub fn punycode_encode(cps: &[u32]) -> Result<(Vec<u8>, bool), IdnaError> {
    let mut out: Vec<u8> = cps
        .iter()
        .filter(|&&c| c < 0x80)
        .map(|&c| u8::try_from(c).unwrap_or(b'?'))
        .collect();
    let b = out.len();
    if b >= cps.len() {
        return Ok((out, true));
    }
    if b > 0 {
        grow(&mut out, 1)?;
        out.push(b'-');
    }
    let mut n = INITIAL_N;
    let mut bias = INITIAL_BIAS;
    let mut delta: u32 = 0;
    let mut h = b;
    while h < cps.len() {
        let m = cps
            .iter()
            .copied()
            .filter(|&c| c >= n)
            .min()
            .unwrap_or(u32::MAX);
        let hp1 = u32::try_from(h + 1).map_err(|_| IdnaError)?;
        if m - n > (u32::MAX - delta) / hp1 {
            return Err(IdnaError);
        }
        delta += (m - n) * hp1;
        n = m;
        for &c in cps {
            if c < n {
                delta = delta.checked_add(1).ok_or(IdnaError)?;
            }
            if c == n {
                let mut q = delta;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    grow(&mut out, 1)?;
                    out.push(encode_digit(t + (q - t) % (BASE - t)));
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                grow(&mut out, 1)?;
                out.push(encode_digit(q));
                let hh = u32::try_from(h).map_err(|_| IdnaError)?;
                bias = adapt(delta, hh + 1, h == b);
                delta = 0;
                h += 1;
            }
        }
        delta = delta.wrapping_add(1);
        n = n.wrapping_add(1);
    }
    Ok((out, false))
}

/// Decodes the part of a label after `xn--`.
#[allow(clippy::many_single_char_names)]
pub fn punycode_decode(input: &[u32]) -> Result<Vec<u32>, IdnaError> {
    let delimiter = input.iter().rposition(|&c| c == 0x2D);
    let mut out: Vec<u32> = Vec::new();
    let mut pos = 0;
    if let Some(d) = delimiter.filter(|&d| d > 0) {
        for &c in &input[..d] {
            if c >= 0x80 {
                return Err(IdnaError);
            }
            grow(&mut out, 1)?;
            out.push(c);
        }
        pos = d + 1;
    }
    let mut i: u32 = 0;
    let mut n = INITIAL_N;
    let mut bias = INITIAL_BIAS;
    while pos < input.len() {
        let oldi = i;
        let mut w: u32 = 1;
        let mut k = BASE;
        loop {
            let Some(&c) = input.get(pos) else {
                return Err(IdnaError);
            };
            pos += 1;
            let digit = decode_digit(c);
            if digit >= BASE {
                return Err(IdnaError);
            }
            if digit > (u32::MAX - i) / w {
                return Err(IdnaError);
            }
            i += digit * w;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            if w > u32::MAX / (BASE - t) {
                return Err(IdnaError);
            }
            w *= BASE - t;
            k += BASE;
        }
        let h = u32::try_from(out.len() + 1).map_err(|_| IdnaError)?;
        bias = adapt(i - oldi, h, oldi == 0);
        if i / h > u32::MAX - n {
            return Err(IdnaError);
        }
        n += i / h;
        i %= h;
        grow(&mut out, 1)?;
        out.insert(i as usize, n);
        i += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Result<String, IdnaError> {
        to_ascii(s.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    fn u(s: &str) -> Result<String, IdnaError> {
        to_unicode(s.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn ascii() {
        assert_eq!(a("Example.COM").unwrap(), "example.com");
        assert_eq!(a("bücher.de").unwrap(), "xn--bcher-kva.de");
        assert_eq!(a("faß.de").unwrap(), "xn--fa-hia.de");
        assert_eq!(a("例え.テスト").unwrap(), "xn--r8jz45g.xn--zckzah");
        assert_eq!(a("a.").unwrap(), "a.");
        assert_eq!(a(".").unwrap(), ".");
        assert_eq!(a("").unwrap(), "");
        assert_eq!(a("xn--bcher-kva.de").unwrap(), "xn--bcher-kva.de");
        assert_eq!(a("ＥＸＡＭＰＬＥ。com").unwrap(), "example.com");
        assert!(a("xn--a").is_err());
        assert!(a("a\u{FFFF}").is_err());
    }

    #[test]
    fn unicode() {
        assert_eq!(u("xn--bcher-kva.de").unwrap(), "bücher.de");
        assert_eq!(u("example.com").unwrap(), "example.com");
        assert_eq!(u("xn--r8jz45g").unwrap(), "例え");
    }

    #[test]
    fn nfc_composes() {
        assert_eq!(nfc(&[0x61, 0x308]).unwrap(), vec![0xE4]);
        assert_eq!(nfc(&[0x1100, 0x1161, 0x11A8]).unwrap(), vec![0xAC01]);
        assert_eq!(nfc(&[0x212B]).unwrap(), vec![0xC5]);
        assert_eq!(nfc(&[0x61, 0x323, 0x302]).unwrap(), vec![0x1EAD]);
        assert_eq!(nfc(&[0x61, 0x302, 0x323]).unwrap(), vec![0x1EAD]);
    }

    #[test]
    fn punycode_round_trip() {
        let s: Vec<u32> = "ü".chars().map(u32::from).collect();
        let (e, unchanged) = punycode_encode(&s).unwrap();
        assert!(!unchanged);
        assert_eq!(e, b"tda");
        let d = punycode_decode(&e.iter().map(|&b| u32::from(b)).collect::<Vec<_>>()).unwrap();
        assert_eq!(d, s);
    }
}
