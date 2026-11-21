// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `phpinfo()` rendering (`ext/standard/info.c`): the table/box/section
//! primitives in text (CLI) and HTML mode, and [`print_info`], which lays
//! out the sections from data the VM supplies through [`InfoSource`].

use php_runtime::{RtResult, RuntimeContext, Value, ZArray};

mod logos;

/// `INFO_*` constants.
pub const INFO_CONSTANTS: &[(&str, i64)] = &[
    ("INFO_GENERAL", INFO_GENERAL),
    ("INFO_CREDITS", INFO_CREDITS),
    ("INFO_CONFIGURATION", INFO_CONFIGURATION),
    ("INFO_MODULES", INFO_MODULES),
    ("INFO_ENVIRONMENT", INFO_ENVIRONMENT),
    ("INFO_VARIABLES", INFO_VARIABLES),
    ("INFO_LICENSE", INFO_LICENSE),
    ("INFO_ALL", INFO_ALL),
    ("CREDITS_GROUP", 1),
    ("CREDITS_GENERAL", 2),
    ("CREDITS_SAPI", 4),
    ("CREDITS_MODULES", 8),
    ("CREDITS_DOCS", 16),
    ("CREDITS_FULLPAGE", 32),
    ("CREDITS_QA", 64),
    ("CREDITS_ALL", 0xFFFF_FFFF),
];

/// `PHP_INFO_GENERAL`.
pub const INFO_GENERAL: i64 = 1;
/// `PHP_INFO_CREDITS`.
pub const INFO_CREDITS: i64 = 2;
/// `PHP_INFO_CONFIGURATION`.
pub const INFO_CONFIGURATION: i64 = 4;
/// `PHP_INFO_MODULES`.
pub const INFO_MODULES: i64 = 8;
/// `PHP_INFO_ENVIRONMENT`.
pub const INFO_ENVIRONMENT: i64 = 16;
/// `PHP_INFO_VARIABLES`.
pub const INFO_VARIABLES: i64 = 32;
/// `PHP_INFO_LICENSE`.
pub const INFO_LICENSE: i64 = 64;
/// `PHP_INFO_ALL`.
pub const INFO_ALL: i64 = 0xFFFF_FFFF;

/// `php_escape_html_entities(s, ENT_QUOTES, "utf-8")` (invalid UTF-8
/// yields an empty string, as without `ENT_SUBSTITUTE`).
#[must_use]
pub fn html_escape(s: &[u8]) -> Vec<u8> {
    if std::str::from_utf8(s).is_err() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(s.len());
    for &c in s {
        match c {
            b'&' => out.extend_from_slice(b"&amp;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'"' => out.extend_from_slice(b"&quot;"),
            b'\'' => out.extend_from_slice(b"&#039;"),
            _ => out.push(c),
        }
    }
    out
}

/// The output of `phpinfo()`-style printing.
#[derive(Debug, Default, Clone)]
pub struct InfoPrinter {
    /// `sapi_module.phpinfo_as_text`.
    pub as_text: bool,
    /// Accumulated output.
    pub out: Vec<u8>,
}

impl InfoPrinter {
    /// A printer for text (CLI) or HTML output.
    #[must_use]
    pub fn new(as_text: bool) -> Self {
        InfoPrinter {
            as_text,
            out: Vec::new(),
        }
    }

    /// `php_info_print`.
    pub fn print(&mut self, s: impl AsRef<[u8]>) {
        self.out.extend_from_slice(s.as_ref());
    }

    fn print_esc(&mut self, s: &[u8]) {
        let e = html_escape(s);
        self.out.extend_from_slice(&e);
    }

    /// `php_info_print_table_start`.
    pub fn table_start(&mut self) {
        self.print(if self.as_text { "\n" } else { "<table>\n" });
    }

    /// `php_info_print_table_end`.
    pub fn table_end(&mut self) {
        if !self.as_text {
            self.print("</table>\n");
        }
    }

    /// `php_info_print_box_start`.
    pub fn box_start(&mut self, flag: bool) {
        self.table_start();
        if flag {
            if !self.as_text {
                self.print("<tr class=\"h\"><td>\n");
            }
        } else if !self.as_text {
            self.print("<tr class=\"v\"><td>\n");
        } else {
            self.print("\n");
        }
    }

    /// `php_info_print_box_end`.
    pub fn box_end(&mut self) {
        if !self.as_text {
            self.print("</td></tr>\n");
        }
        self.table_end();
    }

    /// `php_info_print_hr`.
    pub fn hr(&mut self) {
        self.print(if self.as_text {
            "\n\n _______________________________________________________________________\n\n"
        } else {
            "<hr />\n"
        });
    }

    /// `php_info_print_table_colspan_header`.
    pub fn colspan_header(&mut self, num_cols: usize, header: &str) {
        if self.as_text {
            let spaces = 74i64 - i64::try_from(header.len()).unwrap_or(0);
            let pad = usize::try_from((spaces / 2).max(1)).unwrap_or(1);
            self.print(format!("{:>pad$}{header}{:>pad$}\n", " ", " "));
        } else {
            self.print(format!(
                "<tr class=\"h\"><th colspan=\"{num_cols}\">{header}</th></tr>\n"
            ));
        }
    }

    /// `php_info_print_table_header`.
    pub fn table_header(&mut self, cols: &[&[u8]]) {
        if !self.as_text {
            self.print("<tr class=\"h\">");
        }
        for (i, c) in cols.iter().enumerate() {
            let c: &[u8] = if c.is_empty() { b" " } else { c };
            if self.as_text {
                self.print(c);
                self.print(if i + 1 < cols.len() { " => " } else { "\n" });
            } else {
                self.print("<th>");
                self.print(c);
                self.print("</th>");
            }
        }
        if !self.as_text {
            self.print("</tr>\n");
        }
    }

    /// `php_info_print_table_row_ex` (`value_class` is "v" for
    /// `php_info_print_table_row`).
    pub fn table_row_ex(&mut self, value_class: &str, cols: &[&[u8]]) {
        if !self.as_text {
            self.print("<tr>");
        }
        for (i, c) in cols.iter().enumerate() {
            if !self.as_text {
                self.print(format!(
                    "<td class=\"{}\">",
                    if i == 0 { "e" } else { value_class }
                ));
            }
            if c.is_empty() {
                self.print(if self.as_text { " " } else { "<i>no value</i>" });
            } else if self.as_text {
                self.print(c);
                if i + 1 < cols.len() {
                    self.print(" => ");
                }
            } else {
                self.print_esc(c);
            }
            if !self.as_text {
                self.print(" </td>");
            } else if i + 1 == cols.len() {
                self.print("\n");
            }
        }
        if !self.as_text {
            self.print("</tr>\n");
        }
    }

    /// `php_info_print_table_row`.
    pub fn table_row(&mut self, cols: &[&[u8]]) {
        self.table_row_ex("v", cols);
    }

    /// The `SECTION(name)` macro.
    pub fn section(&mut self, name: &str) {
        if self.as_text {
            self.table_start();
            self.table_header(&[name.as_bytes()]);
            self.table_end();
        } else {
            self.print(format!("<h2>{name}</h2>\n"));
        }
    }

    /// `php_info_print_module()` header for a module with an info
    /// function or version.
    pub fn module_header(&mut self, name: &str) {
        if self.as_text {
            self.table_start();
            self.table_header(&[name.as_bytes()]);
            self.table_end();
        } else {
            let mut enc = Vec::new();
            crate::query::url_encode_into(&mut enc, name.as_bytes(), false);
            let enc = String::from_utf8_lossy(&enc).to_ascii_lowercase();
            self.print(format!(
                "<h2><a name=\"module_{enc}\" href=\"#module_{enc}\">{name}</a></h2>\n"
            ));
        }
    }

    /// A module without info function or version (in the "Additional
    /// Modules" table).
    pub fn module_plain(&mut self, name: &str) {
        if self.as_text {
            self.print(format!("{name}\n"));
        } else {
            self.print(format!("<tr><td class=\"v\">{name}</td></tr>\n"));
        }
    }

    /// `php_info_print_stream_hash()`; `None` = "disabled".
    pub fn stream_hash(&mut self, name: &str, entries: Option<&[Vec<u8>]>) {
        match entries {
            None => self.table_row(&[name.as_bytes(), b"disabled"]),
            Some([]) => {
                let n = format!("Registered {name}");
                self.table_row(&[n.as_bytes(), b"none registered"]);
            }
            Some(list) => {
                if self.as_text {
                    self.print(format!("\nRegistered {name} => "));
                } else {
                    self.print(format!(
                        "<tr><td class=\"e\">Registered {name}</td><td class=\"v\">"
                    ));
                }
                for (i, k) in list.iter().enumerate() {
                    if i > 0 {
                        self.print(", ");
                    }
                    if self.as_text {
                        self.print(k);
                    } else {
                        self.print_esc(k);
                    }
                }
                if !self.as_text {
                    self.print("</td></tr>\n");
                }
            }
        }
    }

    /// `display_ini_entries()`: rows are `(name, local, master)` display
    /// strings as produced by the entries' displayers ([`ini_display`]
    /// for plain entries).
    pub fn ini_entries(&mut self, rows: &[(Vec<u8>, Vec<u8>, Vec<u8>)]) {
        if rows.is_empty() {
            return;
        }
        self.table_start();
        self.table_header(&[b"Directive", b"Local Value", b"Master Value"]);
        for (name, local, master) in rows {
            if self.as_text {
                self.print(name);
                self.print(" => ");
                self.print(local);
                self.print(" => ");
                self.print(master);
                self.print("\n");
            } else {
                self.print("<tr><td class=\"e\">");
                self.print(name);
                self.print("</td><td class=\"v\">");
                self.print(local);
                self.print("</td><td class=\"v\">");
                self.print(master);
                self.print("</td></tr>\n");
            }
        }
        self.table_end();
    }

    /// One `$_SERVER['...']`-style variable row of `php_print_gpcse_array`.
    pub fn gpcse_array(
        &mut self,
        name: &str,
        arr: &ZArray,
        ctx: &mut (impl RuntimeContext + ?Sized),
    ) -> RtResult<()> {
        for (k, v) in arr.iter() {
            if !self.as_text {
                self.print("<tr><td class=\"e\">");
            }
            self.print(format!("${name}['"));
            match k {
                php_runtime::ArrayKey::Str(s) => {
                    if self.as_text {
                        self.print(s.as_bytes());
                    } else {
                        self.print_esc(s.as_bytes());
                    }
                }
                php_runtime::ArrayKey::Int(i) => self.print(i.to_string()),
            }
            self.print("']");
            self.print(if self.as_text {
                " => "
            } else {
                "</td><td class=\"v\">"
            });
            let v = v.deref();
            if let Value::Array(_) = v {
                let s = php_runtime::dump::print_r(&v, ctx)?;
                if self.as_text {
                    self.print(&s);
                } else {
                    self.print("<pre>");
                    self.print_esc(&s);
                    self.print("</pre>");
                }
            } else {
                let s = v.to_zstr(ctx)?;
                if self.as_text {
                    self.print(s.as_bytes());
                } else if s.is_empty() {
                    self.print("<i>no value</i>");
                } else {
                    self.print_esc(s.as_bytes());
                }
            }
            self.print(if self.as_text { "\n" } else { "</td></tr>\n" });
        }
        Ok(())
    }
}

/// `php_ini_displayer_cb()` for entries without a custom displayer.
#[must_use]
pub fn ini_display(value: Option<&[u8]>, as_text: bool) -> Vec<u8> {
    match value {
        Some(v) if !v.is_empty() => {
            if as_text {
                v.to_vec()
            } else {
                crate::highlight::html_puts_public(v)
            }
        }
        _ => {
            if as_text {
                b"no value".to_vec()
            } else {
                b"<i>no value</i>".to_vec()
            }
        }
    }
}

/// A module as listed by `phpinfo(INFO_MODULES)`.
pub trait InfoModule {
    /// The module name.
    fn name(&self) -> &str;
    /// Whether it has an info function or a version (gets its own
    /// section) rather than being listed under "Additional Modules".
    fn has_section(&self) -> bool;
    /// Writes the module's section body (its `info_func`, or the version
    /// table plus INI entries).
    fn print_body(&self, p: &mut InfoPrinter);
}

/// Everything [`print_info`] needs from the engine.
pub struct InfoSource<'a> {
    /// `PHP_VERSION`.
    pub php_version: &'a str,
    /// `php_uname('a')`.
    pub uname: &'a [u8],
    /// Build date.
    pub build_date: &'a str,
    /// Further `(label, value)` rows after "Build Date" (Build System,
    /// Compiler, Architecture, Configure Command...).
    pub build_rows: &'a [(&'a str, &'a str)],
    /// `sapi_module.pretty_name`.
    pub server_api: Option<&'a str>,
    /// `PHP_CONFIG_FILE_PATH`.
    pub config_file_path: &'a str,
    /// `php_ini_opened_path`.
    pub loaded_ini: Option<&'a [u8]>,
    /// `php_ini_scanned_path`.
    pub scan_dir: Option<&'a [u8]>,
    /// `php_ini_scanned_files`.
    pub scanned_files: Option<&'a [u8]>,
    /// The remaining general rows (from "PHP API" to "`DTrace` Support").
    pub engine_rows: &'a [(&'a str, &'a str)],
    /// Registered stream wrappers.
    pub stream_wrappers: &'a [Vec<u8>],
    /// Registered socket transports.
    pub stream_transports: &'a [Vec<u8>],
    /// Registered stream filters.
    pub stream_filters: &'a [Vec<u8>],
    /// `get_zend_version()`.
    pub zend_version: &'a str,
    /// Core INI rows (`display_ini_entries(NULL)`).
    pub core_ini: &'a [(Vec<u8>, Vec<u8>, Vec<u8>)],
    /// Loaded modules (sorted case-insensitively by the printer).
    pub modules: &'a [&'a dyn InfoModule],
    /// `PHP_SELF`/`PHP_AUTH_*` string globals present.
    pub php_vars: &'a [(&'a str, Vec<u8>)],
    /// `$_REQUEST`, `$_GET`, ... arrays (name without `$`).
    pub gpcse: &'a [(&'a str, ZArray)],
}

impl std::fmt::Debug for InfoSource<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InfoSource")
            .field("php_version", &self.php_version)
            .finish_non_exhaustive()
    }
}

const LICENSE_TEXT: &str = "\nLicense\n\nPHP is free software. You may redistribute it and/or modify it under the\nterms of the Modified BSD License (SPDX-License-Identifier: BSD-3-Clause).\n\nCopyright \u{a9} The PHP Group and Contributors.\nCopyright \u{a9} Zend Technologies Ltd., a subsidiary company of\n    Perforce Software, Inc.\n\nRedistribution and use in source and binary forms, with or without\nmodification, are permitted provided that the following conditions are met:\n\n1. Redistributions of source code must retain the above copyright notice, this\n   list of conditions and the following disclaimer.\n\n2. Redistributions in binary form must reproduce the above copyright notice,\n   this list of conditions and the following disclaimer in the documentation\n   and/or other materials provided with the distribution.\n\n3. Neither the name of the copyright holder nor the names of its\n   contributors may be used to endorse or promote products derived from\n   this software without specific prior written permission.\n\nTHIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\"\nAND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE\nIMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE\nDISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE\nFOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL\nDAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR\nSERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER\nCAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,\nOR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE\nOF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.\n";

/// `php_info_print_css()` (`ext/standard/css.c`).
pub const INFO_CSS: &str = concat!(
    "body {background-color: #fff; color: #222; font-family: sans-serif;}\n",
    "pre {margin: 0; font-family: monospace;}\n",
    "a {color: inherit;}\n",
    "a:hover {text-decoration: none;}\n",
    "table {border-collapse: collapse; border: 0; width: 934px; box-shadow: 1px 2px 3px rgba(0, 0, 0, 0.2);}\n",
    ".center {text-align: center;}\n",
    ".center table {margin: 1em auto; text-align: left;}\n",
    ".center th {text-align: center !important;}\n",
    "td, th {border: 1px solid #666; font-size: 75%; vertical-align: baseline; padding: 4px 5px;}\n",
    "th {position: sticky; top: 0; background: inherit;}\n",
    "h1 {font-size: 150%;}\n",
    "h2 {font-size: 125%;}\n",
    "h2 > a {text-decoration: none;}\n",
    "h2 > a:hover {text-decoration: underline;}\n",
    ".p {text-align: left;}\n",
    ".e {background-color: #ccf; width: 300px; font-weight: bold;}\n",
    ".h {background-color: #99c; font-weight: bold;}\n",
    ".v {background-color: #ddd; max-width: 300px; overflow-x: auto; word-wrap: break-word;}\n",
    ".v i {color: #999;}\n",
    "img {float: right; border: 0;}\n",
    "hr {width: 934px; background-color: #ccc; border: 0; height: 1px;}\n",
    ":root {--php-dark-grey: #333; --php-dark-blue: #4F5B93; --php-medium-blue: #8892BF; --php-light-blue: #E2E4EF; --php-accent-purple: #793862}",
    "@media (prefers-color-scheme: dark) {\n",
    "  body {background: var(--php-dark-grey); color: var(--php-light-blue)}\n",
    "  .h td, td.e, th {border-color: #606A90}\n",
    "  td {border-color: #505153}\n",
    "  .e {background-color: #404A77}\n",
    "  .h {background-color: var(--php-dark-blue)}\n",
    "  .v {background-color: var(--php-dark-grey)}\n",
    "  hr {background-color: #505153}\n",
    "}\n",
);

/// `php_print_info_htmlhead(title)`: the HTML page head shared by
/// `phpinfo()` and `phpcredits(CREDITS_FULLPAGE)`.
#[must_use]
pub fn html_head(php_version: &str, title: &str) -> Vec<u8> {
    format!(
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \"DTD/xhtml1-transitional.dtd\">\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\"><head>\n\
         <style type=\"text/css\">\n{INFO_CSS}</style>\n\
         <title>PHP {php_version} - {title}</title>\
         <meta name=\"ROBOTS\" content=\"NOINDEX,NOFOLLOW,NOARCHIVE\" /></head>\n\
         <body><div class=\"center\">\n"
    )
    .into_bytes()
}

/// The HTML license box of `phpinfo(INFO_LICENSE)`.
const LICENSE_HTML: &str = concat!(
    "<p>\n",
    "PHP is free software. You may redistribute it and/or modify it under the ",
    "terms of the Modified BSD License (SPDX-License-Identifier: BSD-3-Clause).\n",
    "</p>\n",
    "<p>\n",
    "Copyright &copy; The PHP Group and Contributors.<br>\n",
    "Copyright &copy; Zend Technologies Ltd., a subsidiary company of Perforce Software, Inc.\n",
    "</p>\n",
    "<p>\n",
    "Redistribution and use in source and binary forms, with or without ",
    "modification, are permitted provided that the following conditions are met:\n",
    "</p>\n",
    "<ol>\n",
    "<li>Redistributions of source code must retain the above copyright notice, this ",
    "list of conditions and the following disclaimer.</li>\n",
    "<li>Redistributions in binary form must reproduce the above copyright notice, ",
    "this list of conditions and the following disclaimer in the documentation ",
    "and/or other materials provided with the distribution.</li>\n",
    "<li>Neither the name of the copyright holder nor the names of its ",
    "contributors may be used to endorse or promote products derived from ",
    "this software without specific prior written permission.</li>\n",
    "</ol>\n",
    "<p>\n",
    "THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS \"AS IS\" ",
    "AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE ",
    "IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE ",
    "DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE ",
    "FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL ",
    "DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR ",
    "SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER ",
    "CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, ",
    "OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE ",
    "OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.\n",
    "</p>\n",
);

/// `php_print_info($flag)` in text mode (the CLI's `phpinfo_as_text`) or
/// as an HTML page (CGI, the built-in web server, Apache).
#[allow(clippy::too_many_lines)]
pub fn print_info(
    flag: i64,
    src: &InfoSource<'_>,
    as_text: bool,
    ctx: &mut (impl RuntimeContext + ?Sized),
) -> RtResult<Vec<u8>> {
    let mut p = InfoPrinter::new(as_text);
    if as_text {
        p.print("phpinfo()\n");
    } else {
        p.print(html_head(src.php_version, "phpinfo()"));
    }
    if flag & INFO_GENERAL != 0 {
        if as_text {
            p.table_row(&[b"PHP Version", src.php_version.as_bytes()]);
        } else {
            p.box_start(true);
            let egg = crate::sys::local_month_day() == Some((3, 1));
            p.print("<a href=\"https://www.php.net/\"><img src=\"");
            p.print(if egg {
                logos::PHP_EGG_LOGO_DATA_URI
            } else {
                logos::PHP_LOGO_DATA_URI
            });
            p.print("\" alt=\"PHP logo\" /></a>");
            p.print(format!(
                "<h1 class=\"p\">PHP Version {}</h1>\n",
                src.php_version
            ));
        }
        p.box_end();
        p.table_start();
        p.table_row(&[b"System", src.uname]);
        p.table_row(&[b"Build Date", src.build_date.as_bytes()]);
        for (k, v) in src.build_rows {
            p.table_row(&[k.as_bytes(), v.as_bytes()]);
        }
        if let Some(api) = src.server_api {
            p.table_row(&[b"Server API", api.as_bytes()]);
        }
        p.table_row(&[b"Virtual Directory Support", b"disabled"]);
        p.table_row(&[
            b"Configuration File (php.ini) Path",
            src.config_file_path.as_bytes(),
        ]);
        p.table_row(&[
            b"Loaded Configuration File",
            src.loaded_ini.unwrap_or(b"(none)"),
        ]);
        p.table_row(&[
            b"Scan this dir for additional .ini files",
            src.scan_dir.unwrap_or(b"(none)"),
        ]);
        p.table_row(&[
            b"Additional .ini files parsed",
            src.scanned_files.unwrap_or(b"(none)"),
        ]);
        for (k, v) in src.engine_rows {
            p.table_row(&[k.as_bytes(), v.as_bytes()]);
        }
        p.stream_hash("PHP Streams", Some(src.stream_wrappers));
        p.stream_hash("Stream Socket Transports", Some(src.stream_transports));
        p.stream_hash("Stream Filters", Some(src.stream_filters));
        p.table_end();
        p.box_start(false);
        if !as_text {
            p.print("<a href=\"https://www.zend.com/\"><img src=\"");
            p.print(logos::ZEND_LOGO_DATA_URI);
            p.print("\" alt=\"Zend logo\" /></a>\n");
        }
        p.print("This program makes use of the Zend Scripting Language Engine:");
        p.print(if as_text { "\n" } else { "<br />" });
        if as_text {
            p.print(src.zend_version);
        } else {
            p.print(crate::highlight::html_puts_public(
                src.zend_version.as_bytes(),
            ));
        }
        p.box_end();
    }
    if flag & INFO_CONFIGURATION != 0 {
        p.hr();
        if as_text {
            p.section("Configuration");
        } else {
            p.print("<h1>Configuration</h1>\n");
        }
        if flag & INFO_MODULES == 0 {
            p.section("PHP Core");
            p.ini_entries(src.core_ini);
        }
    }
    if flag & INFO_MODULES != 0 {
        let mut mods: Vec<&&dyn InfoModule> = src.modules.iter().collect();
        mods.sort_by(|a, b| {
            a.name()
                .to_ascii_lowercase()
                .cmp(&b.name().to_ascii_lowercase())
        });
        for m in &mods {
            if m.has_section() {
                p.module_header(m.name());
                m.print_body(&mut p);
            }
        }
        p.section("Additional Modules");
        p.table_start();
        p.table_header(&[b"Module Name"]);
        for m in &mods {
            if !m.has_section() {
                p.module_plain(m.name());
            }
        }
        p.table_end();
    }
    if flag & INFO_ENVIRONMENT != 0 {
        p.section("Environment");
        p.table_start();
        p.table_header(&[b"Variable", b"Value"]);
        for (k, v) in crate::sys::environ_entries() {
            p.table_row(&[&k, &v]);
        }
        p.table_end();
    }
    if flag & INFO_VARIABLES != 0 {
        p.section("PHP Variables");
        p.table_start();
        p.table_header(&[b"Variable", b"Value"]);
        for (k, v) in src.php_vars {
            p.table_row(&[k.as_bytes(), v]);
        }
        for (name, arr) in src.gpcse {
            p.gpcse_array(name, arr, ctx)?;
        }
        p.table_end();
    }
    if flag & INFO_LICENSE != 0 {
        if as_text {
            p.print(LICENSE_TEXT);
        } else {
            p.section("License");
            p.box_start(false);
            p.print(LICENSE_HTML);
            p.box_end();
        }
    }
    if !as_text {
        p.print("</div></body></html>");
    }
    Ok(p.out)
}
