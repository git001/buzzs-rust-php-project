// buzzs-rust-php-project - Copyright (c) 2026 Aleks <al-git001@none.at>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The build id of the bytecode cache (`PHPRUST_BUILD_ID`): a hash of the
//! sources of this crate and of every workspace crate it depends on by
//! path, the part of `zend_system_id` that `ZEND_BIN_ID` fills in php-src.
//! A cache entry written by a build whose compiler, optimizer or op
//! semantics differ lands under another system id and is never read, even
//! when `FORMAT_VERSION` and the package version stay the same.
//!
//! The sources, not the binary: two builds from the same sources (CLI,
//! CGI, FPM; debug and release) share their entries, as php-src's SAPIs do.

use std::collections::BTreeSet;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

use twox_hash::XxHash3_64;

/// The `path = "…"` dependencies in a manifest's `[dependencies]` table.
fn path_deps(manifest_dir: &Path) -> Vec<PathBuf> {
    let text = std::fs::read_to_string(manifest_dir.join("Cargo.toml")).expect("read Cargo.toml");
    let mut in_deps = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        if let Some(rest) = line.split("path = \"").nth(1) {
            if let Some(rel) = rest.split('"').next() {
                out.push(manifest_dir.join(rel));
            }
        }
    }
    out
}

/// Every file below `dir`, sorted, so the hash does not depend on the
/// order the file system lists them in.
fn files_below(dir: &Path, out: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_below(&path, out);
        } else {
            out.insert(path);
        }
    }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"));

    // This crate and its path dependencies, transitively.
    let mut crates = BTreeSet::new();
    let mut todo = vec![manifest_dir.canonicalize().expect("manifest dir")];
    while let Some(dir) = todo.pop() {
        if !crates.insert(dir.clone()) {
            continue;
        }
        for dep in path_deps(&dir) {
            todo.push(dep.canonicalize().expect("path dependency"));
        }
    }

    let mut hasher = XxHash3_64::with_seed(0);
    for dir in &crates {
        let name = dir.file_name().expect("crate dir name");
        for part in ["Cargo.toml", "build.rs", "src"] {
            let root = dir.join(part);
            println!("cargo:rerun-if-changed={}", root.display());
            let mut files = BTreeSet::new();
            if root.is_dir() {
                files_below(&root, &mut files);
            } else if root.is_file() {
                files.insert(root.clone());
            }
            for file in files {
                // The path relative to the workspace, so two checkouts of
                // the same sources get the same id.
                let rel = file.strip_prefix(dir).expect("below crate dir");
                hasher.write(name.as_encoded_bytes());
                hasher.write(rel.as_os_str().as_encoded_bytes());
                hasher.write(&[0]);
                let bytes = std::fs::read(&file).expect("read source");
                hasher.write(&(bytes.len() as u64).to_le_bytes());
                hasher.write(&bytes);
            }
        }
    }
    println!("cargo:rustc-env=PHPRUST_BUILD_ID={:016x}", hasher.finish());
}
