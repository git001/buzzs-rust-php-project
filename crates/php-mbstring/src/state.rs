// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Per-request mbstring state (`zend_mbstring_globals`) and the INI /
//! getter-setter functions built on it (`mb_language`, `mb_internal_encoding`,
//! `mb_http_input`, `mb_http_output`, `mb_detect_order`,
//! `mb_substitute_character`, `mb_preferred_mime_name`, `mb_list_encodings`,
//! `mb_encoding_aliases`).
//!
//! Ref: `ext/mbstring/mbstring.c` (INI handlers, `PHP_GINIT`, `PHP_RINIT`),
//! `libmbfl/mbfl/mbfl_language.c`, `libmbfl/nls/*.c`.

use crate::convert::ErrorMode;
use crate::encoding::{self, Encoding, EncodingNo};
use crate::error::{Diagnostic, Level, MbError, MbResult};

/// A language known to mbstring (`mbfl_language`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Language {
    /// Canonical name (returned by `mb_language()`).
    pub name: &'static str,
    /// Short name (`ja`, `en`, ...).
    pub short_name: &'static str,
    /// Aliases.
    pub aliases: &'static [&'static str],
    /// Charset used by `mb_send_mail` / default for `mb_encode_mimeheader`.
    pub mail_charset: EncodingNo,
    /// Header transfer encoding.
    pub mail_header_encoding: EncodingNo,
    /// Body transfer encoding.
    pub mail_body_encoding: EncodingNo,
    /// Default detect order for this language.
    pub detect_order: &'static [EncodingNo],
}

use EncodingNo as N;

const ORDER_JA: &[EncodingNo] = &[N::Ascii, N::Jis, N::Utf8, N::EucJp, N::Sjis];
const ORDER_CN: &[EncodingNo] = &[N::Ascii, N::Utf8, N::EucCn, N::Cp936, N::Gb18030_2022];
const ORDER_TW: &[EncodingNo] = &[N::Ascii, N::Utf8, N::EucTw, N::Big5];
const ORDER_KR: &[EncodingNo] = &[N::Ascii, N::Utf8, N::EucKr, N::Uhc];
const ORDER_RU: &[EncodingNo] = &[N::Ascii, N::Utf8, N::Koi8r, N::Cp1251, N::Cp866];
const ORDER_HY: &[EncodingNo] = &[N::Ascii, N::Utf8, N::Armscii8];
const ORDER_TR: &[EncodingNo] = &[N::Ascii, N::Utf8, N::Cp1254, N::Iso8859_9];
const ORDER_UA: &[EncodingNo] = &[N::Ascii, N::Utf8, N::Koi8u];
const ORDER_NEUT: &[EncodingNo] = &[N::Ascii, N::Utf8];

/// `mbfl_language_ptr_table`, in lookup order.
pub static LANGUAGES: [Language; 12] = [
    Language {
        name: "uni",
        short_name: "uni",
        aliases: &["universal"],
        mail_charset: N::Utf8,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::Base64,
        detect_order: ORDER_NEUT,
    },
    Language {
        name: "Japanese",
        short_name: "ja",
        aliases: &[],
        mail_charset: N::Iso2022Jp,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::SevenBit,
        detect_order: ORDER_JA,
    },
    Language {
        name: "Korean",
        short_name: "ko",
        aliases: &[],
        mail_charset: N::Iso2022Kr,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::SevenBit,
        detect_order: ORDER_KR,
    },
    Language {
        name: "Simplified Chinese",
        short_name: "zh-cn",
        aliases: &[],
        mail_charset: N::Hz,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::SevenBit,
        detect_order: ORDER_CN,
    },
    Language {
        name: "Traditional Chinese",
        short_name: "zh-tw",
        aliases: &[],
        mail_charset: N::Big5,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_TW,
    },
    Language {
        name: "English",
        short_name: "en",
        aliases: &[],
        mail_charset: N::Iso8859_1,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_NEUT,
    },
    Language {
        name: "German",
        short_name: "de",
        aliases: &["Deutsch"],
        mail_charset: N::Iso8859_15,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_NEUT,
    },
    Language {
        name: "Russian",
        short_name: "ru",
        aliases: &[],
        mail_charset: N::Koi8r,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_RU,
    },
    Language {
        name: "Ukrainian",
        short_name: "ua",
        aliases: &[],
        mail_charset: N::Koi8u,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_UA,
    },
    Language {
        name: "Armenian",
        short_name: "hy",
        aliases: &[],
        mail_charset: N::Armscii8,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_HY,
    },
    Language {
        name: "Turkish",
        short_name: "tr",
        aliases: &[],
        mail_charset: N::Iso8859_9,
        mail_header_encoding: N::Qprint,
        mail_body_encoding: N::EightBit,
        detect_order: ORDER_TR,
    },
    Language {
        name: "neutral",
        short_name: "neutral",
        aliases: &[],
        mail_charset: N::Utf8,
        mail_header_encoding: N::Base64,
        mail_body_encoding: N::Base64,
        detect_order: ORDER_NEUT,
    },
];

/// `mbfl_name2language`: by name, then short name, then alias
/// (ASCII case-insensitive; the name ends at the first NUL).
pub fn language_by_name(name: &[u8]) -> Option<&'static Language> {
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    let eq = |s: &str| s.as_bytes().eq_ignore_ascii_case(name);
    LANGUAGES
        .iter()
        .find(|l| eq(l.name))
        .or_else(|| LANGUAGES.iter().find(|l| eq(l.short_name)))
        .or_else(|| LANGUAGES.iter().find(|l| l.aliases.iter().any(|a| eq(a))))
}

/// Value of `mb_substitute_character()` (getter form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubstituteChar {
    /// `"none"`.
    None,
    /// `"long"`.
    Long,
    /// `"entity"`.
    Entity,
    /// A code point.
    Char(u32),
}

/// Argument of `mb_substitute_character($x)` (setter form).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubstituteArg<'a> {
    /// A string argument.
    Str(&'a [u8]),
    /// An integer argument.
    Int(i64),
}

/// An encoding list argument as accepted by `mb_detect_order`,
/// `mb_detect_encoding`, `mb_convert_encoding` (`$from_encoding`) and
/// `mb_convert_variables`.
#[derive(Debug, Clone, Copy)]
pub enum EncodingList<'a> {
    /// A comma-separated string (`php_mb_parse_encoding_list`).
    Str(&'a [u8]),
    /// An array of names (`php_mb_parse_encoding_array`), resolved as
    /// the caller converted its elements.
    Array(&'a EncodingArray),
}

/// One element of an encoding-list array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayEntry {
    /// `"auto"`: the language's detection order.
    Auto,
    /// A known encoding.
    Encoding(&'static Encoding),
}

/// An encoding-list array resolved element by element, as
/// `php_mb_parse_encoding_array` does: the list keeps the encodings, not
/// the names (one pointer per element); only the first invalid name is
/// kept, for the error, and nothing after it is read.
#[derive(Debug, Clone, Default)]
pub struct EncodingArray {
    entries: Vec<ArrayEntry>,
    invalid: Option<Vec<u8>>,
}

impl EncodingArray {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The list of `names` (tests and callers that have them at hand).
    #[must_use]
    pub fn from_names<'n>(names: impl IntoIterator<Item = &'n [u8]>) -> Self {
        let mut list = Self::new();
        for name in names {
            if !list.push(name) {
                break;
            }
        }
        list
    }

    /// Room for one more element, checked against `memory_limit` (the
    /// array is script-sized).
    ///
    /// # Errors
    /// The recorded violation.
    pub fn reserve(&mut self) -> Result<(), php_memlimit::OverLimit> {
        php_memlimit::TryGrow::try_grow(&mut self.entries, 1)
    }

    /// Resolves the element `name`; `false` when it names no encoding
    /// (the caller then stops converting elements).
    pub fn push(&mut self, name: &[u8]) -> bool {
        if name.eq_ignore_ascii_case(b"auto") {
            self.entries.push(ArrayEntry::Auto);
        } else if let Some(enc) = encoding::by_cname(name) {
            self.entries.push(ArrayEntry::Encoding(enc));
        } else {
            self.invalid = Some(name.to_vec());
            return false;
        }
        true
    }

    /// The number of elements read (the invalid one included).
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() + usize::from(self.invalid.is_some())
    }

    /// Whether no element was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The per-request state of the mbstring extension.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct MbState {
    language: &'static Language,
    detect_order_ini: Option<Vec<&'static Encoding>>,
    current_detect_order: Vec<&'static Encoding>,
    internal_encoding: &'static Encoding,
    current_internal_encoding: &'static Encoding,
    http_output_encoding: &'static Encoding,
    current_http_output_encoding: &'static Encoding,
    http_input_list: Vec<&'static Encoding>,
    /// `mb_http_input()` results (set by the SAPI's input handling).
    pub http_input_identify: Option<&'static Encoding>,
    /// `mb_http_input("G")`.
    pub http_input_identify_get: Option<&'static Encoding>,
    /// `mb_http_input("P")`.
    pub http_input_identify_post: Option<&'static Encoding>,
    /// `mb_http_input("C")`.
    pub http_input_identify_cookie: Option<&'static Encoding>,
    /// `mb_http_input("S")`.
    pub http_input_identify_string: Option<&'static Encoding>,
    filter_illegal_mode: ErrorMode,
    filter_illegal_substchar: u32,
    current_filter_illegal_mode: ErrorMode,
    current_filter_illegal_substchar: u32,
    /// Number of conversion errors so far (`MBSTRG(illegalchars)`).
    pub illegalchars: u64,
    /// `mbstring.strict_detection`.
    pub strict_detection: bool,
    /// `mbstring.encoding_translation`.
    pub encoding_translation: bool,
    last_used_encoding: Option<(Vec<u8>, &'static Encoding)>,
    internal_encoding_set: bool,
    http_output_set: bool,
    http_input_set: bool,
    default_charset: Vec<u8>,
    php_internal_encoding: Option<Vec<u8>>,
    php_input_encoding: Option<Vec<u8>>,
    php_output_encoding: Option<Vec<u8>>,
    /// `mbstring.http_output_conv_mimetypes` (a PCRE pattern, matched
    /// case-insensitively by the SAPI glue).
    pub http_output_conv_mimetypes: Vec<u8>,
    pub(crate) outconv_enabled: bool,
    pub(crate) outconv_state: u32,
    pub(crate) regex: crate::mbregex::RegexState,
    /// Warnings / deprecations emitted since last drained.
    pub diagnostics: Vec<Diagnostic>,
}

impl Default for MbState {
    fn default() -> Self {
        Self::new()
    }
}

impl MbState {
    /// State after `MINIT` + `RINIT` with no `mbstring.*` INI settings and
    /// `default_charset=UTF-8`.
    pub fn new() -> Self {
        Self::with_ini(&[])
    }

    /// State after `MINIT` + `RINIT` given startup INI settings (php.ini /
    /// `-d`): `mbstring.*` entries and PHP's `default_charset`,
    /// `internal_encoding`, `input_encoding`, `output_encoding`. Warnings and
    /// deprecations raised by the INI handlers ("PHP Startup") are left in
    /// [`MbState::diagnostics`].
    pub fn with_ini(settings: &[(&str, &[u8])]) -> Self {
        let utf8 = encoding::utf8();
        let neutral = &LANGUAGES[11];
        let mut s = Self {
            language: neutral,
            detect_order_ini: None,
            current_detect_order: Vec::new(),
            internal_encoding: utf8,
            current_internal_encoding: utf8,
            http_output_encoding: encoding::pass(),
            current_http_output_encoding: encoding::pass(),
            http_input_list: Vec::new(),
            http_input_identify: None,
            http_input_identify_get: None,
            http_input_identify_post: None,
            http_input_identify_cookie: None,
            http_input_identify_string: None,
            filter_illegal_mode: ErrorMode::Char,
            filter_illegal_substchar: u32::from(b'?'),
            current_filter_illegal_mode: ErrorMode::Char,
            current_filter_illegal_substchar: u32::from(b'?'),
            illegalchars: 0,
            strict_detection: false,
            encoding_translation: false,
            last_used_encoding: None,
            internal_encoding_set: false,
            http_output_set: false,
            http_input_set: false,
            default_charset: b"UTF-8".to_vec(),
            php_internal_encoding: None,
            php_input_encoding: None,
            php_output_encoding: None,
            http_output_conv_mimetypes: br"^(text/|application/xhtml\+xml)".to_vec(),
            outconv_enabled: false,
            outconv_state: 0,
            regex: crate::mbregex::RegexState::default(),
            diagnostics: Vec::new(),
        };
        let get = |k: &str| {
            settings
                .iter()
                .rev()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| *v)
        };
        // PHP core settings are registered before mbstring's MINIT.
        if let Some(v) = get("default_charset") {
            s.default_charset = v.to_vec();
        }
        for (key, slot) in [
            ("internal_encoding", 0),
            ("input_encoding", 1),
            ("output_encoding", 2),
        ] {
            if let Some(v) = get(key).filter(|v| !v.is_empty()) {
                let v = Some(v.to_vec());
                match slot {
                    0 => s.php_internal_encoding = v,
                    1 => s.php_input_encoding = v,
                    _ => s.php_output_encoding = v,
                }
            }
        }
        // mbstring INI entries, in registration order (PHP_INI_BEGIN).
        s.ini_set_language(get("mbstring.language").unwrap_or(b"neutral"));
        s.ini_set_detect_order(get("mbstring.detect_order"), false);
        s.ini_set_http_input(get("mbstring.http_input"));
        s.ini_set_http_output(get("mbstring.http_output"));
        s.ini_set_internal_encoding(get("mbstring.internal_encoding"));
        s.ini_set_substitute_character(get("mbstring.substitute_character"));
        s.encoding_translation = get("mbstring.encoding_translation").is_some_and(ini_bool);
        s.strict_detection = get("mbstring.strict_detection").is_some_and(ini_bool);
        // MINIT: the internal-encoding hook, then RINIT.
        s.internal_encoding_changed_hook();
        s.request_startup();
        s
    }

    /// `PHP_RINIT`: reset the per-request "current" values from the INI ones.
    pub fn request_startup(&mut self) {
        self.current_internal_encoding = self.internal_encoding;
        self.current_http_output_encoding = self.http_output_encoding;
        self.current_filter_illegal_mode = self.filter_illegal_mode;
        self.current_filter_illegal_substchar = self.filter_illegal_substchar;
        self.illegalchars = 0;
        self.populate_current_detect_order_list();
    }

    /// `PHP_RSHUTDOWN`.
    pub fn request_shutdown(&mut self) {
        self.http_input_identify = None;
        self.http_input_identify_get = None;
        self.http_input_identify_post = None;
        self.http_input_identify_cookie = None;
        self.http_input_identify_string = None;
        self.last_used_encoding = None;
        self.internal_encoding_set = false;
        self.http_output_set = false;
        self.http_input_set = false;
        self.outconv_enabled = false;
        self.outconv_state = 0;
        self.regex.request_shutdown();
    }

    /// Take the pending diagnostics.
    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    pub(crate) fn warn(&mut self, msg: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Warning,
            message: msg.into(),
        });
    }

    pub(crate) fn deprecated(&mut self, msg: impl Into<String>) {
        self.diagnostics.push(Diagnostic {
            level: Level::Deprecated,
            message: msg.into(),
        });
    }

    /// The current internal encoding.
    #[allow(clippy::misnamed_getters)]
    pub fn internal_encoding(&self) -> &'static Encoding {
        self.current_internal_encoding
    }

    /// Encoding exposed by `php_mb_zend_internal_encoding_getter`.
    /// Unlike [`Self::internal_encoding`], `mb_internal_encoding()` does not
    /// change this configured encoding; encoding INI handlers do.
    pub fn script_internal_encoding(&self) -> &'static Encoding {
        self.internal_encoding
    }

    /// The current HTTP output encoding (may be `pass`).
    #[allow(clippy::misnamed_getters)]
    pub fn http_output_encoding(&self) -> &'static Encoding {
        self.current_http_output_encoding
    }

    /// The current language.
    pub fn language(&self) -> &'static Language {
        self.language
    }

    /// The current detect order.
    pub fn detect_order(&self) -> &[&'static Encoding] {
        &self.current_detect_order
    }

    /// The HTTP input encoding list.
    pub fn http_input_list(&self) -> &[&'static Encoding] {
        &self.http_input_list
    }

    /// Current substitution mode.
    pub fn illegal_mode(&self) -> ErrorMode {
        self.current_filter_illegal_mode
    }

    /// Current substitute character.
    pub fn illegal_substchar(&self) -> u32 {
        self.current_filter_illegal_substchar
    }

    fn populate_current_detect_order_list(&mut self) {
        self.current_detect_order = match &self.detect_order_ini {
            Some(list) if !list.is_empty() => list.clone(),
            _ => self
                .language
                .detect_order
                .iter()
                .map(|&n| encoding::get(n))
                .collect(),
        };
    }

    /// `php_get_{internal,input,output}_encoding()` (main/main.c): the
    /// core INI setting if non-empty, else a non-empty `default_charset`,
    /// else `"UTF-8"`.
    fn php_encoding(&self, setting: Option<&[u8]>) -> Vec<u8> {
        match setting {
            Some(v) if !v.is_empty() => v.to_vec(),
            _ if !self.default_charset.is_empty() => self.default_charset.clone(),
            _ => b"UTF-8".to_vec(),
        }
    }

    /// `php_get_internal_encoding()`.
    fn php_internal(&self) -> Vec<u8> {
        self.php_encoding(self.php_internal_encoding.as_deref())
    }

    /// `php_get_input_encoding()`.
    fn php_input(&self) -> Vec<u8> {
        self.php_encoding(self.php_input_encoding.as_deref())
    }

    /// `php_get_output_encoding()`.
    fn php_output(&self) -> Vec<u8> {
        self.php_encoding(self.php_output_encoding.as_deref())
    }

    fn internal_encoding_changed_hook(&mut self) {
        if !self.internal_encoding_set {
            let v = self.php_internal();
            self.internal_encoding_set_value(Some(&v));
        }
        if !self.http_output_set {
            let v = self.php_output();
            self.http_output_set_value(&v);
        }
        if !self.http_input_set {
            let v = self.php_input();
            self.http_input_set_value(&v);
        }
    }

    // ---- INI handlers -------------------------------------------------

    /// One of PHP's core encoding INI settings changed (`default_charset`,
    /// `internal_encoding`, `input_encoding`, `output_encoding`); runs the
    /// `php_internal_encoding_changed` hook. Empty values count as unset.
    /// Returns `false` for other keys.
    pub fn set_php_encoding_ini(&mut self, key: &str, value: &[u8]) -> bool {
        let v = (!value.is_empty()).then(|| value.to_vec());
        match key {
            "default_charset" => self.default_charset = value.to_vec(),
            "internal_encoding" => self.php_internal_encoding = v,
            "input_encoding" => self.php_input_encoding = v,
            "output_encoding" => self.php_output_encoding = v,
            _ => return false,
        }
        self.internal_encoding_changed_hook();
        true
    }

    /// Apply an `mbstring.*` (or PHP core encoding) INI setting by name, as
    /// at startup (`runtime == false`) or via `ini_set` (`runtime == true`).
    /// Returns whether the setting was accepted.
    pub fn ini_set(&mut self, key: &str, value: Option<&[u8]>, runtime: bool) -> bool {
        match key {
            "mbstring.language" => self.ini_set_language(value.unwrap_or(b"neutral")),
            "mbstring.detect_order" => {
                let ok = self.ini_set_detect_order(value, runtime);
                if !runtime {
                    self.populate_current_detect_order_list();
                }
                ok
            }
            "mbstring.http_input" => self.ini_set_http_input(value),
            "mbstring.http_output" => self.ini_set_http_output(value),
            "mbstring.internal_encoding" => {
                let ok = self.ini_set_internal_encoding(value);
                if !runtime {
                    self.current_internal_encoding = self.internal_encoding;
                }
                ok
            }
            "mbstring.substitute_character" => self.ini_set_substitute_character(value),
            "mbstring.strict_detection" => {
                self.strict_detection = value.is_some_and(ini_bool);
                true
            }
            "mbstring.http_output_conv_mimetypes" => {
                let v = value.unwrap_or(br"^(text/|application/xhtml\+xml)");
                self.http_output_conv_mimetypes = v.trim_ascii().to_vec();
                true
            }
            "mbstring.encoding_translation" => {
                self.encoding_translation = value.is_some_and(ini_bool);
                true
            }
            "default_charset" | "internal_encoding" | "input_encoding" | "output_encoding" => {
                self.set_php_encoding_ini(key, value.unwrap_or(b""))
            }
            _ => false,
        }
    }

    /// `mbstring.language` INI handler; returns `false` on failure.
    pub fn ini_set_language(&mut self, value: &[u8]) -> bool {
        if let Some(l) = language_by_name(value) {
            self.language = l;
            true
        } else {
            self.language = &LANGUAGES[11];
            false
        }
    }

    /// `mbstring.detect_order` INI handler (`None` = reset to default).
    /// `runtime` is true for `ini_set()` (stage RUNTIME).
    pub fn ini_set_detect_order(&mut self, value: Option<&[u8]>, runtime: bool) -> bool {
        let Some(value) = value else {
            self.detect_order_ini = None;
            return true;
        };
        match self.parse_encoding_list(value, 0) {
            Ok(list) if !list.is_empty() => {
                self.detect_order_ini = Some(list);
                if runtime {
                    self.populate_current_detect_order_list();
                }
                true
            }
            _ => false,
        }
    }

    fn http_input_set_value(&mut self, value: &[u8]) -> bool {
        let list = if value == b"pass" {
            vec![encoding::pass()]
        } else {
            match self.parse_encoding_list(value, 0) {
                Ok(l) if !l.is_empty() => l,
                _ => return false,
            }
        };
        self.http_input_list = list;
        true
    }

    /// `mbstring.http_input` INI handler.
    pub fn ini_set_http_input(&mut self, value: Option<&[u8]>) -> bool {
        if value.is_some() {
            self.deprecated("Use of mbstring.http_input is deprecated");
        }
        match value {
            Some(v) if !v.is_empty() => {
                self.http_input_set = true;
                self.http_input_set_value(v)
            }
            _ => {
                self.http_input_set = false;
                let cs = self.php_input();
                self.http_input_set_value(&cs);
                true
            }
        }
    }

    fn http_output_set_value(&mut self, value: &[u8]) -> bool {
        match get_encoding_or_pass(value) {
            Some(e) => {
                self.http_output_encoding = e;
                self.current_http_output_encoding = e;
                true
            }
            None => false,
        }
    }

    /// `mbstring.http_output` INI handler.
    pub fn ini_set_http_output(&mut self, value: Option<&[u8]>) -> bool {
        if value.is_some() {
            self.deprecated("Use of mbstring.http_output is deprecated");
        }
        match value {
            Some(v) if !v.is_empty() => {
                self.http_output_set = true;
                self.http_output_set_value(v)
            }
            _ => {
                self.http_output_set = false;
                let cs = self.php_output();
                self.http_output_set_value(&cs);
                true
            }
        }
    }

    fn internal_encoding_set_value(&mut self, value: Option<&[u8]>) {
        let enc = match value {
            Some(v) if !v.is_empty() => encoding::by_cname(v),
            _ => None,
        };
        let enc = enc.unwrap_or_else(|| {
            if let Some(v) = value {
                let shown = &v[..v.iter().position(|&b| b == 0).unwrap_or(v.len())];
                self.warn(format!(
                    "Unknown encoding \"{}\" in ini setting",
                    String::from_utf8_lossy(shown)
                ));
            }
            encoding::utf8()
        });
        self.internal_encoding = enc;
        self.current_internal_encoding = enc;
        let name = value.unwrap_or(b"");
        if !self.regex.set_default_mbctype(name) {
            self.regex.set_default_mbctype(b"UTF-8");
        }
        self.regex.set_mbctype(name);
    }

    /// `mbstring.internal_encoding` INI handler.
    pub fn ini_set_internal_encoding(&mut self, value: Option<&[u8]>) -> bool {
        if value.is_some() {
            self.deprecated("Use of mbstring.internal_encoding is deprecated");
        }
        match value {
            Some(v) if !v.is_empty() => {
                self.internal_encoding_set = true;
                self.internal_encoding_set_value(Some(v));
            }
            _ => {
                self.internal_encoding_set = false;
                let cs = self.php_internal();
                self.internal_encoding_set_value(Some(&cs));
            }
        }
        true
    }

    /// `mbstring.substitute_character` INI handler.
    pub fn ini_set_substitute_character(&mut self, value: Option<&[u8]>) -> bool {
        let Some(v) = value else {
            self.filter_illegal_mode = ErrorMode::Char;
            self.current_filter_illegal_mode = ErrorMode::Char;
            self.filter_illegal_substchar = u32::from(b'?');
            self.current_filter_illegal_substchar = u32::from(b'?');
            return true;
        };
        let mode = if v.eq_ignore_ascii_case(b"none") {
            ErrorMode::None
        } else if v.eq_ignore_ascii_case(b"long") {
            ErrorMode::Long
        } else if v.eq_ignore_ascii_case(b"entity") {
            ErrorMode::Entity
        } else {
            ErrorMode::Char
        };
        self.filter_illegal_mode = mode;
        self.current_filter_illegal_mode = mode;
        if mode == ErrorMode::Char && !v.is_empty() {
            if let Some(c) = c_strtol_whole(v) {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let c = c as i32 as u32;
                self.filter_illegal_substchar = c;
                self.current_filter_illegal_substchar = c;
            }
        }
        true
    }

    // ---- encoding argument handling -----------------------------------

    /// `php_mb_get_encoding`: resolve an optional encoding-name argument
    /// (argument `arg_num`, named `arg_name`), defaulting to the internal
    /// encoding. Emits the master-branch deprecations for Base64, `QPrint`,
    /// HTML-ENTITIES and UUENCODE.
    pub fn get_encoding(
        &mut self,
        name: Option<&[u8]>,
        arg_num: u32,
        arg_name: &'static str,
    ) -> MbResult<&'static Encoding> {
        let Some(name) = name else {
            return Ok(self.current_internal_encoding);
        };
        if let Some((last, enc)) = &self.last_used_encoding {
            if last.eq_ignore_ascii_case(name) {
                return Ok(enc);
            }
        }
        let Some(enc) = encoding::by_cname(name) else {
            return Err(MbError::invalid_encoding(arg_num, arg_name, name));
        };
        match enc.no {
            N::Base64 => self.deprecated(
                "Handling Base64 via mbstring is deprecated; use base64_encode/base64_decode instead",
            ),
            N::Qprint => self.deprecated(
                "Handling QPrint via mbstring is deprecated; use quoted_printable_encode/quoted_printable_decode instead",
            ),
            N::HtmlEnt => self.deprecated(
                "Handling HTML entities via mbstring is deprecated; use htmlspecialchars, htmlentities, or mb_encode_numericentity/mb_decode_numericentity instead",
            ),
            N::Uuencode => self.deprecated(
                "Handling Uuencode via mbstring is deprecated; use convert_uuencode/convert_uudecode instead",
            ),
            _ => {}
        }
        self.last_used_encoding = Some((name.to_vec(), enc));
        Ok(enc)
    }

    /// `php_mb_parse_encoding_list`: a comma-separated list using the language's
    /// `auto` expansion. With `arg_num == 0`, invalid names produce INI
    /// diagnostics; hosts can drain them with [`Self::take_diagnostics`].
    pub fn parse_encoding_list(
        &mut self,
        value: &[u8],
        arg_num: u32,
    ) -> Result<Vec<&'static Encoding>, Option<MbError>> {
        self.parse_encoding_list_named(value, arg_num, "")
    }

    pub(crate) fn parse_encoding_list_named(
        &mut self,
        value: &[u8],
        arg_num: u32,
        arg_name: &'static str,
    ) -> Result<Vec<&'static Encoding>, Option<MbError>> {
        if value.is_empty() {
            return Ok(Vec::new());
        }
        let tmp = if value.len() > 2 && value[0] == b'"' && value[value.len() - 1] == b'"' {
            &value[1..value.len() - 1]
        } else {
            value
        };
        #[allow(clippy::naive_bytecount)]
        let size =
            1 + tmp.iter().filter(|&&b| b == b',').count() + self.language.detect_order.len();
        let mut list = Vec::new();
        let mut included_auto = false;
        let mut rest = tmp;
        loop {
            let comma = rest.iter().position(|&b| b == b',');
            let item = &rest[..comma.unwrap_or(rest.len())];
            let item = trim_spaces_tabs(item);
            if item.eq_ignore_ascii_case(b"auto") {
                if !included_auto {
                    included_auto = true;
                    list.extend(self.language.detect_order.iter().map(|&n| encoding::get(n)));
                }
            } else if let Some(enc) = encoding::by_name(item) {
                list.push(enc);
            } else {
                // Printed with "%.*s", which stops at a NUL byte.
                let shown = String::from_utf8_lossy(c_str(item));
                if arg_num == 0 {
                    self.warn(format!("INI setting contains invalid encoding \"{shown}\""));
                    return Err(None);
                }
                return Err(Some(MbError::arg_value(
                    arg_num,
                    arg_name,
                    format!("contains invalid encoding \"{shown}\""),
                )));
            }
            match comma {
                Some(c) if list.len() < size => rest = &rest[c + 1..],
                _ => break,
            }
        }
        Ok(list)
    }

    /// `php_mb_parse_encoding_array`.
    pub(crate) fn parse_encoding_array(
        &self,
        items: &EncodingArray,
        arg_num: u32,
        arg_name: &'static str,
    ) -> MbResult<Vec<&'static Encoding>> {
        if let Some(name) = &items.invalid {
            return Err(MbError::arg_value(
                arg_num,
                arg_name,
                format!(
                    "contains invalid encoding \"{}\"",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
        let mut list = Vec::new();
        let mut included_auto = false;
        for entry in &items.entries {
            match *entry {
                ArrayEntry::Auto => {
                    if !included_auto {
                        included_auto = true;
                        list.extend(self.language.detect_order.iter().map(|&n| encoding::get(n)));
                    }
                }
                ArrayEntry::Encoding(enc) => list.push(enc),
            }
        }
        Ok(list)
    }

    /// Parse either form of encoding list argument.
    pub(crate) fn parse_list_arg(
        &mut self,
        list: EncodingList<'_>,
        arg_num: u32,
        arg_name: &'static str,
    ) -> MbResult<Vec<&'static Encoding>> {
        match list {
            EncodingList::Str(s) => self
                .parse_encoding_list_named(s, arg_num, arg_name)
                .map_err(|e| e.unwrap_or_else(|| MbError::Value(String::new()))),
            EncodingList::Array(a) => self.parse_encoding_array(a, arg_num, arg_name),
        }
    }

    // ---- PHP functions -------------------------------------------------

    /// `mb_language()` (getter).
    pub fn mb_language_get(&self) -> &'static str {
        self.language.name
    }

    /// `mb_language($language)` (setter); runs the INI handler.
    pub fn mb_language_set(&mut self, name: &[u8]) -> MbResult<bool> {
        if self.ini_set_language(name) {
            Ok(true)
        } else {
            Err(MbError::arg_value(
                1,
                "language",
                format!(
                    "must be a valid language, \"{}\" given",
                    String::from_utf8_lossy(name)
                ),
            ))
        }
    }

    /// `mb_internal_encoding()` (getter).
    pub fn mb_internal_encoding_get(&self) -> &'static str {
        self.current_internal_encoding.name
    }

    /// `mb_internal_encoding($encoding)` (setter).
    pub fn mb_internal_encoding_set(&mut self, name: &[u8]) -> MbResult<bool> {
        match encoding::by_cname(name) {
            Some(enc) => {
                self.current_internal_encoding = enc;
                self.internal_encoding_set = true;
                Ok(true)
            }
            None => Err(MbError::invalid_encoding(1, "encoding", c_str(name))),
        }
    }

    /// `mb_http_input($type)`.
    pub fn mb_http_input(&self, ty: Option<&[u8]>) -> MbResult<HttpInput> {
        let err = || {
            MbError::arg_value(
                1,
                "type",
                "must be one of \"G\", \"P\", \"C\", \"S\", \"I\", or \"L\"",
            )
        };
        let enc = match ty {
            None => self.http_input_identify,
            Some(t) if t.len() != 1 => return Err(err()),
            Some(t) => match t[0].to_ascii_uppercase() {
                b'G' => self.http_input_identify_get,
                b'P' => self.http_input_identify_post,
                b'C' => self.http_input_identify_cookie,
                b'S' => self.http_input_identify_string,
                b'I' => {
                    return Ok(HttpInput::List(
                        self.http_input_list.iter().map(|e| e.name).collect(),
                    ))
                }
                b'L' => {
                    if self.http_input_list.is_empty() {
                        return Ok(HttpInput::False);
                    }
                    let names: Vec<_> = self.http_input_list.iter().map(|e| e.name).collect();
                    return Ok(HttpInput::Name(names.join(",")));
                }
                _ => return Err(err()),
            },
        };
        Ok(enc.map_or(HttpInput::False, |e| HttpInput::Name(e.name.to_string())))
    }

    /// `mb_http_output()` (getter).
    pub fn mb_http_output_get(&self) -> &'static str {
        self.current_http_output_encoding.name
    }

    /// `mb_http_output($encoding)` (setter). Names containing NUL bytes are
    /// a `TypeError`/`ValueError` raised by the VM's path parsing first.
    pub fn mb_http_output_set(&mut self, name: &[u8]) -> MbResult<bool> {
        match get_encoding_or_pass(name) {
            Some(e) => {
                self.http_output_set = true;
                self.current_http_output_encoding = e;
                Ok(true)
            }
            None => Err(MbError::invalid_encoding(1, "encoding", name)),
        }
    }

    /// `mb_detect_order()` (getter).
    pub fn mb_detect_order_get(&self) -> Vec<&'static str> {
        self.current_detect_order.iter().map(|e| e.name).collect()
    }

    /// `mb_detect_order($encoding)` (setter).
    pub fn mb_detect_order_set(&mut self, list: EncodingList<'_>) -> MbResult<bool> {
        let list = self.parse_list_arg(list, 1, "encoding")?;
        if list.is_empty() {
            return Err(MbError::arg_value(
                1,
                "encoding",
                "must specify at least one encoding",
            ));
        }
        self.current_detect_order = list;
        Ok(true)
    }

    /// `mb_substitute_character()` (getter).
    pub fn mb_substitute_character_get(&self) -> SubstituteChar {
        match self.current_filter_illegal_mode {
            ErrorMode::None => SubstituteChar::None,
            ErrorMode::Long => SubstituteChar::Long,
            ErrorMode::Entity => SubstituteChar::Entity,
            _ => SubstituteChar::Char(self.current_filter_illegal_substchar),
        }
    }

    /// `mb_substitute_character($substitute_character)` (setter).
    #[allow(clippy::needless_pass_by_value)]
    pub fn mb_substitute_character_set(&mut self, arg: SubstituteArg<'_>) -> MbResult<bool> {
        match arg {
            SubstituteArg::Str(s) => {
                let mode = if s.eq_ignore_ascii_case(b"none") {
                    ErrorMode::None
                } else if s.eq_ignore_ascii_case(b"long") {
                    ErrorMode::Long
                } else if s.eq_ignore_ascii_case(b"entity") {
                    ErrorMode::Entity
                } else {
                    return Err(MbError::arg_value(
                        1,
                        "substitute_character",
                        "must be \"none\", \"long\", \"entity\" or a valid codepoint",
                    ));
                };
                self.current_filter_illegal_mode = mode;
                Ok(true)
            }
            SubstituteArg::Int(cp) => {
                if !(0..0x11_0000).contains(&cp) || (0xD800..=0xDFFF).contains(&cp) {
                    return Err(MbError::arg_value(
                        1,
                        "substitute_character",
                        "is not a valid codepoint",
                    ));
                }
                self.current_filter_illegal_mode = ErrorMode::Char;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                {
                    self.current_filter_illegal_substchar = cp as u32;
                }
                Ok(true)
            }
        }
    }

    /// `mb_preferred_mime_name($encoding)`; `Ok(None)` is `false` (with a
    /// warning emitted).
    pub fn mb_preferred_mime_name(&mut self, name: &[u8]) -> MbResult<Option<&'static str>> {
        let Some(enc) = encoding::by_cname(name) else {
            return Err(MbError::invalid_encoding(1, "encoding", c_str(name)));
        };
        if let Some(m) = enc.preferred_mime_name() {
            Ok(Some(m))
        } else {
            self.warn(format!(
                "No MIME preferred name corresponding to \"{}\"",
                String::from_utf8_lossy(c_str(name))
            ));
            Ok(None)
        }
    }

    /// `mb_encoding_aliases($encoding)`.
    pub fn mb_encoding_aliases(&mut self, name: &[u8]) -> MbResult<Vec<&'static str>> {
        let enc = self.get_encoding(Some(name), 1, "encoding")?;
        Ok(enc.aliases.to_vec())
    }
}

/// Result of `mb_http_input()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpInput {
    /// `false`.
    False,
    /// A string.
    Name(String),
    /// An array of names.
    List(Vec<&'static str>),
}

/// `mb_list_encodings()`.
pub fn mb_list_encodings() -> Vec<&'static str> {
    encoding::list().map(|e| e.name).collect()
}

/// PHP's INI boolean parsing (`zend_ini_parse_bool`).
fn ini_bool(v: &[u8]) -> bool {
    let l = v.to_ascii_lowercase();
    match l.as_slice() {
        b"true" | b"yes" | b"on" => true,
        _ => std::str::from_utf8(v)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .is_some_and(|n| n != 0),
    }
}

/// `php_mb_get_encoding_or_pass`.
fn get_encoding_or_pass(name: &[u8]) -> Option<&'static Encoding> {
    if name == b"pass" {
        Some(encoding::pass())
    } else {
        encoding::by_name(name)
    }
}

fn c_str(s: &[u8]) -> &[u8] {
    &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
}

fn trim_spaces_tabs(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// `strtol(s, &end, 0)` requiring the whole string to be consumed.
#[allow(clippy::many_single_char_names)]
fn c_strtol_whole(s: &[u8]) -> Option<i64> {
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C) {
        i += 1;
    }
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let (radix, start) = if s.get(i) == Some(&b'0')
        && matches!(s.get(i + 1), Some(b'x' | b'X'))
        && s.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
    {
        (16, i + 2)
    } else if s.get(i) == Some(&b'0') {
        (8, i)
    } else {
        (10, i)
    };
    let mut j = start;
    let mut v: i64 = 0;
    while j < s.len() {
        let Some(d) = char::from(s[j]).to_digit(radix) else {
            break;
        };
        v = v
            .saturating_mul(i64::from(radix))
            .saturating_add(i64::from(d));
        j += 1;
    }
    if j == start || j != s.len() {
        return None;
    }
    Some(if neg { -v } else { v })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_php() {
        let s = MbState::new();
        assert_eq!(s.mb_internal_encoding_get(), "UTF-8");
        assert_eq!(s.mb_http_output_get(), "UTF-8");
        assert_eq!(s.mb_language_get(), "neutral");
        assert_eq!(s.mb_detect_order_get(), vec!["ASCII", "UTF-8"]);
        assert_eq!(
            s.mb_http_input(Some(b"L")).unwrap(),
            HttpInput::Name("UTF-8".into())
        );
        assert_eq!(s.mb_http_input(None).unwrap(), HttpInput::False);
        assert_eq!(s.mb_substitute_character_get(), SubstituteChar::Char(63));
    }

    #[test]
    fn empty_default_charset_falls_back_to_utf8() {
        // phar_dotted_path.phpt: `default_charset=` is not an unknown
        // encoding (php_get_internal_encoding() returns "UTF-8").
        let s = MbState::with_ini(&[("default_charset", b"")]);
        assert!(s.diagnostics.is_empty());
        assert_eq!(s.mb_internal_encoding_get(), "UTF-8");
        assert_eq!(s.mb_http_output_get(), "UTF-8");
    }

    #[test]
    fn detect_order_parsing() {
        let mut s = MbState::new();
        s.mb_language_set(b"ja").unwrap();
        s.mb_detect_order_set(EncodingList::Str(b" auto , UTF-8"))
            .unwrap();
        assert_eq!(
            s.mb_detect_order_get(),
            vec!["ASCII", "JIS", "UTF-8", "EUC-JP", "SJIS", "UTF-8"]
        );
        let e = s
            .mb_detect_order_set(EncodingList::Str(b"UTF-8, BAD"))
            .unwrap_err();
        assert_eq!(
            e.to_string(),
            "Argument #1 ($encoding) contains invalid encoding \"BAD\""
        );
    }

    #[test]
    fn substitute_character() {
        let mut s = MbState::new();
        assert!(s
            .mb_substitute_character_set(SubstituteArg::Str(b"LONG"))
            .unwrap());
        assert_eq!(s.mb_substitute_character_get(), SubstituteChar::Long);
        assert!(s
            .mb_substitute_character_set(SubstituteArg::Int(0xD800))
            .is_err());
        assert!(s.ini_set_substitute_character(Some(b"0x3013")));
        assert_eq!(s.illegal_substchar(), 0x3013);
    }
}
