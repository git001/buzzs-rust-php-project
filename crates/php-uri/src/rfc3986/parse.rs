// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! A port of uriparser's recursive-descent RFC 3986 parser
//! (`UriParse.c`), preserving its acceptance rules and component splitting.

use super::{
    hex_val, is_alpha, is_digit, is_hex, is_pchar_wo_pct, is_sub_delim, is_unreserved, Host,
    HostKind, Uri,
};

/// Marker for a syntax error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntaxError;

type R = Result<usize, SyntaxError>;

struct Parser<'a> {
    s: &'a [u8],
    uri: Uri,
    /// Start of scheme (or of the first segment, uriparser abuses the
    /// scheme pointer for this).
    scheme_first: Option<usize>,
    user_info_first: Option<usize>,
    host_first: Option<usize>,
    host_after: Option<usize>,
    port_first: Option<usize>,
    ip6: Option<[u8; 16]>,
    ip_future: bool,
}

/// Parses a complete URI reference (`uriParseSingleUriExMmA`).
pub fn parse(s: &[u8]) -> Result<Uri, SyntaxError> {
    let mut p = Parser {
        s,
        uri: Uri::default(),
        scheme_first: None,
        user_info_first: None,
        host_first: None,
        host_after: None,
        port_first: None,
        ip6: None,
        ip_future: false,
    };
    let after = p.uri_reference(0)?;
    if after != s.len() {
        return Err(SyntaxError);
    }
    Ok(p.uri)
}

/// Parses the text of an IPv6 address (without brackets), returning its
/// sixteen bytes, as `uriParseIpSixAddressMmA` does.
pub fn parse_ipv6(text: &[u8]) -> Option<[u8; 16]> {
    if matches!(text.first(), Some(b'v' | b'V')) {
        return None;
    }
    if text.len() > 8 * 4 + 7 {
        return None;
    }
    let mut candidate = b"//[".to_vec();
    candidate.extend_from_slice(text);
    candidate.push(b']');
    let uri = parse(&candidate).ok()?;
    match uri.host?.kind {
        HostKind::Ipv6(b) => Some(b),
        _ => None,
    }
}

/// Whether `text` is a dotted-decimal IPv4 address (`uriIsWellFormedHostIp4A`).
pub fn is_well_formed_ipv4(text: &[u8]) -> bool {
    parse_ipv4(text).is_some()
}

/// `ParseIpFourAddress`: strict dotted decimal with dec-octets.
pub(crate) fn parse_ipv4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut parts = text.split(|&c| c == b'.');
    for slot in &mut out {
        let part = parts.next()?;
        *slot = parse_dec_octet(part)?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

fn parse_dec_octet(part: &[u8]) -> Option<u8> {
    if part.is_empty() || part.len() > 3 || !part.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if part.len() > 1 && part[0] == b'0' {
        return None;
    }
    let v: u32 = part
        .iter()
        .fold(0, |acc, &c| acc * 10 + u32::from(c - b'0'));
    u8::try_from(v).ok()
}

fn octet_value(digits: &[u8]) -> u8 {
    let v: u32 = digits.iter().fold(0, |acc, &d| acc * 10 + u32::from(d));
    // Range checked by the caller.
    u8::try_from(v).unwrap_or(u8::MAX)
}

fn write_quad(digits: &[u8], out: &mut [u8]) {
    let mut v: u16 = 0;
    for &d in digits {
        v = (v << 4) | u16::from(d);
    }
    out[0] = (v >> 8) as u8;
    out[1] = (v & 0xff) as u8;
}

impl Parser<'_> {
    fn at(&self, i: usize) -> Option<u8> {
        self.s.get(i).copied()
    }

    /// Keeps a path segment. Past `memory_limit` (one segment per `/`, so
    /// the input sizes the list) the segment is dropped and the violation
    /// stays recorded: the caller raises the fatal instead of using the URI.
    fn push_segment(&mut self, first: usize, after: usize) {
        if php_memlimit::TryGrow::try_grow(&mut self.uri.segments, 1).is_ok() {
            self.uri.segments.push(self.s[first..after].to_vec());
        }
    }

    fn pct_encoded(&self, i: usize) -> R {
        // s[i] == '%'
        match (self.at(i + 1), self.at(i + 2)) {
            (Some(a), Some(b)) if is_hex(a) && is_hex(b) => Ok(i + 3),
            _ => Err(SyntaxError),
        }
    }

    fn pchar(&self, i: usize) -> R {
        match self.at(i) {
            Some(b'%') => self.pct_encoded(i),
            Some(c) if is_pchar_wo_pct(c) => Ok(i + 1),
            _ => Err(SyntaxError),
        }
    }

    fn uri_reference(&mut self, i: usize) -> R {
        let Some(c) = self.at(i) else {
            return Ok(i);
        };
        match c {
            c if is_alpha(c) => {
                self.scheme_first = Some(i);
                self.segment_nz_nc_or_scheme2(i + 1)
            }
            c if is_digit(c)
                || is_sub_delim(c)
                || matches!(c, b'.' | b'_' | b'~' | b'-' | b'@') =>
            {
                self.scheme_first = Some(i);
                self.must_be_segment_nz_nc(i + 1)
            }
            b'%' => {
                let after = self.pct_encoded(i)?;
                self.scheme_first = Some(i);
                self.must_be_segment_nz_nc(after)
            }
            b'/' => {
                let after = self.part_helper_two(i + 1)?;
                self.uri_tail(after)
            }
            _ => self.uri_tail(i),
        }
    }

    fn take_first_segment(&mut self, after: usize) {
        let first = self.scheme_first.take().unwrap_or(after);
        self.push_segment(first, after);
    }

    fn segment_nz_nc_or_scheme2(&mut self, mut i: usize) -> R {
        loop {
            let Some(c) = self.at(i) else {
                self.take_first_segment(i);
                return Ok(i);
            };
            match c {
                b'.' | b'+' | b'-' => i += 1,
                c if c.is_ascii_alphanumeric() => i += 1,
                b'%' => {
                    let after = self.pct_encoded(i)?;
                    return self.must_be_segment_nz_nc(after);
                }
                b'!' | b'$' | b'&' | b'(' | b')' | b'*' | b',' | b';' | b'@' | b'_' | b'~'
                | b'=' | b'\'' => return self.must_be_segment_nz_nc(i + 1),
                b'/' => {
                    let after_seg = self.segment(i + 1)?;
                    self.take_first_segment(i);
                    self.push_segment(i + 1, after_seg);
                    let after = self.zero_more_slash_segs(after_seg)?;
                    return self.uri_tail(after);
                }
                b':' => {
                    let after_hier = self.hier_part(i + 1)?;
                    let first = self.scheme_first.take().unwrap_or(0);
                    self.uri.scheme = Some(self.s[first..i].to_vec());
                    return self.uri_tail(after_hier);
                }
                _ => {
                    self.take_first_segment(i);
                    return self.uri_tail(i);
                }
            }
        }
    }

    fn must_be_segment_nz_nc(&mut self, mut i: usize) -> R {
        loop {
            let Some(c) = self.at(i) else {
                self.take_first_segment(i);
                return Ok(i);
            };
            match c {
                b'%' => i = self.pct_encoded(i)?,
                b'@' => i += 1,
                c if is_sub_delim(c) || is_unreserved(c) => i += 1,
                b'/' => {
                    self.take_first_segment(i);
                    let after_seg = self.segment(i + 1)?;
                    self.push_segment(i + 1, after_seg);
                    let after = self.zero_more_slash_segs(after_seg)?;
                    return self.uri_tail(after);
                }
                _ => {
                    self.take_first_segment(i);
                    return self.uri_tail(i);
                }
            }
        }
    }

    fn hier_part(&mut self, i: usize) -> R {
        match self.at(i) {
            Some(b'/') => self.part_helper_two(i + 1),
            Some(c) if c == b'%' || is_pchar_wo_pct(c) => self.path_rootless(i),
            _ => Ok(i),
        }
    }

    fn path_rootless(&mut self, i: usize) -> R {
        let after = self.segment_nz(i)?;
        self.push_segment(i, after);
        self.zero_more_slash_segs(after)
    }

    fn part_helper_two(&mut self, i: usize) -> R {
        match self.at(i) {
            None => {
                self.uri.absolute_path = true;
                Ok(i)
            }
            Some(b'/') => {
                let after_authority = self.authority(i + 1)?;
                let after = self.path_abs_empty(after_authority)?;
                self.uri.fix_empty_trail_segment();
                Ok(after)
            }
            Some(_) => {
                self.uri.absolute_path = true;
                self.path_abs_no_lead_slash(i)
            }
        }
    }

    fn path_abs_empty(&mut self, mut i: usize) -> R {
        while self.at(i) == Some(b'/') {
            let after = self.segment(i + 1)?;
            self.push_segment(i + 1, after);
            i = after;
        }
        Ok(i)
    }

    fn path_abs_no_lead_slash(&mut self, i: usize) -> R {
        match self.at(i) {
            Some(c) if c == b'%' || is_pchar_wo_pct(c) => {
                let after = self.segment_nz(i)?;
                self.push_segment(i, after);
                self.zero_more_slash_segs(after)
            }
            _ => Ok(i),
        }
    }

    fn segment(&self, mut i: usize) -> R {
        while let Some(c) = self.at(i) {
            if c == b'%' || is_pchar_wo_pct(c) {
                i = self.pchar(i)?;
            } else {
                break;
            }
        }
        Ok(i)
    }

    fn segment_nz(&self, i: usize) -> R {
        let after = self.pchar(i)?;
        self.segment(after)
    }

    fn zero_more_slash_segs(&mut self, mut i: usize) -> R {
        while self.at(i) == Some(b'/') {
            let after = self.segment(i + 1)?;
            self.push_segment(i + 1, after);
            i = after;
        }
        Ok(i)
    }

    fn query_frag(&self, mut i: usize) -> R {
        while let Some(c) = self.at(i) {
            if c == b'%' || is_pchar_wo_pct(c) {
                i = self.pchar(i)?;
            } else if c == b'/' || c == b'?' {
                i += 1;
            } else {
                break;
            }
        }
        Ok(i)
    }

    fn uri_tail(&mut self, i: usize) -> R {
        match self.at(i) {
            Some(b'#') => {
                let after = self.query_frag(i + 1)?;
                self.uri.fragment = Some(self.s[i + 1..after].to_vec());
                Ok(after)
            }
            Some(b'?') => {
                let after = self.query_frag(i + 1)?;
                self.uri.query = Some(self.s[i + 1..after].to_vec());
                if self.at(after) == Some(b'#') {
                    let after2 = self.query_frag(after + 1)?;
                    self.uri.fragment = Some(self.s[after + 1..after2].to_vec());
                    return Ok(after2);
                }
                Ok(after)
            }
            _ => Ok(i),
        }
    }

    // ---- authority ----

    fn set_host(&mut self, first: usize, after: usize) {
        let text = self.s[first..after].to_vec();
        let kind = if let Some(b) = self.ip6.take() {
            HostKind::Ipv6(b)
        } else if self.ip_future {
            HostKind::IpFuture
        } else {
            HostKind::RegName
        };
        self.uri.host = Some(Host { text, kind });
    }

    /// Sets a reg-name/IPv4 host, detecting IPv4.
    fn set_host_detect_ipv4(&mut self, first: usize, after: usize) {
        let text = self.s[first..after].to_vec();
        let kind = parse_ipv4(&text).map_or(HostKind::RegName, HostKind::Ipv4);
        self.uri.host = Some(Host { text, kind });
    }

    fn set_empty_host(&mut self) {
        self.uri.host = Some(Host {
            text: Vec::new(),
            kind: HostKind::RegName,
        });
    }

    fn authority(&mut self, i: usize) -> R {
        match self.at(i) {
            Some(b'[') => {
                let after = self.ip_lit2(i + 1)?;
                let host_after = self.host_after.unwrap_or(after);
                self.set_host(i + 1, host_after);
                Ok(self.authority_two(after))
            }
            Some(c) if c == b'%' || is_pchar_wo_pct(c) => {
                self.user_info_first = Some(i);
                self.own_host_user_info_nz(i)
            }
            _ => {
                self.set_empty_host();
                Ok(i)
            }
        }
    }

    fn authority_two(&mut self, i: usize) -> usize {
        if self.at(i) == Some(b':') {
            let mut j = i + 1;
            while self.at(j).is_some_and(is_digit) {
                j += 1;
            }
            self.uri.port = Some(self.s[i + 1..j].to_vec());
            return j;
        }
        i
    }

    fn pct_sub_unres(&self, i: usize) -> R {
        match self.at(i) {
            Some(b'%') => self.pct_encoded(i),
            Some(c) if is_sub_delim(c) || is_unreserved(c) => Ok(i + 1),
            _ => Err(SyntaxError),
        }
    }

    fn own_host_user_info_nz(&mut self, mut i: usize) -> R {
        let orig = i;
        while let Some(c) = self.at(i) {
            if c == b'%' || is_sub_delim(c) || is_unreserved(c) {
                i = self.pct_sub_unres(i)?;
            } else {
                break;
            }
        }
        match self.at(i) {
            Some(b':') => {
                self.host_after = Some(i);
                self.port_first = Some(i + 1);
                return self.own_port_user_info(i + 1);
            }
            Some(b'@') => {
                let first = self.user_info_first.take().unwrap_or(orig);
                self.uri.user_info = Some(self.s[first..i].to_vec());
                self.host_first = Some(i + 1);
                return self.own_host(i + 1);
            }
            _ => {}
        }
        if i == orig {
            return Err(SyntaxError);
        }
        let first = self.user_info_first.take().unwrap_or(orig);
        self.set_host_detect_ipv4(first, i);
        Ok(i)
    }

    fn exit_own_port_user_info(&mut self, i: usize) {
        let first = self.user_info_first.take().unwrap_or(0);
        let host_after = self.host_after.take().unwrap_or(first);
        let port_first = self.port_first.take().unwrap_or(i);
        self.set_host_detect_ipv4(first, host_after);
        self.uri.port = Some(self.s[port_first..i].to_vec());
    }

    fn own_port_user_info(&mut self, mut i: usize) -> R {
        loop {
            let Some(c) = self.at(i) else {
                self.exit_own_port_user_info(i);
                return Ok(i);
            };
            match c {
                c if is_digit(c) => i += 1,
                c if is_sub_delim(c)
                    || matches!(c, b'-' | b'.' | b'_' | b'~' | b':')
                    || is_alpha(c) =>
                {
                    self.host_after = None;
                    self.port_first = None;
                    return self.own_user_info(i + 1);
                }
                b'%' => {
                    self.port_first = None;
                    let after = self.pct_encoded(i)?;
                    return self.own_user_info(after);
                }
                b'@' => {
                    self.host_after = None;
                    self.port_first = None;
                    let first = self.user_info_first.take().unwrap_or(0);
                    self.uri.user_info = Some(self.s[first..i].to_vec());
                    self.host_first = Some(i + 1);
                    return self.own_host(i + 1);
                }
                _ => {
                    self.exit_own_port_user_info(i);
                    return Ok(i);
                }
            }
        }
    }

    fn own_user_info(&mut self, mut i: usize) -> R {
        loop {
            let Some(c) = self.at(i) else {
                return Err(SyntaxError);
            };
            match c {
                b'%' => i = self.pct_encoded(i)?,
                c if is_sub_delim(c) || is_unreserved(c) || c == b':' => i += 1,
                b'@' => {
                    let first = self.user_info_first.take().unwrap_or(0);
                    self.uri.user_info = Some(self.s[first..i].to_vec());
                    self.host_first = Some(i + 1);
                    return self.own_host(i + 1);
                }
                _ => return Err(SyntaxError),
            }
        }
    }

    fn own_host(&mut self, i: usize) -> R {
        match self.at(i) {
            None => {
                self.set_empty_host();
                Ok(i)
            }
            Some(b'[') => {
                self.host_after = None;
                let after = self.ip_lit2(i + 1)?;
                let host_after = self.host_after.unwrap_or(after);
                self.set_host(i + 1, host_after);
                Ok(self.authority_two(after))
            }
            Some(_) => self.own_host2(i),
        }
    }

    fn own_host2(&mut self, mut i: usize) -> R {
        let first = self.host_first.unwrap_or(i);
        loop {
            match self.at(i) {
                Some(c) if c == b'%' || is_sub_delim(c) || is_unreserved(c) => {
                    i = self.pct_sub_unres(i)?;
                }
                None => {
                    self.set_host_detect_ipv4(first, i);
                    return Ok(i);
                }
                Some(_) => {
                    self.set_host_detect_ipv4(first, i);
                    return Ok(self.authority_two(i));
                }
            }
        }
    }

    // ---- IP literals ----

    fn ip_lit2(&mut self, i: usize) -> R {
        match self.at(i) {
            Some(b'v' | b'V') => {
                let after = self.ip_future(i)?;
                if self.at(after) != Some(b']') {
                    return Err(SyntaxError);
                }
                Ok(after + 1)
            }
            Some(c) if c == b':' || c == b']' || is_hex(c) => self.ipv6_address2(i),
            _ => Err(SyntaxError),
        }
    }

    fn ip_future(&mut self, i: usize) -> R {
        // s[i] is 'v' or 'V'
        if !self.at(i + 1).is_some_and(is_hex) {
            return Err(SyntaxError);
        }
        let mut j = i + 2;
        while self.at(j).is_some_and(is_hex) {
            j += 1;
        }
        if self.at(j) != Some(b'.') {
            return Err(SyntaxError);
        }
        let loop_start = j + 1;
        let mut k = loop_start;
        while let Some(c) = self.at(k) {
            if c == b':' || is_sub_delim(c) || is_unreserved(c) {
                k += 1;
            } else {
                break;
            }
        }
        if k == loop_start {
            return Err(SyntaxError);
        }
        self.ip_future = true;
        self.host_after = Some(k);
        Ok(k)
    }

    #[allow(clippy::too_many_lines)]
    fn ipv6_address2(&mut self, mut i: usize) -> R {
        let mut data = [0u8; 16];
        let mut zipper_ever = false;
        let mut quads_done: usize = 0;
        let mut digit_count: usize = 0;
        let mut digit_history = [0u8; 4];
        let mut ip4_octets_done: usize = 0;
        let mut quads_after_zipper = [0u8; 14];
        let mut quads_after_zipper_count: usize = 0;

        let check_octet = |history: &[u8; 4], count: usize| -> bool {
            if count == 0 || count == 4 {
                return false;
            }
            if count > 1 && history[0] == 0 {
                return false;
            }
            if count == 3
                && 100 * u32::from(history[0]) + 10 * u32::from(history[1]) + u32::from(history[2])
                    > 255
            {
                return false;
            }
            true
        };

        loop {
            let Some(_) = self.at(i) else {
                return Err(SyntaxError);
            };
            if ip4_octets_done > 0 {
                loop {
                    let c = self.at(i).ok_or(SyntaxError)?;
                    match c {
                        b'0'..=b'9' => {
                            if digit_count == 4 {
                                return Err(SyntaxError);
                            }
                            digit_history[digit_count] = c - b'0';
                            digit_count += 1;
                        }
                        b'.' => {
                            if ip4_octets_done == 4 || !check_octet(&digit_history, digit_count) {
                                return Err(SyntaxError);
                            }
                            data[16 - 4 + ip4_octets_done] =
                                octet_value(&digit_history[..digit_count]);
                            digit_count = 0;
                            ip4_octets_done += 1;
                        }
                        b']' => {
                            if ip4_octets_done != 3 || !check_octet(&digit_history, digit_count) {
                                return Err(SyntaxError);
                            }
                            self.host_after = Some(i);
                            let n = 2 * quads_after_zipper_count;
                            data[16 - 4 - n..16 - 4].copy_from_slice(&quads_after_zipper[..n]);
                            data[16 - 4 + 3] = octet_value(&digit_history[..digit_count]);
                            self.ip6 = Some(data);
                            return Ok(i + 1);
                        }
                        _ => return Err(SyntaxError),
                    }
                    i += 1;
                    if i >= self.s.len() {
                        return Err(SyntaxError);
                    }
                }
            }
            let mut letter_among = false;
            loop {
                let c = self.at(i).ok_or(SyntaxError)?;
                let mut walking = true;
                match c {
                    b'a'..=b'f' | b'A'..=b'F' | b'0'..=b'9' => {
                        if c.is_ascii_alphabetic() {
                            letter_among = true;
                        }
                        if digit_count == 4 {
                            return Err(SyntaxError);
                        }
                        digit_history[digit_count] = hex_val(c);
                        digit_count += 1;
                    }
                    b':' => {
                        let mut set_zipper = false;
                        if digit_count > 0 {
                            if zipper_ever {
                                let off = 2 * quads_after_zipper_count;
                                if off + 2 > quads_after_zipper.len() {
                                    return Err(SyntaxError);
                                }
                                write_quad(
                                    &digit_history[..digit_count],
                                    &mut quads_after_zipper[off..off + 2],
                                );
                                quads_after_zipper_count += 1;
                            } else {
                                let off = 2 * quads_done;
                                write_quad(&digit_history[..digit_count], &mut data[off..off + 2]);
                            }
                            quads_done += 1;
                            digit_count = 0;
                        }
                        letter_among = false;
                        if quads_done >= 8 - usize::from(zipper_ever) {
                            return Err(SyntaxError);
                        }
                        if self.s.len() - i < 2 {
                            return Err(SyntaxError);
                        }
                        if self.s[i + 1] == b':' {
                            let reset_offset = 2 * quads_done;
                            i += 1;
                            if zipper_ever {
                                return Err(SyntaxError);
                            }
                            for b in &mut data[reset_offset..] {
                                *b = 0;
                            }
                            set_zipper = true;
                            if self.s.len() - i < 2 {
                                return Err(SyntaxError);
                            }
                            if self.s[i + 1] == b':' {
                                return Err(SyntaxError);
                            }
                        } else if quads_done == 0 || self.s[i + 1] == b']' {
                            return Err(SyntaxError);
                        }
                        if set_zipper {
                            zipper_ever = true;
                        }
                    }
                    b'.' => {
                        if quads_done + usize::from(zipper_ever) > 6
                            || (!zipper_ever && quads_done < 6)
                            || letter_among
                            || !check_octet(&digit_history, digit_count)
                        {
                            return Err(SyntaxError);
                        }
                        data[16 - 4] = octet_value(&digit_history[..digit_count]);
                        digit_count = 0;
                        ip4_octets_done = 1;
                        walking = false;
                    }
                    b']' => {
                        if !(zipper_ever || quads_done == 7 && digit_count > 0) {
                            return Err(SyntaxError);
                        }
                        if digit_count > 0 {
                            if zipper_ever {
                                if quads_done >= 7 {
                                    return Err(SyntaxError);
                                }
                                let off = 2 * quads_after_zipper_count;
                                write_quad(
                                    &digit_history[..digit_count],
                                    &mut quads_after_zipper[off..off + 2],
                                );
                                quads_after_zipper_count += 1;
                            } else {
                                let off = 2 * quads_done;
                                write_quad(&digit_history[..digit_count], &mut data[off..off + 2]);
                            }
                        }
                        let n = 2 * quads_after_zipper_count;
                        data[16 - n..].copy_from_slice(&quads_after_zipper[..n]);
                        self.host_after = Some(i);
                        self.ip6 = Some(data);
                        return Ok(i + 1);
                    }
                    _ => return Err(SyntaxError),
                }
                i += 1;
                if i >= self.s.len() {
                    return Err(SyntaxError);
                }
                if !walking {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> Uri {
        parse(s.as_bytes()).unwrap_or_else(|_| panic!("should parse: {s}"))
    }

    fn bad(s: &str) {
        assert!(parse(s.as_bytes()).is_err(), "should fail: {s}");
    }

    #[test]
    fn components() {
        let u = ok("https://user:pw@example.com:8080/a/b?q=1#frag");
        assert_eq!(u.scheme.as_deref(), Some(&b"https"[..]));
        assert_eq!(u.user_info.as_deref(), Some(&b"user:pw"[..]));
        assert_eq!(u.host.as_ref().unwrap().text, b"example.com");
        assert_eq!(u.port.as_deref(), Some(&b"8080"[..]));
        assert_eq!(u.segments, vec![b"a".to_vec(), b"b".to_vec()]);
        assert_eq!(u.query.as_deref(), Some(&b"q=1"[..]));
        assert_eq!(u.fragment.as_deref(), Some(&b"frag"[..]));
    }

    #[test]
    fn acceptance() {
        for s in [
            "",
            "a",
            "a:",
            "//",
            "///",
            "/",
            "?#",
            "urn:a:b",
            "http://[::]",
            "http://[1:2:3:4:5:6:7:8]",
            "http://[::ffff:1.2.3.4]",
            "http://[vF.addr]",
            "http://:80",
            "http://@host",
            "//user@:1",
            "mailto:john.doe@example.com",
            "a/b:c",
        ] {
            ok(s);
        }
        for s in [
            ":",
            ":a",
            "http://[::1",
            "http://[:::1]",
            "http://[1::2::3]",
            "http://[::01.2.3.4]",
            "http://[::256.2.3.4]",
            "http://ex ample",
            "http://a%2",
            "http://a:b:c@d:e",
            "http://ü",
            "a b",
            "http://[v.x]",
            "http://[vF.]",
            "http://a\0b",
            "1:a",
            "%61:b",
        ] {
            bad(s);
        }
    }

    #[test]
    fn hosts() {
        let u = ok("http://192.168.0.1/");
        assert_eq!(u.host.unwrap().kind, HostKind::Ipv4([192, 168, 0, 1]));
        let u = ok("http://192.168.0.01/");
        assert_eq!(u.host.unwrap().kind, HostKind::RegName);
        let u = ok("http://[2001:db8::1]/");
        let h = u.host.unwrap();
        assert_eq!(h.text, b"2001:db8::1");
        let HostKind::Ipv6(b) = h.kind else { panic!() };
        assert_eq!(b[0..2], [0x20, 0x01]);
        assert_eq!(b[14..16], [0, 1]);
        let u = ok("http://[::ffff:1.2.3.4]");
        let HostKind::Ipv6(b) = u.host.unwrap().kind else {
            panic!()
        };
        assert_eq!(b[10..16], [0xff, 0xff, 1, 2, 3, 4]);
    }

    #[test]
    fn userinfo_port_split() {
        let u = ok("//a:1");
        assert_eq!(u.host.as_ref().unwrap().text, b"a");
        assert_eq!(u.port.as_deref(), Some(&b"1"[..]));
        assert!(u.user_info.is_none());
        let u = ok("//a:1b@c");
        assert_eq!(u.user_info.as_deref(), Some(&b"a:1b"[..]));
        assert_eq!(u.host.as_ref().unwrap().text, b"c");
        let u = ok("//:@h");
        assert_eq!(u.user_info.as_deref(), Some(&b":"[..]));
    }

    #[test]
    fn ipv6_helper() {
        assert!(parse_ipv6(b"::1").is_some());
        assert!(parse_ipv6(b"v1.x").is_none());
        assert!(parse_ipv6(b"1:2").is_none());
        assert!(is_well_formed_ipv4(b"1.2.3.4"));
        assert!(!is_well_formed_ipv4(b"1.2.3"));
        assert!(!is_well_formed_ipv4(b"1.2.3.04"));
    }
}
