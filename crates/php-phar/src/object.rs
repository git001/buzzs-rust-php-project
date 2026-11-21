// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! The `Phar`, `PharData` and `PharFileInfo` methods (`phar_object.c`).
//!
//! Each method is a Rust function with plain inputs; PHP values the VM
//! must handle itself (iterators, streams, metadata (un)serialization,
//! object construction) are passed in or returned as data:
//!
//! * metadata is set as `(value, serialized bytes)` and read back as a
//!   [`MetadataGet`];
//! * methods that create objects (`offsetGet()`, `convertTo*()`,
//!   `compress()`...) return the constructor argument;
//! * `buildFromIterator()` is driven item by item through [`BuildState`].
//!
//! Warnings go through `php_streams::diag`; exceptions are returned as
//! [`PharError`].

use std::rc::Rc;

use php_runtime::Value;

use crate::archive::{ArchiveRef, Entry, FpType, Metadata};
use crate::consts::{
    API_VERSION_STR, ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, ENT_COMPRESSION_MASK, ENT_PERM_MASK,
    FILE_COMPRESSED_BZ2, FILE_COMPRESSED_GZ, FILE_COMPRESSED_NONE, FILE_COMPRESSION_MASK,
    FORMAT_PHAR, FORMAT_SAME, FORMAT_TAR, FORMAT_ZIP, SIG_MD5, SIG_OPENSSL, SIG_OPENSSL_SHA256,
    SIG_OPENSSL_SHA512, SIG_SHA1, SIG_SHA256, SIG_SHA512, TAR_DIR, TAR_FILE,
};
use crate::error::{PharError, PharResult};
use crate::registry::{self as reg, EntryRef};
use crate::util::{is_magic_phar, path_check, s};

/// What `getMetadata()` should return.
///
/// For [`MetadataGet::Serialized`] the caller unserializes the data with the
/// user's options; diagnostics are prefixed with
/// [`PharObject::METADATA_FUNCTION`] / [`PharFileInfo::METADATA_FUNCTION`]
/// (`"Phar::getMetadata(): Error at offset ..."`), not `unserialize()`.
#[derive(Debug, Clone)]
pub enum MetadataGet {
    /// No metadata (`NULL`).
    None,
    /// A copy of the value set with `setMetadata()`.
    Value(Value),
    /// Serialized data to unserialize (with the caller's options) — the
    /// `method` name is used in unserialize warnings.
    Serialized(Vec<u8>),
}

fn metadata_get(m: &Metadata, has_options: bool) -> MetadataGet {
    if !m.has_data() {
        return MetadataGet::None;
    }
    match (&m.value, &m.serialized) {
        (Some(v), _) if !has_options => MetadataGet::Value(v.clone()),
        (_, Some(sz)) => MetadataGet::Serialized(sz.clone()),
        (Some(v), None) => MetadataGet::Value(v.clone()),
        (None, None) => MetadataGet::None,
    }
}

fn flush_err(ar: &ArchiveRef) -> PharResult<()> {
    reg::flush(ar).map_err(PharError::phar)
}

fn now() -> u32 {
    crate::sys::entry_now32()
}

/// The archive behind a `Phar` / `PharData` object.
#[derive(Debug, Default)]
pub struct PharObject {
    archive: Option<ArchiveRef>,
    /// Whether the object is a `PharData`.
    pub is_data_class: bool,
    /// Whether `$this->setInfoClass()` keeps a `PharFileInfo` subclass
    /// (`offsetExists()` reports directories only then).
    pub info_class_is_pharfileinfo: bool,
}

/// The result of `Phar::__construct()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constructed {
    /// The path for the parent `RecursiveDirectoryIterator::__construct()`.
    pub iterator_path: Vec<u8>,
}

/// The result of a conversion (`convertToExecutable()` and friends): the
/// VM constructs `Phar` (or `PharData` when `is_data`) with `fname`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Converted {
    /// The new archive path.
    pub fname: Vec<u8>,
    /// Construct a `PharData` rather than a `Phar`.
    pub is_data: bool,
}

/// `Phar::getSignature()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureInfo {
    /// Upper-case hex hash.
    pub hash: Vec<u8>,
    /// `hash_type`.
    pub hash_type: String,
}

/// Files selector of `extractTo()`.
#[derive(Debug, Clone)]
pub enum ExtractFiles {
    /// Everything.
    All,
    /// One file, or a directory prefix ending in `/`.
    One(Vec<u8>),
    /// A list (`Err` marks a non-string element).
    List(Vec<Option<Vec<u8>>>),
}

impl PharObject {
    /// The function name used for metadata unserialize diagnostics (the
    /// same for `PharData`, as in PHP).
    pub const METADATA_FUNCTION: &'static str = "Phar::getMetadata";

    /// A new, uninitialised object.
    pub fn new(is_data_class: bool) -> Self {
        PharObject {
            archive: None,
            is_data_class,
            info_class_is_pharfileinfo: true,
        }
    }

    /// The archive (`PHAR_ARCHIVE_OBJECT()`).
    pub fn archive(&self) -> PharResult<ArchiveRef> {
        self.archive.clone().ok_or_else(|| {
            PharError::bad_method("Cannot call method on an uninitialized Phar object")
        })
    }

    /// Whether the constructor ran.
    pub fn is_initialized(&self) -> bool {
        self.archive.is_some()
    }

    /// `Phar::__construct()` / `PharData::__construct()`.
    pub fn construct(
        &mut self,
        fname: &[u8],
        alias: Option<&[u8]>,
        format: i64,
    ) -> PharResult<Constructed> {
        let is_data = self.is_data_class;
        if self.archive.is_some() {
            return Err(PharError::bad_method("Cannot call constructor twice"));
        }
        let (fname, entry) = match reg::split_fname(fname, i32::from(!is_data), 2) {
            Ok((arch, entry)) => (arch, Some(entry)),
            Err(_) => (fname.to_vec(), None),
        };
        let ar = reg::open_or_create_filename(&fname, alias, is_data, true).map_err(|e| {
            PharError::unexpected(e.unwrap_or_else(|| "Phar creation or opening failed".into()))
        })?;
        {
            let mut a = ar.borrow_mut();
            if is_data && a.is_tar && a.is_brandnew && format == FORMAT_ZIP {
                a.is_zip = true;
                a.is_tar = false;
            }
            if is_data != a.is_data {
                return Err(PharError::unexpected(if is_data {
                    "PharData class can only be used for non-executable tar and zip archives"
                } else {
                    "Phar class can only be used for executable tar and zip archives"
                }));
            }
        }
        reg::addref(&ar);
        let mut path = b"phar://".to_vec();
        path.extend_from_slice(&ar.borrow().fname);
        if let Some(e) = entry {
            path.extend_from_slice(&e);
        }
        self.archive = Some(ar);
        Ok(Constructed {
            iterator_path: path,
        })
    }

    /// Object destruction (`phar_spl_foreign_dtor`).
    pub fn release(&mut self) {
        if let Some(ar) = self.archive.take() {
            if !ar.borrow().is_persistent {
                reg::delref(&ar);
            }
        }
    }

    /// `Phar::count()`.
    pub fn count(&self) -> PharResult<i64> {
        Ok(crate::util::to_i64(self.archive()?.borrow().count()))
    }

    /// `Phar::isFileFormat()`.
    pub fn is_file_format(&self, format: i64) -> PharResult<bool> {
        let ar = self.archive()?;
        let a = ar.borrow();
        match format {
            FORMAT_TAR => Ok(a.is_tar),
            FORMAT_ZIP => Ok(a.is_zip),
            FORMAT_PHAR => Ok(!a.is_tar && !a.is_zip),
            _ => Err(PharError::phar("Unknown file format specified")),
        }
    }

    /// `Phar::getAlias()`.
    pub fn get_alias(&self) -> PharResult<Option<Vec<u8>>> {
        Ok(self.archive()?.borrow().alias.clone())
    }

    /// `Phar::getPath()`.
    pub fn get_path(&self) -> PharResult<Vec<u8>> {
        Ok(self.archive()?.borrow().fname.clone())
    }

    /// `Phar::getVersion()`.
    pub fn get_version(&self) -> PharResult<String> {
        Ok(self.archive()?.borrow().version.clone())
    }

    /// `Phar::getModified()`.
    pub fn get_modified(&self) -> PharResult<bool> {
        Ok(self.archive()?.borrow().is_modified)
    }

    /// `Phar::startBuffering()`.
    pub fn start_buffering(&self) -> PharResult<()> {
        self.archive()?.borrow_mut().donotflush = true;
        Ok(())
    }

    /// `Phar::isBuffering()`.
    pub fn is_buffering(&self) -> PharResult<bool> {
        Ok(self.archive()?.borrow().donotflush)
    }

    /// `Phar::stopBuffering()`.
    pub fn stop_buffering(&self) -> PharResult<()> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot write out phar archive, phar is read-only",
            ));
        }
        ar.borrow_mut().donotflush = false;
        flush_err(&ar)
    }

    /// `Phar::isCompressed()`: `Phar::GZ`, `Phar::BZ2` or `None` (false).
    pub fn is_compressed(&self) -> PharResult<Option<i64>> {
        let ar = self.archive()?;
        let f = ar.borrow().flags;
        Ok(if f & FILE_COMPRESSED_GZ != 0 {
            Some(i64::from(ENT_COMPRESSED_GZ))
        } else if f & FILE_COMPRESSED_BZ2 != 0 {
            Some(i64::from(ENT_COMPRESSED_BZ2))
        } else {
            None
        })
    }

    /// `Phar::isWritable()`.
    pub fn is_writable(&self) -> PharResult<bool> {
        let ar = self.archive()?;
        let a = ar.borrow();
        if !a.is_writeable {
            return Ok(false);
        }
        match php_streams::stat::stat_path(&a.fname) {
            Ok(sb) => Ok(sb.mode & 0o222 != 0),
            Err(_) => Ok(a.is_brandnew),
        }
    }

    /// `Phar::getSignature()`; `None` is `false`.
    pub fn get_signature(&self) -> PharResult<Option<SignatureInfo>> {
        let ar = self.archive()?;
        let a = ar.borrow();
        Ok(a.signature.as_ref().map(|h| SignatureInfo {
            hash: h.clone(),
            hash_type: match a.sig_flags {
                SIG_MD5 | SIG_SHA1 | SIG_SHA256 | SIG_SHA512 | SIG_OPENSSL | SIG_OPENSSL_SHA256
                | SIG_OPENSSL_SHA512 => crate::signature::type_name(a.sig_flags).to_string(),
                other => format!("Unknown ({other})"),
            },
        }))
    }

    /// `Phar::delete()`.
    pub fn delete(&mut self, name: &[u8]) -> PharResult<bool> {
        let ar = self.writable_archive("Cannot write out phar archive, phar is read-only")?;
        {
            let mut a = ar.borrow_mut();
            match a.manifest.get_mut(name) {
                Some(e) if e.is_deleted => return Ok(true),
                Some(e) => {
                    e.is_deleted = true;
                    e.is_modified = true;
                    a.is_modified = true;
                }
                None => {
                    return Err(PharError::bad_method(format!(
                        "Entry {} does not exist and cannot be deleted",
                        s(name)
                    )))
                }
            }
        }
        flush_err(&ar)?;
        Ok(true)
    }

    /// Checks `phar.readonly` (`UnexpectedValueException` `msg`) and makes a
    /// cached archive writable.
    fn writable_archive(&mut self, msg: &str) -> PharResult<ArchiveRef> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(msg));
        }
        self.cow()
    }

    fn cow(&mut self) -> PharResult<ArchiveRef> {
        let ar = self.archive()?;
        if !ar.borrow().is_persistent {
            return Ok(ar);
        }
        let new = reg::copy_on_write(&ar).ok_or_else(|| {
            PharError::phar(format!(
                "phar \"{}\" is persistent, unable to copy on write",
                s(&ar.borrow().fname)
            ))
        })?;
        self.archive = Some(Rc::clone(&new));
        Ok(new)
    }

    /// `Phar::setAlias()`.
    pub fn set_alias(&mut self, new_alias: &[u8]) -> PharResult<bool> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot write out phar archive, phar is read-only",
            ));
        }
        {
            let a = ar.borrow();
            if a.is_data {
                return Err(PharError::unexpected(if a.is_tar {
                    "A Phar alias cannot be set in a plain tar archive"
                } else {
                    "A Phar alias cannot be set in a plain zip archive"
                }));
            }
            if a.alias.as_deref() == Some(new_alias) {
                return Ok(true);
            }
        }
        if let Some(fd) = reg::find_by_alias(new_alias) {
            if !reg::free_alias(&fd) {
                return Err(PharError::phar(format!(
                    "alias \"{}\" is already used for archive \"{}\" and cannot be used for other archives",
                    s(new_alias),
                    s(&fd.borrow().fname)
                )));
            }
        } else if !crate::util::validate_alias(new_alias) {
            return Err(PharError::unexpected(format!(
                "Invalid alias \"{}\" specified for phar \"{}\"",
                s(new_alias),
                s(&ar.borrow().fname)
            )));
        }
        let ar = self.cow()?;
        let (old_alias, old_temp) = {
            let a = ar.borrow();
            (a.alias.clone(), a.is_temporary_alias)
        };
        let mut readd = false;
        if let Some(oa) = old_alias.as_ref().filter(|a| !a.is_empty()) {
            if reg::find_by_alias(oa).is_some() {
                reg::alias_map_remove(oa);
                readd = true;
            }
        }
        {
            let mut a = ar.borrow_mut();
            a.alias = (!new_alias.is_empty()).then(|| new_alias.to_vec());
            a.is_temporary_alias = false;
        }
        if let Err(e) = reg::flush(&ar) {
            let mut a = ar.borrow_mut();
            a.alias.clone_from(&old_alias);
            a.is_temporary_alias = old_temp;
            drop(a);
            if readd {
                if let Some(oa) = old_alias {
                    reg::alias_map_add(&oa, &ar);
                }
            }
            return Err(PharError::phar(e));
        }
        reg::alias_map_add(new_alias, &ar);
        Ok(true)
    }

    fn stub_checks(&self, ro_msg: &str) -> PharResult<ArchiveRef> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(ro_msg));
        }
        let a = ar.borrow();
        if a.is_data {
            return Err(PharError::unexpected(if a.is_tar {
                "A Phar stub cannot be set in a plain tar archive"
            } else {
                "A Phar stub cannot be set in a plain zip archive"
            }));
        }
        drop(a);
        Ok(ar)
    }

    /// `Phar::setStub(string $stub)`.
    pub fn set_stub(&mut self, stub: &[u8]) -> PharResult<bool> {
        self.stub_checks("Cannot change stub, phar is read-only")?;
        let ar = self.cow()?;
        reg::flush_ex(&ar, Some(stub), false).map_err(PharError::phar)?;
        Ok(true)
    }

    /// `Phar::setStub(resource $stub, int $length)` (deprecated; the VM
    /// emits the deprecation and reads the stream). `None` content means
    /// the stream could not be read.
    pub fn set_stub_from_stream(&mut self, content: Option<&[u8]>) -> PharResult<bool> {
        self.stub_checks("Cannot change stub, phar is read-only")?;
        let ar = self.cow()?;
        let Some(stub) = content else {
            return Err(PharError::phar(format!(
                "unable to read resource to copy stub to new phar \"{}\"",
                s(&ar.borrow().fname)
            )));
        };
        if let Err(e) = reg::flush_ex(&ar, Some(stub), false) {
            // PHP reports the error but still returns true here.
            return Err(PharError::phar(e));
        }
        Ok(true)
    }

    /// `Phar::setDefaultStub()`.
    pub fn set_default_stub(
        &mut self,
        index: Option<&[u8]>,
        webindex: Option<&[u8]>,
    ) -> PharResult<bool> {
        let ar = self.archive()?;
        {
            let a = ar.borrow();
            if a.is_data {
                return Err(PharError::unexpected(if a.is_tar {
                    "A Phar stub cannot be set in a plain tar archive"
                } else {
                    "A Phar stub cannot be set in a plain zip archive"
                }));
            }
            if (index.is_some() || webindex.is_some()) && (a.is_tar || a.is_zip) {
                return Err(PharError::arg(
                    if index.is_some() { 1 } else { 2 },
                    if index.is_some() {
                        "Phar::setDefaultStub(): Argument #1 ($index) must be null for a tar- or zip-based phar stub, string given"
                    } else {
                        "Phar::setDefaultStub(): Argument #2 ($webIndex) must be null for a tar- or zip-based phar stub, string given"
                    },
                ));
            }
        }
        if reg::readonly() {
            return Err(PharError::unexpected("Cannot change stub: phar.readonly=1"));
        }
        let (is_tar, is_zip) = {
            let a = ar.borrow();
            (a.is_tar, a.is_zip)
        };
        let stub = if !is_tar && !is_zip {
            Some(crate::stub::create_default_stub(index, webindex).map_err(PharError::unexpected)?)
        } else {
            None
        };
        let ar = self.cow()?;
        reg::flush_ex(&ar, stub.as_deref(), true).map_err(PharError::phar)?;
        Ok(true)
    }

    /// `Phar::setSignatureAlgorithm()`.
    pub fn set_signature_algorithm(&mut self, algo: i64, key: Option<&[u8]>) -> PharResult<()> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot set signature algorithm, phar is read-only",
            ));
        }
        let valid = [
            SIG_SHA256,
            SIG_SHA512,
            SIG_MD5,
            SIG_SHA1,
            SIG_OPENSSL,
            SIG_OPENSSL_SHA256,
            SIG_OPENSSL_SHA512,
        ];
        let Some(&algo) = valid.iter().find(|&&v| i64::from(v) == algo) else {
            return Err(PharError::unexpected(
                "Unknown signature algorithm specified",
            ));
        };
        let ar = self.cow()?;
        {
            let mut a = ar.borrow_mut();
            a.sig_flags = algo;
            a.is_modified = true;
        }
        reg::set_openssl_privatekey(key.unwrap_or_default().to_vec());
        flush_err(&ar)
    }

    /// `Phar::getStub()`.
    pub fn get_stub(&self) -> PharResult<Vec<u8>> {
        let ar = self.archive()?;
        let a = ar.borrow();
        let fname = a.fname.clone();
        if a.is_tar || a.is_zip {
            let Some(stub) = a.manifest.get(b".phar/stub.php").cloned() else {
                return Ok(Vec::new());
            };
            let compressed = stub.flags & ENT_COMPRESSION_MASK != 0;
            let img = if a.fp.is_some() && !a.is_brandnew && !compressed {
                a.fp.clone()
            } else {
                match crate::sys::read_file(&fname) {
                    Ok(d) => Some(Rc::new(d)),
                    Err(_) => {
                        return Err(PharError::unexpected(format!(
                            "phar error: unable to open phar \"{}\"",
                            s(&fname)
                        )))
                    }
                }
            };
            let img = img.unwrap_or_default();
            let off = usize::try_from(stub.offset_abs).unwrap_or(usize::MAX);
            let data = if compressed {
                let end = off
                    .saturating_add(stub.compressed_filesize as usize)
                    .min(img.len());
                let raw = img.get(off..end).unwrap_or_default();
                let out = if stub.flags & ENT_COMPRESSED_GZ != 0 {
                    crate::compress::inflate_raw(raw).unwrap_or_default()
                } else {
                    crate::bz2::decompress(raw).0
                };
                out.into_iter()
                    .take(stub.uncompressed_filesize as usize)
                    .collect::<Vec<u8>>()
            } else {
                img.get(off..off.saturating_add(stub.uncompressed_filesize as usize))
                    .map(<[u8]>::to_vec)
                    .unwrap_or_default()
            };
            if data.len() != stub.uncompressed_filesize as usize {
                return Err(PharError::runtime("Unable to read stub"));
            }
            return Ok(data);
        }
        let len = usize::try_from(a.halt_offset).unwrap_or(usize::MAX);
        let img = if a.fp.is_some() && !a.is_brandnew {
            a.fp.clone()
        } else {
            crate::sys::read_file(&fname).ok().map(Rc::new)
        };
        let Some(img) = img else {
            return Err(PharError::runtime("Unable to read stub"));
        };
        img.get(..len)
            .map(<[u8]>::to_vec)
            .ok_or_else(|| PharError::runtime("Unable to read stub"))
    }

    /// `Phar::hasMetadata()`.
    pub fn has_metadata(&self) -> PharResult<bool> {
        Ok(self.archive()?.borrow().metadata.has_data())
    }

    /// `Phar::getMetadata()`.
    pub fn get_metadata(&self, has_options: bool) -> PharResult<MetadataGet> {
        Ok(metadata_get(
            &self.archive()?.borrow().metadata,
            has_options,
        ))
    }

    /// `Phar::setMetadata()`: `serialized` is `serialize($value)`.
    pub fn set_metadata(&mut self, value: Value, serialized: Vec<u8>) -> PharResult<()> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        let ar = self.cow()?;
        {
            let mut a = ar.borrow_mut();
            a.metadata = Metadata {
                serialized: Some(serialized),
                value: Some(value),
            };
            a.is_modified = true;
        }
        flush_err(&ar)
    }

    /// `Phar::delMetadata()`.
    pub fn del_metadata(&mut self) -> PharResult<bool> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        if !ar.borrow().metadata.has_data() {
            return Ok(true);
        }
        {
            let mut a = ar.borrow_mut();
            a.metadata.clear();
            a.is_modified = true;
        }
        flush_err(&ar)?;
        Ok(true)
    }

    // ------------------------------------------------------- ArrayAccess

    /// `Phar::offsetExists()`.
    pub fn offset_exists(&self, name: &[u8]) -> PharResult<bool> {
        let ar = self.archive()?;
        let a = ar.borrow();
        if let Some(e) = a.manifest.get(name) {
            if e.is_deleted {
                return Ok(false);
            }
            return Ok(!is_magic_phar(name));
        }
        if !self.info_class_is_pharfileinfo {
            return Ok(false);
        }
        Ok(a.virtual_dirs.contains_key(name))
    }

    /// `Phar::offsetGet()`: returns the constructor argument for the info
    /// class (`phar://<archive>/<name>`).
    pub fn offset_get(&self, name: &[u8]) -> PharResult<Vec<u8>> {
        let ar = self.archive()?;
        if let Err(e) = reg::get_entry_info_dir(&ar, name, 1, false) {
            return Err(PharError::bad_method(format!(
                "Entry {} does not exist{}",
                s(name),
                e.map(|e| format!(", {e}")).unwrap_or_default()
            )));
        }
        let fname = ar.borrow().fname.clone();
        if name == b".phar/stub.php" {
            return Err(PharError::bad_method(format!(
                "Cannot get stub \".phar/stub.php\" directly in phar \"{}\", use getStub",
                s(&fname)
            )));
        }
        if name == b".phar/alias.txt" {
            return Err(PharError::bad_method(format!(
                "Cannot get alias \".phar/alias.txt\" directly in phar \"{}\", use getAlias",
                s(&fname)
            )));
        }
        if is_magic_phar(name) {
            return Err(PharError::bad_method(
                "Cannot directly get any files or directories in magic \".phar\" directory",
            ));
        }
        let mut p = b"phar://".to_vec();
        p.extend_from_slice(&fname);
        p.push(b'/');
        p.extend_from_slice(name);
        Ok(p)
    }

    fn set_checks(&self, name: &[u8]) -> PharResult<ArchiveRef> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        let fname = ar.borrow().fname.clone();
        if name == b".phar/stub.php" {
            return Err(PharError::bad_method(format!(
                "Cannot set stub \".phar/stub.php\" directly in phar \"{}\", use setStub",
                s(&fname)
            )));
        }
        if name == b".phar/alias.txt" {
            return Err(PharError::bad_method(format!(
                "Cannot set alias \".phar/alias.txt\" directly in phar \"{}\", use setAlias",
                s(&fname)
            )));
        }
        if is_magic_phar(name) {
            return Err(PharError::bad_method(
                "Cannot set any files or directories in magic \".phar\" directory",
            ));
        }
        Ok(ar)
    }

    /// `Phar::offsetSet($name, string $value)`.
    pub fn offset_set(&mut self, name: &[u8], content: &[u8]) -> PharResult<()> {
        self.set_checks(name)?;
        self.add_file_impl(name, Some(content), None)
    }

    /// `Phar::offsetSet($name, resource $value)`: `content` is the stream's
    /// remaining data and `mode` its `fstat()` mode, if any.
    pub fn offset_set_stream(
        &mut self,
        name: &[u8],
        content: &[u8],
        mode: Option<u32>,
    ) -> PharResult<()> {
        self.set_checks(name)?;
        self.add_file_impl(name, None, Some((content, mode)))
    }

    /// `phar_add_file()`.
    fn add_file_impl(
        &mut self,
        name: &[u8],
        content: Option<&[u8]>,
        stream: Option<(&[u8], Option<u32>)>,
    ) -> PharResult<()> {
        if is_magic_phar(name) {
            return Err(PharError::bad_method(
                "Cannot create any files in magic \".phar\" directory",
            ));
        }
        let ar = self.archive()?;
        let fname = ar.borrow().fname.clone();
        let mut data =
            reg::get_or_create_entry_data(&fname, name, "w+b", 0, true, now()).map_err(|e| {
                PharError::bad_method(match e {
                    Some(e) => format!(
                        "Entry {} does not exist and cannot be created: {e}",
                        s(name)
                    ),
                    None => format!("Entry {} does not exist and cannot be created", s(name)),
                })
            })?;
        let is_dir = data.with_entry(|e| e.is_dir).unwrap_or(false);
        if !is_dir {
            let bytes = content
                .or(stream.map(|(c, _)| c))
                .unwrap_or_default()
                .to_vec();
            data.with_entry_mut(|e| e.set_content(bytes));
        }
        let stat_mode = stream.and_then(|(_, m)| m);
        data.with_entry_mut(|e| match stat_mode {
            Some(m) => e.flags = m & ENT_PERM_MASK,
            None => e.flags &= !php_streams::sys::current_umask(),
        });
        let phar = Rc::clone(&data.archive);
        if !Rc::ptr_eq(&phar, &ar) {
            self.archive = Some(Rc::clone(&phar));
        }
        reg::entry_delref(data);
        flush_err(&phar)
    }

    /// `Phar::offsetUnset()`.
    pub fn offset_unset(&mut self, name: &[u8]) -> PharResult<()> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        match ar.borrow().manifest.get(name) {
            None => return Ok(()),
            Some(e) if e.is_deleted => return Ok(()),
            Some(_) => {}
        }
        let ar = self.cow()?;
        if let Some(e) = ar.borrow_mut().manifest.get_mut(name) {
            e.is_modified = false;
            e.is_deleted = true;
        }
        flush_err(&ar)
    }

    /// `Phar::addEmptyDir()`.
    pub fn add_empty_dir(&mut self, dir: &[u8]) -> PharResult<()> {
        let ar = self.archive()?;
        if is_magic_phar(dir) {
            return Err(PharError::bad_method(
                "Cannot create a directory in magic \".phar\" directory",
            ));
        }
        let fname = ar.borrow().fname.clone();
        let data =
            reg::get_or_create_entry_data(&fname, dir, "w+b", 2, true, now()).map_err(|e| {
                PharError::bad_method(match e {
                    Some(e) => format!(
                        "Directory {} does not exist and cannot be created: {e}",
                        s(dir)
                    ),
                    None => format!("Directory {} does not exist and cannot be created", s(dir)),
                })
            })?;
        let phar = Rc::clone(&data.archive);
        self.archive = Some(Rc::clone(&phar));
        reg::entry_delref(data);
        flush_err(&phar)
    }

    /// `Phar::addFile()`: `opened` is the content and `fstat()` mode of the
    /// opened file, or `None` when it could not be opened.
    pub fn add_file(
        &mut self,
        file: &[u8],
        local_name: Option<&[u8]>,
        opened: Option<(&[u8], Option<u32>)>,
    ) -> PharResult<()> {
        self.archive()?;
        if find_scheme(file).is_none() && !php_streams::paths::check_open_basedir_ex(file, false) {
            return Err(PharError::runtime(format!(
                "phar error: unable to open file \"{}\" to add to phar archive, open_basedir restrictions prevent this",
                s(file)
            )));
        }
        let Some((content, mode)) = opened else {
            return Err(PharError::runtime(format!(
                "phar error: unable to open file \"{}\" to add to phar archive",
                s(file)
            )));
        };
        let name = local_name.unwrap_or(file);
        self.add_file_impl(name, None, Some((content, mode)))
    }

    /// `Phar::addFromString()`.
    pub fn add_from_string(&mut self, local_name: &[u8], content: &[u8]) -> PharResult<()> {
        self.archive()?;
        self.add_file_impl(local_name, Some(content), None)
    }

    /// `Phar::copy()`.
    pub fn copy(&mut self, old: &[u8], new: &[u8]) -> PharResult<bool> {
        let ar = self.archive()?;
        let fname = ar.borrow().fname.clone();
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(format!(
                "Cannot copy \"{}\" to \"{}\", phar is read-only",
                s(old),
                s(new)
            )));
        }
        if is_magic_phar(old) {
            return Err(PharError::unexpected(format!(
                "file \"{}\" cannot be copied to file \"{}\", cannot copy Phar meta-file in {}",
                s(old),
                s(new),
                s(&fname)
            )));
        }
        if is_magic_phar(new) {
            return Err(PharError::unexpected(format!(
                "file \"{}\" cannot be copied to file \"{}\", cannot copy to Phar meta-file in {}",
                s(old),
                s(new),
                s(&fname)
            )));
        }
        let exists = ar.borrow().manifest.get(old).is_some_and(|e| !e.is_deleted);
        if !exists {
            return Err(PharError::unexpected(format!(
                "file \"{}\" cannot be copied to file \"{}\", file does not exist in {}",
                s(old),
                s(new),
                s(&fname)
            )));
        }
        if ar.borrow().manifest.get(new).is_some_and(|e| !e.is_deleted) {
            return Err(PharError::unexpected(format!(
                "file \"{}\" cannot be copied to file \"{}\", file must not already exist in phar {}",
                s(old),
                s(new),
                s(&fname)
            )));
        }
        if let crate::util::PathCheck::Err(e) = path_check(new).0 {
            return Err(PharError::unexpected(format!(
                "file \"{}\" contains invalid characters {e}, cannot be copied from \"{}\" in phar {}",
                s(new),
                s(old),
                s(&fname)
            )));
        }
        let ar = self.cow()?;
        let mut newentry = ar.borrow().manifest.get(old).cloned().unwrap_or_default();
        newentry.filename = new.to_vec();
        newentry.fp_refcount = 0;
        newentry.fileinfo_lock_count = 0;
        if newentry.fp_type != FpType::Fp {
            let content = ar.borrow_mut().read_entry(old).map_err(PharError::phar)?;
            newentry.symlink = None;
            if newentry.is_tar {
                newentry.tar_type = TAR_FILE;
            }
            newentry.data = Some(Rc::new(std::cell::RefCell::new(content)));
            newentry.fp_type = FpType::Mod;
            newentry.offset = 0;
            newentry.is_modified = true;
        }
        {
            let mut a = ar.borrow_mut();
            a.manifest.add(new.to_vec(), newentry);
            a.is_modified = true;
        }
        flush_err(&ar)?;
        Ok(true)
    }

    // ------------------------------------------------------- compression

    fn can_compress_all(&self) -> PharResult<bool> {
        let ar = self.archive()?;
        let (z, b) = (reg::has_zlib(), reg::has_bz2());
        let a = ar.borrow();
        let bad = a.manifest.iter().any(|(_, e)| {
            !e.is_deleted
                && ((!b && e.flags & ENT_COMPRESSED_BZ2 != 0)
                    || (!z && e.flags & ENT_COMPRESSED_GZ != 0))
        });
        Ok(!bad)
    }

    /// `pharobj_set_compression()`.
    fn set_compression(ar: &ArchiveRef, compress: u32) -> Result<(), String> {
        let keys = ar.borrow().manifest.keys();
        for k in &keys {
            let (skip, current) = {
                let a = ar.borrow();
                let Some(e) = a.manifest.get(k) else { continue };
                let cur = e.flags & ENT_COMPRESSION_MASK;
                (e.is_deleted || e.is_dir || cur == 0 || cur == compress, cur)
            };
            let _ = current;
            if !skip {
                ar.borrow_mut().open_entry_fp(k, false)?;
            }
        }
        let mut a = ar.borrow_mut();
        for (_, e) in a.manifest.iter_mut() {
            if e.is_deleted || e.flags & ENT_COMPRESSION_MASK == compress {
                continue;
            }
            e.old_flags = e.flags;
            e.flags &= !ENT_COMPRESSION_MASK;
            e.flags |= compress;
            e.is_modified = true;
        }
        Ok(())
    }

    /// `Phar::compressFiles()`.
    pub fn compress_files(&mut self, method: i64) -> PharResult<()> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Phar is readonly, cannot change compression",
            ));
        }
        let flags = match method {
            m if m == i64::from(ENT_COMPRESSED_GZ) => {
                if !reg::has_zlib() {
                    return Err(PharError::bad_method(
                        "Cannot compress files within archive with gzip, enable ext/zlib in php.ini",
                    ));
                }
                ENT_COMPRESSED_GZ
            }
            m if m == i64::from(ENT_COMPRESSED_BZ2) => {
                if !reg::has_bz2() {
                    return Err(PharError::bad_method(
                        "Cannot compress files within archive with bz2, enable ext/bz2 in php.ini",
                    ));
                }
                ENT_COMPRESSED_BZ2
            }
            _ => {
                return Err(PharError::bad_method(
                    "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
                ))
            }
        };
        if ar.borrow().is_tar {
            return Err(PharError::bad_method("Cannot compress with Gzip compression, tar archives cannot compress individual files, use compress() to compress the whole archive"));
        }
        if !self.can_compress_all()? {
            return Err(PharError::bad_method(if flags == ENT_COMPRESSED_GZ {
                "Cannot compress all files as Gzip, some are compressed as bzip2 and cannot be decompressed"
            } else {
                "Cannot compress all files as Bzip2, some are compressed as gzip and cannot be decompressed"
            }));
        }
        let ar = self.cow()?;
        Self::set_compression(&ar, flags).map_err(PharError::bad_method)?;
        ar.borrow_mut().is_modified = true;
        reg::flush(&ar).map_err(PharError::bad_method)
    }

    /// `Phar::decompressFiles()`.
    pub fn decompress_files(&mut self) -> PharResult<bool> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Phar is readonly, cannot change compression",
            ));
        }
        if !self.can_compress_all()? {
            return Err(PharError::bad_method("Cannot decompress all files, some are compressed as bzip2 or gzip and cannot be decompressed"));
        }
        if ar.borrow().is_tar {
            return Ok(true);
        }
        let ar = self.cow()?;
        Self::set_compression(&ar, 0).map_err(PharError::bad_method)?;
        ar.borrow_mut().is_modified = true;
        reg::flush(&ar).map_err(PharError::bad_method)?;
        Ok(true)
    }

    fn whole_method(
        method: Option<i64>,
        current: u32,
        format: i64,
        prefix: &str,
    ) -> PharResult<u32> {
        let _ = prefix;
        let Some(method) = method else {
            return Ok(current & FILE_COMPRESSION_MASK);
        };
        match method {
            9_021_976 => Ok(current & FILE_COMPRESSION_MASK),
            0 => Ok(FILE_COMPRESSED_NONE),
            m if m == i64::from(ENT_COMPRESSED_GZ) => {
                if format == FORMAT_ZIP {
                    return Err(PharError::bad_method("Cannot compress entire archive with gzip, zip archives do not support whole-archive compression"));
                }
                if !reg::has_zlib() {
                    return Err(PharError::bad_method(
                        "Cannot compress entire archive with gzip, enable ext/zlib in php.ini",
                    ));
                }
                Ok(FILE_COMPRESSED_GZ)
            }
            m if m == i64::from(ENT_COMPRESSED_BZ2) => {
                if format == FORMAT_ZIP {
                    return Err(PharError::bad_method("Cannot compress entire archive with bz2, zip archives do not support whole-archive compression"));
                }
                if !reg::has_bz2() {
                    return Err(PharError::bad_method(
                        "Cannot compress entire archive with bz2, enable ext/bz2 in php.ini",
                    ));
                }
                Ok(FILE_COMPRESSED_BZ2)
            }
            _ => Err(PharError::bad_method(
                "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
            )),
        }
    }

    /// `Phar::convertToExecutable()`.
    pub fn convert_to_executable(
        &mut self,
        format: Option<i64>,
        method: Option<i64>,
        ext: Option<&[u8]>,
    ) -> PharResult<Converted> {
        let ar = self.archive()?;
        if reg::readonly() {
            return Err(PharError::unexpected(
                "Cannot write out executable phar archive, phar is read-only",
            ));
        }
        let format = match format.unwrap_or(FORMAT_SAME) {
            9_021_976 | FORMAT_SAME => {
                let a = ar.borrow();
                if a.is_tar {
                    FORMAT_TAR
                } else if a.is_zip {
                    FORMAT_ZIP
                } else {
                    FORMAT_PHAR
                }
            }
            f @ (FORMAT_PHAR | FORMAT_TAR | FORMAT_ZIP) => f,
            _ => {
                return Err(PharError::bad_method(
                    "Unknown file format specified, please pass one of Phar::PHAR, Phar::TAR or Phar::ZIP",
                ))
            }
        };
        let flags = Self::whole_method(method, ar.borrow().flags, format, "")?;
        convert_to_other(&ar, format, ext, flags, false)
    }

    /// `Phar::convertToData()`.
    pub fn convert_to_data(
        &mut self,
        format: Option<i64>,
        method: Option<i64>,
        ext: Option<&[u8]>,
    ) -> PharResult<Converted> {
        let ar = self.archive()?;
        let format = match format.unwrap_or(FORMAT_SAME) {
            9_021_976 | FORMAT_SAME => {
                let a = ar.borrow();
                if a.is_tar {
                    FORMAT_TAR
                } else if a.is_zip {
                    FORMAT_ZIP
                } else {
                    return Err(PharError::unexpected(
                        "Cannot write out data phar archive, use Phar::TAR or Phar::ZIP",
                    ));
                }
            }
            FORMAT_PHAR => {
                return Err(PharError::unexpected(
                    "Cannot write out data phar archive, use Phar::TAR or Phar::ZIP",
                ))
            }
            f @ (FORMAT_TAR | FORMAT_ZIP) => f,
            _ => {
                return Err(PharError::bad_method(
                    "Unknown file format specified, please pass one of Phar::TAR or Phar::ZIP",
                ))
            }
        };
        let flags = Self::whole_method(method, ar.borrow().flags, format, "")?;
        convert_to_other(&ar, format, ext, flags, true)
    }

    /// `Phar::compress()`.
    pub fn compress(&mut self, method: i64, ext: Option<&[u8]>) -> PharResult<Converted> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot compress phar archive, phar is read-only",
            ));
        }
        if ar.borrow().is_zip {
            return Err(PharError::unexpected(
                "Cannot compress zip-based archives with whole-archive compression",
            ));
        }
        let flags = match method {
            0 => FILE_COMPRESSED_NONE,
            m if m == i64::from(ENT_COMPRESSED_GZ) => {
                if !reg::has_zlib() {
                    return Err(PharError::bad_method(
                        "Cannot compress entire archive with gzip, enable ext/zlib in php.ini",
                    ));
                }
                FILE_COMPRESSED_GZ
            }
            m if m == i64::from(ENT_COMPRESSED_BZ2) => {
                if !reg::has_bz2() {
                    return Err(PharError::bad_method(
                        "Cannot compress entire archive with bz2, enable ext/bz2 in php.ini",
                    ));
                }
                FILE_COMPRESSED_BZ2
            }
            _ => {
                return Err(PharError::bad_method(
                    "Unknown compression specified, please pass one of Phar::GZ or Phar::BZ2",
                ))
            }
        };
        let (is_tar, is_data) = {
            let a = ar.borrow();
            (a.is_tar, a.is_data)
        };
        convert_to_other(
            &ar,
            if is_tar { FORMAT_TAR } else { FORMAT_PHAR },
            ext,
            flags,
            is_data,
        )
    }

    /// `Phar::decompress()`.
    pub fn decompress(&mut self, ext: Option<&[u8]>) -> PharResult<Converted> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot decompress phar archive, phar is read-only",
            ));
        }
        if ar.borrow().is_zip {
            return Err(PharError::unexpected(
                "Cannot decompress zip-based archives with whole-archive compression",
            ));
        }
        let (is_tar, is_data) = {
            let a = ar.borrow();
            (a.is_tar, a.is_data)
        };
        convert_to_other(
            &ar,
            if is_tar { FORMAT_TAR } else { FORMAT_PHAR },
            ext,
            FILE_COMPRESSED_NONE,
            is_data,
        )
    }

    // ------------------------------------------------------- extraction

    /// `Phar::extractTo()`.
    pub fn extract_to(
        &self,
        path_to: &[u8],
        files: &ExtractFiles,
        overwrite: bool,
    ) -> PharResult<bool> {
        let ar = self.archive()?;
        let fname = ar.borrow().fname.clone();
        if !crate::sys::path(&fname).is_file() {
            return Err(PharError::invalid_argument(format!(
                "Invalid argument, {} cannot be found",
                s(&fname)
            )));
        }
        if path_to.is_empty() {
            return Err(PharError::invalid_argument(
                "Invalid argument, extraction path must be non-zero length",
            ));
        }
        if path_to.len() >= 4096 {
            let shown = &path_to[..50];
            return Err(PharError::invalid_argument(format!(
                "Cannot extract to \"{}...\", destination directory is too long for filesystem",
                s(shown)
            )));
        }
        match php_streams::stat::stat_path(path_to) {
            Err(_) => {
                if !mkdir_recursive(path_to, 0o777) {
                    return Err(PharError::runtime(format!(
                        "Unable to create path \"{}\" for extraction",
                        s(path_to)
                    )));
                }
            }
            Ok(sb) if !sb.is_dir() => {
                return Err(PharError::runtime(format!(
                    "Unable to use path \"{}\" for extraction, it is a file, must be a directory",
                    s(path_to)
                )))
            }
            Ok(_) => {}
        }
        let fail = |e: String| {
            PharError::phar(format!(
                "Extraction from phar \"{}\" failed: {e}",
                s(&fname)
            ))
        };
        let missing = |f: &[u8]| {
            PharError::phar(format!(
                "phar error: attempted to extract non-existent file or directory \"{}\" from phar \"{}\"",
                s(f),
                s(&fname)
            ))
        };
        match files {
            ExtractFiles::List(list) => {
                if list.is_empty() {
                    return Ok(false);
                }
                for f in list {
                    let Some(f) = f else {
                        return Err(PharError::invalid_argument(
                            "Invalid argument, array of filenames to extract contains non-string value",
                        ));
                    };
                    if extract_helper(&ar, Some(f), path_to, overwrite).map_err(fail)? == 0 {
                        return Err(missing(f));
                    }
                }
                Ok(true)
            }
            ExtractFiles::One(f) => {
                let n = extract_helper(&ar, Some(f), path_to, overwrite).map_err(fail)?;
                if n == 0 {
                    return Err(missing(f));
                }
                Ok(true)
            }
            ExtractFiles::All => {
                extract_helper(&ar, None, path_to, overwrite).map_err(fail)?;
                Ok(true)
            }
        }
    }

    // ------------------------------------------------------- building

    /// Starts `buildFromIterator()` (`base` is the base directory, empty for
    /// none) after the method's checks.
    pub fn build_from_iterator_start(&mut self, base: &[u8]) -> PharResult<BuildState> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot write out phar archive, phar is read-only",
            ));
        }
        let ar = self.cow()?;
        Ok(BuildState {
            archive: ar,
            base: base.to_vec(),
            result: Vec::new(),
        })
    }

    /// `Phar::buildFromDirectory()`: walks `dir` like
    /// `RecursiveIteratorIterator(RecursiveDirectoryIterator(dir))` (in
    /// `readdir()` order); `filter` implements the optional `RegexIterator`
    /// on each path. Returns the `entry => source path` array.
    pub fn build_from_directory(
        &mut self,
        dir: &[u8],
        filter: Option<PathFilter<'_>>,
    ) -> PharResult<BuildResult> {
        let ar = self.archive()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::unexpected(
                "Cannot write to archive - write operations restricted by INI setting",
            ));
        }
        let entries = walk_directory(dir).map_err(PharError::unexpected)?;
        let ar = self.cow()?;
        let mut st = BuildState {
            archive: ar,
            base: dir.to_vec(),
            result: Vec::new(),
        };
        for path in entries {
            if let Some(f) = filter {
                if !f(&path) {
                    continue;
                }
            }
            st.add(
                BuildItem::FileInfo {
                    pathname: path,
                    mtime: MTime::Internal,
                },
                "RecursiveIteratorIterator",
            )?;
        }
        st.finish()
    }
}

fn find_scheme(p: &[u8]) -> Option<usize> {
    crate::util::find(p, b"://")
}

fn mkdir_recursive(path: &[u8], mode: u32) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(crate::sys::path(path))
        .is_ok()
}

/// Recursively lists the files under `dir` (skipping dots, directories
/// descended into in `readdir()` order).
fn walk_directory(dir: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    fn walk(dir: &[u8], out: &mut Vec<Vec<u8>>) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        for ent in std::fs::read_dir(crate::sys::path(dir))? {
            let ent = ent?;
            let mut p = dir.to_vec();
            if p.last() != Some(&b'/') {
                p.push(b'/');
            }
            p.extend_from_slice(ent.file_name().as_bytes());
            let is_link = ent.file_type().is_ok_and(|t| t.is_symlink());
            if crate::sys::path(&p).is_dir() {
                // RecursiveDirectoryIterator does not descend into linked
                // directories; phar_build() then skips them as directories.
                if !is_link {
                    walk(&p, out)?;
                }
            } else {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, &mut out).map_err(|e| {
        format!(
            "RecursiveDirectoryIterator::__construct(): Failed to open directory: {}",
            crate::wrapper::strerror(&e)
        )
    })?;
    Ok(out)
}

/// One element produced by the iterator passed to `buildFromIterator()`.
#[derive(Debug, Clone)]
pub enum BuildItem {
    /// A string value (a file path); `key` is the iterator key (`None`
    /// when it is not a string).
    Path {
        /// The value.
        value: Vec<u8>,
        /// The key, if a string.
        key: Option<Vec<u8>>,
    },
    /// A stream resource: its remaining content and `fstat()` mode.
    Stream {
        /// The key, if a string.
        key: Option<Vec<u8>>,
        /// Content.
        content: Vec<u8>,
        /// Mode.
        mode: Option<u32>,
    },
    /// An `SplFileInfo`: its path name and, for user classes, the result
    /// of `getMTime()`.
    FileInfo {
        /// `getPathname()`.
        pathname: Vec<u8>,
        /// `getMTime()` for user classes.
        mtime: MTime,
    },
    /// Any other value.
    Invalid,
}

/// The `getMTime()` result of a user `SplFileInfo` subclass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MTime {
    /// Not a user class: the entry gets the current time.
    Internal,
    /// The returned integer.
    Int(i64),
    /// It did not return an int.
    NotInt,
}

/// A `RegexIterator`-like path filter for `buildFromDirectory()`.
pub type PathFilter<'a> = &'a dyn Fn(&[u8]) -> bool;

/// The `entry => source path` array returned by the build methods.
pub type BuildResult = Vec<(Vec<u8>, Vec<u8>)>;

/// Source data of one built entry: (key, content, mode, opened path).
type Resolved = (Vec<u8>, Vec<u8>, Option<u32>, Vec<u8>);

/// State of a running `buildFromIterator()` / `buildFromDirectory()`.
#[derive(Debug)]
pub struct BuildState {
    archive: ArchiveRef,
    base: Vec<u8>,
    result: Vec<(Vec<u8>, Vec<u8>)>,
}

impl BuildState {
    /// `phar_build()` for one element; `class` is the iterator class name
    /// used in messages.
    #[allow(clippy::too_many_lines)]
    pub fn add(&mut self, item: BuildItem, class: &str) -> PharResult<()> {
        let fname = self.archive.borrow().fname.clone();
        let (str_key, content, mode, opened): (Vec<u8>, Vec<u8>, Option<u32>, Vec<u8>);
        let mtime: MTime;
        let invalid_key = || {
            PharError::unexpected(format!(
                "Iterator {class} returned an invalid key (must return a string)"
            ))
        };
        match item {
            BuildItem::Invalid => {
                return Err(PharError::unexpected(format!("Iterator {class} returned an invalid value (must return a string, a stream, or an SplFileInfo object)")))
            }
            BuildItem::Stream { key, content: c, mode: m } => {
                let Some(k) = key else { return Err(invalid_key()) };
                str_key = k;
                content = c;
                mode = m;
                opened = b"[stream]".to_vec();
                mtime = MTime::Internal;
            }
            BuildItem::FileInfo { pathname, mtime: mt } => {
                if self.base.is_empty() {
                    return Err(PharError::bad_method(format!("Iterator {class} returns an SplFileInfo object, so base directory must be specified")));
                }
                if crate::sys::path(&pathname).is_dir() {
                    return Ok(());
                }
                let Some(path) = php_streams::paths::expand_filepath(&pathname) else {
                    return Err(PharError::unexpected("Could not resolve file path"));
                };
                mtime = mt;
                let (k, c, m, o) = self.resolve_path(&path, None, class)?;
                if k.is_empty() {
                    return Ok(());
                }
                str_key = k;
                content = c;
                mode = m;
                opened = o;
            }
            BuildItem::Path { value, key } => {
                mtime = MTime::Internal;
                let (k, c, m, o) = self.resolve_path(&value, key, class)?;
                if k.is_empty() && !self.base.is_empty() {
                    return Ok(());
                }
                str_key = k;
                content = c;
                mode = m;
                opened = o;
            }
        }
        if is_magic_phar(&str_key) {
            return Ok(());
        }
        let timestamp = match mtime {
            MTime::Int(t) => {
                if !(0..=i64::from(u32::MAX)).contains(&t) {
                    return Err(PharError::bad_method(format!(
                        "Entry {} cannot be created: timestamp is limited to 32-bit",
                        s(&str_key)
                    )));
                }
                crate::util::lo32(t)
            }
            MTime::NotInt => {
                return Err(PharError::bad_method(format!(
                    "Entry {} cannot be created: getMTime() must return an int",
                    s(&str_key)
                )))
            }
            MTime::Internal => now(),
        };
        let mut data = reg::get_or_create_entry_data(&fname, &str_key, "w+b", 0, true, timestamp)
            .map_err(|e| {
            PharError::bad_method(format!(
                "Entry {} cannot be created: {}",
                s(&str_key),
                e.unwrap_or_default()
            ))
        })?;
        let len = crate::util::to_u32(content.len());
        data.with_entry_mut(|e| {
            e.data = Some(Rc::new(std::cell::RefCell::new(content)));
            e.fp_type = FpType::Ufp;
            e.offset = 0;
            e.offset_abs = 0;
            match mode {
                Some(m) => e.flags = m & ENT_PERM_MASK,
                None => e.flags &= !php_streams::sys::current_umask(),
            }
            e.uncompressed_filesize = len;
            e.compressed_filesize = len;
        });
        self.result.push((str_key, opened));
        reg::entry_delref(data);
        Ok(())
    }

    /// Resolves a source path against the base directory and reads it.
    /// Returns (key, content, mode, opened path); an empty key means skip.
    fn resolve_path(
        &self,
        fname: &[u8],
        key: Option<Vec<u8>>,
        class: &str,
    ) -> PharResult<Resolved> {
        let str_key: Vec<u8>;
        if self.base.is_empty() {
            match key {
                Some(k) => str_key = k,
                None => {
                    return Err(PharError::unexpected(format!(
                        "Iterator {class} returned an invalid key (must return a string)"
                    )))
                }
            }
        } else {
            let Some(base) = php_streams::paths::expand_filepath(&self.base) else {
                return Err(PharError::unexpected("Could not resolve file path"));
            };
            let blen = base.len();
            let slash_adj = usize::from(base.last() == Some(&b'/'));
            let ok = fname.len() >= blen
                && fname.starts_with(&base)
                && matches!(
                    fname.get(blen - slash_adj).copied().unwrap_or(0),
                    0 | b'/' | b'\\'
                );
            if !ok {
                return Err(PharError::unexpected(format!(
                    "Iterator {class} returned a path \"{}\" that is not in the base directory \"{}\"",
                    s(fname),
                    s(&base)
                )));
            }
            let mut k = &fname[blen..];
            if k.is_empty() {
                return Ok((Vec::new(), Vec::new(), None, Vec::new()));
            }
            if k[0] == b'/' || k[0] == b'\\' {
                k = &k[1..];
            }
            str_key = k.to_vec();
        }
        if !php_streams::paths::check_open_basedir(fname) {
            return Err(PharError::unexpected(format!(
                "Iterator {class} returned a path \"{}\" that open_basedir prevents opening",
                s(fname)
            )));
        }
        let Ok(content) = crate::sys::read_file(fname) else {
            return Err(PharError::unexpected(format!(
                "Iterator {class} returned a file that could not be opened \"{}\"",
                s(fname)
            )));
        };
        let mode = php_streams::stat::stat_path(fname)
            .ok()
            .map(|sb| crate::util::lo32(sb.mode));
        let opened = php_streams::paths::expand_filepath(fname).unwrap_or_else(|| fname.to_vec());
        Ok((str_key, content, mode, opened))
    }

    /// Finishes the build: flushes and returns `entry => source` pairs.
    pub fn finish(self) -> PharResult<BuildResult> {
        flush_err(&self.archive)?;
        Ok(self.result)
    }
}

/// `phar_extract_file()`.
#[allow(clippy::too_many_lines)]
fn extract_file(ar: &ArchiveRef, key: &[u8], dest: &[u8], overwrite: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let e = ar.borrow().manifest.get(key).cloned().unwrap_or_default();
    if e.is_mounted || is_magic_phar(&e.filename) {
        return Ok(());
    }
    let name = s(&e.filename);
    let norm = php_streams::paths::normalize(b"/", &e.filename);
    if norm.len() <= 1 {
        return Err(format!("Cannot extract \"{name}\", internal error"));
    }
    let filename = &norm[1..];
    let mut fullpath = dest.to_vec();
    fullpath.push(b'/');
    fullpath.extend_from_slice(filename);
    if fullpath.len() >= 4096 {
        let shown = s(&fullpath[..50]);
        return Err(if e.filename.len() > 50 {
            format!(
                "Cannot extract \"{}...\" to \"{shown}...\", extracted filename is too long for filesystem",
                s(&e.filename[..50])
            )
        } else {
            format!("Cannot extract \"{name}\" to \"{shown}...\", extracted filename is too long for filesystem")
        });
    }
    let full = s(&fullpath);
    if !php_streams::paths::check_open_basedir_ex(&fullpath, false) {
        return Err(format!(
            "Cannot extract \"{name}\" to \"{full}\", open_basedir restrictions in effect"
        ));
    }
    if !overwrite && php_streams::stat::stat_path(&fullpath).is_ok() {
        return Err(format!(
            "Cannot extract \"{name}\" to \"{full}\", path already exists"
        ));
    }
    let dirpart = match filename.iter().rposition(|&c| c == b'/') {
        Some(p) => fullpath[..=(dest.len() + p)].to_vec(),
        None => fullpath[..dest.len()].to_vec(),
    };
    if php_streams::stat::stat_path(&dirpart).is_err() {
        let mode = if e.is_dir {
            e.flags & ENT_PERM_MASK
        } else {
            0o777
        };
        if !mkdir_recursive(&dirpart, mode) {
            return Err(format!(
                "Cannot extract \"{name}\", could not create directory \"{}\"",
                s(&dirpart)
            ));
        }
    }
    if e.is_dir {
        return Ok(());
    }
    let content = ar.borrow_mut().read_entry(key).map_err(|err| {
        format!(
            "Cannot extract \"{name}\" to \"{full}\", unable to open internal file pointer: {err}"
        )
    })?;
    let mut opened = None;
    let Some(mut out) = php_streams::open::open_wrapper(
        &fullpath,
        "w+b",
        php_streams::wrapper::REPORT_ERRORS,
        &mut opened,
        None,
    ) else {
        return Err(format!(
            "Cannot extract \"{name}\", could not open for writing \"{full}\""
        ));
    };
    let _ = out.write(&content[..(e.uncompressed_filesize as usize).min(content.len())]);
    out.close();
    if std::fs::set_permissions(
        crate::sys::path(&fullpath),
        std::fs::Permissions::from_mode(e.flags & ENT_PERM_MASK),
    )
    .is_err()
    {
        return Err(format!(
            "Cannot extract \"{name}\" to \"{full}\", setting file permissions failed"
        ));
    }
    Ok(())
}

/// `extract_helper()`: returns the number of extracted entries.
fn extract_helper(
    ar: &ArchiveRef,
    search: Option<&[u8]>,
    dest: &[u8],
    overwrite: bool,
) -> Result<usize, String> {
    let keys = ar.borrow().manifest.keys();
    let mut n = 0;
    match search {
        None => {
            for k in keys {
                extract_file(ar, &k, dest, overwrite)?;
                n += 1;
            }
        }
        Some(sr) if sr.last() == Some(&b'/') => {
            for k in keys {
                let starts = ar
                    .borrow()
                    .manifest
                    .get(&k)
                    .is_some_and(|e| e.filename.starts_with(sr));
                if !starts {
                    continue;
                }
                extract_file(ar, &k, dest, overwrite)?;
                n += 1;
            }
        }
        Some(sr) => {
            if !ar.borrow().manifest.contains_key(sr) {
                return Ok(0);
            }
            extract_file(ar, sr, dest, overwrite)?;
            n = 1;
        }
    }
    Ok(n)
}

/// `phar_convert_to_other()` + `phar_rename_archive()`.
fn convert_to_other(
    source: &ArchiveRef,
    convert: i64,
    ext: Option<&[u8]>,
    flags: u32,
    is_data: bool,
) -> PharResult<Converted> {
    let mut phar = crate::archive::Archive::default();
    {
        let src = source.borrow();
        phar.flags = flags;
        phar.is_data = is_data;
        match convert {
            FORMAT_TAR => phar.is_tar = true,
            FORMAT_ZIP => phar.is_zip = true,
            _ => phar.is_data = false,
        }
        phar.fname.clone_from(&src.fname);
        phar.version = API_VERSION_STR.to_string();
        phar.is_temporary_alias = src.is_temporary_alias;
        phar.alias.clone_from(&src.alias);
        phar.metadata = src.metadata.clone();
        phar.is_writeable = true;
    }
    let keys = source.borrow().manifest.keys();
    for k in keys {
        let mut e: Entry = source
            .borrow()
            .manifest
            .get(&k)
            .cloned()
            .unwrap_or_default();
        if e.symlink.is_none() && e.tmp.is_none() {
            let content = source.borrow_mut().read_entry(&k).map_err(|err| {
                PharError::unexpected(format!(
                    "Cannot convert phar archive \"{}\", unable to open entry \"{}\" contents: {err}",
                    s(&source.borrow().fname),
                    s(&e.filename)
                ))
            })?;
            let size = e.uncompressed_filesize;
            e.data = Some(Rc::new(std::cell::RefCell::new(content)));
            e.fp_type = FpType::Mod;
            e.offset = 0;
            e.uncompressed_filesize = size;
        }
        e.is_zip = phar.is_zip;
        e.is_tar = phar.is_tar;
        if e.is_tar {
            e.tar_type = if e.is_dir { TAR_DIR } else { TAR_FILE };
        }
        e.header_offset = 0;
        e.is_modified = true;
        e.fp_refcount = 0;
        e.fileinfo_lock_count = 0;
        e.old_flags = e.flags & !ENT_COMPRESSION_MASK;
        e.inode = crate::util::entry_inode(&phar.fname, &e.filename);
        let name = e.filename.clone();
        if phar.manifest.add(name.clone(), e) {
            phar.add_virtual_dirs(&name);
        }
    }
    rename_archive(phar, ext)
}

const PHAR_EXT_LIST: [&[u8]; 12] = [
    b".phar.tar.bz2",
    b".phar.tar.gz",
    b".phar.php",
    b".phar.bz2",
    b".phar.zip",
    b".phar.tar",
    b".phar.gz",
    b".tar.bz2",
    b".tar.gz",
    b".phar",
    b".tar",
    b".zip",
];

/// `phar_rename_archive()`.
#[allow(clippy::too_many_lines)]
fn rename_archive(mut phar: crate::archive::Archive, ext: Option<&[u8]>) -> PharResult<Converted> {
    let ext: Vec<u8> = match ext {
        None => {
            let e: &[u8] = if phar.is_zip {
                if phar.is_data {
                    b"zip"
                } else {
                    b"phar.zip"
                }
            } else if phar.is_tar {
                match phar.flags {
                    FILE_COMPRESSED_GZ => {
                        if phar.is_data {
                            b"tar.gz"
                        } else {
                            b"phar.tar.gz"
                        }
                    }
                    FILE_COMPRESSED_BZ2 => {
                        if phar.is_data {
                            b"tar.bz2"
                        } else {
                            b"phar.tar.bz2"
                        }
                    }
                    _ => {
                        if phar.is_data {
                            b"tar"
                        } else {
                            b"phar.tar"
                        }
                    }
                }
            } else {
                match phar.flags {
                    FILE_COMPRESSED_GZ => b"phar.gz",
                    FILE_COMPRESSED_BZ2 => b"phar.bz2",
                    _ => b"phar",
                }
            };
            e.to_vec()
        }
        Some(e) => {
            let (check, checked) = path_check(e);
            if check.is_err() {
                return Err(PharError::bad_method(format!(
                    "{}phar converted from \"{}\" has invalid extension {}",
                    if phar.is_data { "data " } else { "" },
                    s(&phar.fname),
                    s(crate::util::cstr(e))
                )));
            }
            checked.to_vec()
        }
    };
    let old_path = phar.fname.clone();
    let (dir, old_name) = match old_path.iter().rposition(|&c| c == b'/') {
        Some(p) => (&old_path[..=p], &old_path[p + 1..]),
        None => (&old_path[..0], &old_path[..]),
    };
    let old_name = crate::util::cstr(old_name);
    let mut basename = old_name.to_vec();
    let mut stripped = false;
    for e in PHAR_EXT_LIST {
        if old_name.len() > e.len() && old_name.ends_with(e) {
            basename.truncate(old_name.len() - e.len());
            stripped = true;
            break;
        }
    }
    if !stripped {
        if let Some(p) = basename.iter().rposition(|&c| c == b'.') {
            basename.truncate(p);
        }
    }
    let ext = ext.strip_prefix(b".").unwrap_or(&ext).to_vec();
    let mut newpath = dir.to_vec();
    newpath.extend_from_slice(crate::util::cstr(&basename));
    newpath.push(b'.');
    newpath.extend_from_slice(&ext);
    phar.fname.clone_from(&newpath);
    let newpath_s = s(&newpath);
    if reg::is_cached(&newpath) {
        return Err(PharError::bad_method(format!(
            "Unable to add newly converted phar \"{newpath_s}\" to the list of phars, new phar name is in phar.cache_list"
        )));
    }
    let mut reused: Option<ArchiveRef> = None;
    if let Some(pphar) = reg::find_by_fname(&newpath) {
        if phar.manifest.is_empty() {
            {
                let mut p = pphar.borrow_mut();
                p.is_tar = phar.is_tar;
                p.is_zip = phar.is_zip;
                p.is_data = phar.is_data;
                p.flags = phar.flags;
                p.fp = phar.fp.take();
                p.refcount += 1;
            }
            reused = Some(pphar);
        } else {
            return Err(PharError::bad_method(format!(
                "Unable to add newly converted phar \"{newpath_s}\" to the list of phars, a phar with that name already exists"
            )));
        }
    }
    let target_path = if reused.is_some() {
        old_path.clone()
    } else {
        newpath.clone()
    };
    let unref = || {
        if let Some(r) = &reused {
            r.borrow_mut().refcount -= 1;
        }
    };
    if php_streams::stat::stat_path(&target_path).is_ok() {
        unref();
        return Err(PharError::bad_method(format!(
            "phar \"{}\" exists and must be unlinked prior to conversion",
            s(&target_path)
        )));
    }
    let ar: ArchiveRef = match &reused {
        Some(r) => Rc::clone(r),
        None => phar.into_ref(),
    };
    let is_data = ar.borrow().is_data;
    let exec = i32::from(!is_data);
    let detect = reg::detect_fname_ext(&target_path, exec, 1, true);
    let Ok((_, ext_len)) = detect else {
        unref();
        return Err(PharError::bad_method(format!(
            "{}phar \"{}\" has invalid extension {}",
            if is_data { "data " } else { "" },
            s(&ar.borrow().fname),
            s(&ext)
        )));
    };
    ar.borrow_mut().ext_len = ext_len;
    if reused.is_none() {
        let mut a = ar.borrow_mut();
        if is_data {
            a.alias = None;
        } else if a.alias.is_some() {
            if a.is_temporary_alias {
                a.alias = None;
            } else {
                a.alias = Some(target_path.clone());
                a.is_temporary_alias = true;
                drop(a);
                reg::alias_map_update(&target_path, &ar);
            }
        }
        reg::fname_map_update(&target_path, &ar);
    }
    if let Err(e) = reg::flush_ex(&ar, None, true) {
        reg::fname_map_remove(&target_path);
        return Err(PharError::bad_method(e));
    }
    let (fname, is_data) = {
        let a = ar.borrow();
        (a.fname.clone(), a.is_data)
    };
    Ok(Converted { fname, is_data })
}

// ------------------------------------------------------- static methods

/// `Phar::apiVersion()`.
pub fn api_version() -> &'static str {
    API_VERSION_STR
}

/// `Phar::canCompress()`.
pub fn can_compress(method: i64) -> bool {
    match method {
        m if m == i64::from(ENT_COMPRESSED_GZ) => reg::has_zlib(),
        m if m == i64::from(ENT_COMPRESSED_BZ2) => reg::has_bz2(),
        _ => reg::has_zlib() || reg::has_bz2(),
    }
}

/// `Phar::canWrite()`.
pub fn can_write() -> bool {
    !reg::readonly()
}

/// `Phar::isValidPharFilename()`.
pub fn is_valid_phar_filename(fname: &[u8], executable: bool) -> bool {
    reg::detect_fname_ext(fname, i32::from(executable), 2, true).is_ok()
}

/// `Phar::getSupportedSignatures()`.
pub fn get_supported_signatures() -> Vec<&'static str> {
    crate::signature::supported_signatures()
}

/// `Phar::getSupportedCompression()`.
pub fn get_supported_compression() -> Vec<&'static str> {
    let mut v = Vec::new();
    if reg::has_zlib() {
        v.push("GZ");
    }
    if reg::has_bz2() {
        v.push("BZIP2");
    }
    v
}

/// `Phar::createDefaultStub()`.
pub fn create_default_stub(index: Option<&[u8]>, webindex: Option<&[u8]>) -> PharResult<Vec<u8>> {
    crate::stub::create_default_stub(index, webindex).map_err(PharError::phar)
}

/// `Phar::interceptFileFuncs()`.
pub fn intercept_file_funcs() {
    reg::intercept_functions();
}

/// `Phar::mapPhar()`: `executed` is the executing file,
/// `halt_defined` whether `__COMPILER_HALT_OFFSET__` is defined.
pub fn map_phar(
    executed: Option<&[u8]>,
    alias: Option<&[u8]>,
    halt_defined: bool,
) -> PharResult<bool> {
    match reg::open_executed_filename(executed, alias, halt_defined) {
        Ok(()) => Ok(true),
        Err(Some(e)) => Err(PharError::phar(e)),
        Err(None) => Ok(false),
    }
}

/// `Phar::loadPhar()`.
pub fn load_phar(fname: &[u8], alias: Option<&[u8]>) -> PharResult<bool> {
    match reg::open_from_filename(fname, alias, true) {
        Ok(_) => Ok(true),
        Err(Some(e)) => Err(PharError::phar(e)),
        Err(None) => Ok(false),
    }
}

/// `Phar::running()`.
pub fn running(executed: Option<&[u8]>, retphar: bool) -> Vec<u8> {
    let Some(fname) = executed else {
        return Vec::new();
    };
    if fname.len() >= 7 && fname[..7].eq_ignore_ascii_case(b"phar://") {
        if let Ok((arch, _)) = reg::split_fname(fname, 2, 0) {
            if retphar {
                return fname[..(arch.len() + 7).min(fname.len())].to_vec();
            }
            return arch;
        }
    }
    Vec::new()
}

/// `Phar::mount()`.
pub fn mount(executed: Option<&[u8]>, path: &[u8], actual: &[u8]) -> PharResult<()> {
    let fname = executed.unwrap_or_default();
    let failed = |arch: &[u8]| {
        PharError::phar(format!(
            "Mounting of {} to {} within phar {} failed",
            s(path),
            s(actual),
            s(arch)
        ))
    };
    let do_mount = |ar: &ArchiveRef, inner: &[u8], arch: &[u8]| -> PharResult<()> {
        if reg::mount_entry(ar, actual, inner) {
            Ok(())
        } else {
            Err(PharError::phar(format!(
                "Mounting of {} to {} within phar {} failed",
                s(inner),
                s(actual),
                s(arch)
            )))
        }
    };
    let by_arch = |arch: &[u8], inner: &[u8]| -> PharResult<()> {
        let pphar = match reg::find_by_fname(arch) {
            Some(p) => p,
            None => match reg::cached(arch).and_then(|c| reg::copy_on_write(&c)) {
                Some(p) => p,
                None => {
                    return Err(PharError::phar(format!(
                        "{} is not a phar archive, cannot mount",
                        s(arch)
                    )))
                }
            },
        };
        do_mount(&pphar, inner, arch)
    };
    if fname.len() > 7 && fname.starts_with(b"phar://") {
        if let Ok((arch, _)) = reg::split_fname(fname, 2, 0) {
            if path.len() > 7 && path.starts_with(b"phar://") {
                return Err(PharError::phar(format!(
                    "Can only mount internal paths within a phar archive, use a relative path instead of \"{}\"",
                    s(path)
                )));
            }
            return by_arch(&arch, path);
        }
    }
    if let Some(p) = reg::find_by_fname(fname) {
        return do_mount(&p, path, fname).map_err(|_| failed(fname));
    }
    if let Some(c) = reg::cached(fname) {
        let p = reg::copy_on_write(&c).unwrap_or(c);
        return do_mount(&p, path, fname);
    }
    if let Ok((arch, entry)) = reg::split_fname(path, 2, 0) {
        return by_arch(&arch, &entry);
    }
    Err(PharError::phar(format!(
        "Mounting of {} to {} failed",
        s(path),
        s(actual)
    )))
}

/// `Phar::unlinkArchive()`.
pub fn unlink_archive(fname: &[u8], executed: Option<&[u8]>) -> PharResult<bool> {
    if fname.is_empty() {
        return Err(PharError::phar("Unknown phar archive \"\""));
    }
    let phar = reg::open_from_filename(fname, None, true).map_err(|e| {
        PharError::phar(match e {
            Some(e) => format!("Unknown phar archive \"{}\": {e}", s(fname)),
            None => format!("Unknown phar archive \"{}\"", s(fname)),
        })
    })?;
    if let Some(ex) = executed {
        if ex.len() >= 7 && ex[..7].eq_ignore_ascii_case(b"phar://") {
            if let Ok((arch, _)) = reg::split_fname(ex, 2, 0) {
                if arch == fname {
                    return Err(PharError::phar(format!(
                        "phar archive \"{}\" cannot be unlinked from within itself",
                        s(fname)
                    )));
                }
            }
        }
    }
    if phar.borrow().is_persistent {
        return Err(PharError::phar(format!(
            "phar archive \"{}\" is in phar.cache_list, cannot unlinkArchive()",
            s(fname)
        )));
    }
    if phar.borrow().refcount != 0 {
        return Err(PharError::phar(format!(
            "phar archive \"{}\" has open file handles or objects.  fclose() all file handles, and unset() all objects prior to calling unlinkArchive()",
            s(fname)
        )));
    }
    let path = phar.borrow().fname.clone();
    reg::delref(&phar);
    let _ = std::fs::remove_file(crate::sys::path(&path));
    php_streams::state::clear_stat_cache();
    Ok(true)
}

// ------------------------------------------------------- PharFileInfo

/// A `PharFileInfo` object.
#[derive(Debug, Default)]
pub struct PharFileInfo {
    inner: std::cell::RefCell<Option<(ArchiveRef, EntryRef)>>,
}

impl PharFileInfo {
    /// The function name used for metadata unserialize diagnostics.
    pub const METADATA_FUNCTION: &'static str = "PharFileInfo::getMetadata";

    /// An uninitialised object.
    pub fn new() -> Self {
        PharFileInfo::default()
    }

    /// `PharFileInfo::__construct()`: returns the `SplFileInfo`
    /// constructor argument (the file name).
    pub fn construct(&mut self, fname: &[u8]) -> PharResult<Vec<u8>> {
        if self.inner.borrow().is_some() {
            return Err(PharError::bad_method("Cannot call constructor twice"));
        }
        let invalid = || {
            PharError::runtime(format!(
                "'{}' is not a valid phar archive URL (must have at least phar://filename.phar)",
                s(fname)
            ))
        };
        if fname.len() < 7 || !fname.starts_with(b"phar://") {
            return Err(invalid());
        }
        let (arch, entry) = reg::split_fname(fname, 2, 0).map_err(|_| invalid())?;
        let phar = reg::open_from_filename(&arch, None, true).map_err(|e| {
            PharError::runtime(match e {
                Some(e) => format!("Cannot open phar file '{}': {e}", s(fname)),
                None => format!("Cannot open phar file '{}'", s(fname)),
            })
        })?;
        let er = reg::get_entry_info_dir(&phar, &entry, 1, true).map_err(|e| {
            PharError::runtime(format!(
                "Cannot access phar file entry '{}' in archive '{}'{}",
                s(&entry),
                s(&arch),
                e.map(|e| format!(", {e}")).unwrap_or_default()
            ))
        })?;
        if let EntryRef::Manifest(k) = &er {
            let mut a = phar.borrow_mut();
            let persistent = a.manifest.get(k).is_some_and(|e| e.is_persistent);
            if !persistent {
                if let Some(e) = a.manifest.get_mut(k) {
                    e.fileinfo_lock_count += 1;
                }
                a.refcount += 1;
            }
        }
        *self.inner.get_mut() = Some((phar, er));
        Ok(fname.to_vec())
    }

    /// `PharFileInfo::__destruct()` / object free.
    pub fn release(&mut self) {
        let Some((ar, er)) = self.inner.get_mut().take() else {
            return;
        };
        if let EntryRef::Manifest(k) = er {
            let persistent = {
                let mut a = ar.borrow_mut();
                match a.manifest.get_mut(&k) {
                    Some(e) if !e.is_persistent => {
                        e.fileinfo_lock_count = e.fileinfo_lock_count.saturating_sub(1);
                        false
                    }
                    Some(_) => true,
                    None => false,
                }
            };
            if !persistent {
                reg::delref(&ar);
            }
        }
    }

    fn get(&self) -> PharResult<(ArchiveRef, EntryRef)> {
        self.inner.borrow().clone().ok_or_else(|| {
            PharError::bad_method("Cannot call method on an uninitialized PharFileInfo object")
        })
    }

    /// Makes a cached (`phar.cache_list`) archive writable before a
    /// modification (`phar_copy_on_write()` + re-fetching the entry).
    fn cow(&self) -> PharResult<()> {
        let (ar, _) = self.get()?;
        if !ar.borrow().is_persistent {
            return Ok(());
        }
        let new = reg::copy_on_write(&ar).ok_or_else(|| {
            PharError::phar(format!(
                "phar \"{}\" is persistent, unable to copy on write",
                s(&ar.borrow().fname)
            ))
        })?;
        if let Some((a, er)) = self.inner.borrow_mut().as_mut() {
            // Re-point at the copy and take the reference a non-cached
            // entry holds (lock + archive refcount), so that release()
            // stays balanced and the copy survives a temporary object.
            if let EntryRef::Manifest(k) = er {
                let mut c = new.borrow_mut();
                if let Some(e) = c.manifest.get_mut(k.as_slice()) {
                    e.fileinfo_lock_count += 1;
                }
                c.refcount += 1;
            }
            *a = new;
        }
        Ok(())
    }

    fn entry(&self) -> PharResult<Entry> {
        let (ar, er) = self.get()?;
        let a = ar.borrow();
        Ok(er.with(&a, Clone::clone).unwrap_or_default())
    }

    fn update(&self, f: impl FnOnce(&mut Entry)) -> PharResult<ArchiveRef> {
        let (ar, er) = self.get()?;
        if let EntryRef::Manifest(k) = &er {
            if let Some(e) = ar.borrow_mut().manifest.get_mut(k) {
                f(e);
            }
        }
        Ok(ar)
    }

    /// `getCompressedSize()`.
    pub fn get_compressed_size(&self) -> PharResult<i64> {
        Ok(i64::from(self.entry()?.compressed_filesize))
    }

    /// `isCompressed()`.
    pub fn is_compressed(&self, method: Option<i64>) -> PharResult<bool> {
        let e = self.entry()?;
        match method {
            None | Some(9_021_976) => Ok(e.flags & ENT_COMPRESSION_MASK != 0),
            Some(m) if m == i64::from(ENT_COMPRESSED_GZ) => Ok(e.flags & ENT_COMPRESSED_GZ != 0),
            Some(m) if m == i64::from(ENT_COMPRESSED_BZ2) => Ok(e.flags & ENT_COMPRESSED_BZ2 != 0),
            Some(_) => Err(PharError::bad_method("Unknown compression type specified")),
        }
    }

    /// `getCRC32()`.
    pub fn get_crc32(&self) -> PharResult<i64> {
        let e = self.entry()?;
        if e.is_dir {
            return Err(PharError::bad_method(
                "Phar entry is a directory, does not have a CRC",
            ));
        }
        if e.is_crc_checked {
            Ok(i64::from(e.crc32))
        } else {
            Err(PharError::bad_method("Phar entry was not CRC checked"))
        }
    }

    /// `isCRCChecked()`.
    pub fn is_crc_checked(&self) -> PharResult<bool> {
        Ok(self.entry()?.is_crc_checked)
    }

    /// `getPharFlags()`.
    pub fn get_phar_flags(&self) -> PharResult<i64> {
        Ok(i64::from(
            self.entry()?.flags & !(ENT_PERM_MASK | ENT_COMPRESSION_MASK),
        ))
    }

    /// `chmod()`.
    pub fn chmod(&self, perms: i64) -> PharResult<()> {
        let e = self.entry()?;
        let (ar, _) = self.get()?;
        if e.is_temp_dir {
            return Err(PharError::bad_method(format!(
                "Phar entry \"{}\" is a temporary directory (not an actual entry in the archive), cannot chmod",
                s(&e.filename)
            )));
        }
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::phar(format!(
                "Cannot modify permissions for file \"{}\" in phar \"{}\", write operations are prohibited",
                s(&e.filename),
                s(&ar.borrow().fname)
            )));
        }
        self.cow()?;
        let ar = self.update(|e| {
            e.flags &= !ENT_PERM_MASK;
            e.flags |= crate::util::lo32(perms & 0o777);
            e.old_flags = e.flags;
            e.is_modified = true;
        })?;
        ar.borrow_mut().is_modified = true;
        php_streams::state::clear_stat_cache();
        flush_err(&ar)
    }

    /// `hasMetadata()`.
    pub fn has_metadata(&self) -> PharResult<bool> {
        Ok(self.entry()?.metadata.has_data())
    }

    /// `getMetadata()`.
    pub fn get_metadata(&self, has_options: bool) -> PharResult<MetadataGet> {
        Ok(metadata_get(&self.entry()?.metadata, has_options))
    }

    /// `setMetadata()`.
    pub fn set_metadata(&self, value: Value, serialized: Vec<u8>) -> PharResult<()> {
        let e = self.entry()?;
        let (ar, _) = self.get()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        if e.is_temp_dir {
            return Err(PharError::bad_method("Phar entry is a temporary directory (not an actual entry in the archive), cannot set metadata"));
        }
        self.cow()?;
        let ar = self.update(|e| {
            e.metadata = Metadata {
                serialized: Some(serialized),
                value: Some(value),
            };
            e.is_modified = true;
        })?;
        ar.borrow_mut().is_modified = true;
        flush_err(&ar)
    }

    /// `delMetadata()`.
    pub fn del_metadata(&self) -> PharResult<bool> {
        let e = self.entry()?;
        let (ar, _) = self.get()?;
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Write operations disabled by the php.ini setting phar.readonly",
            ));
        }
        if e.is_temp_dir {
            return Err(PharError::bad_method("Phar entry is a temporary directory (not an actual entry in the archive), cannot delete metadata"));
        }
        if !e.metadata.has_data() {
            return Ok(true);
        }
        self.cow()?;
        let ar = self.update(|e| {
            e.metadata.clear();
            e.is_modified = true;
        })?;
        ar.borrow_mut().is_modified = true;
        flush_err(&ar)?;
        Ok(true)
    }

    /// `getContent()`.
    pub fn get_content(&self) -> PharResult<Vec<u8>> {
        let e = self.entry()?;
        let (ar, er) = self.get()?;
        let fname = ar.borrow().fname.clone();
        if e.is_dir {
            return Err(PharError::bad_method(format!(
                "phar error: Cannot retrieve contents, \"{}\" in phar \"{}\" is a directory",
                s(&e.filename),
                s(&fname)
            )));
        }
        let Some(key) = er.key() else {
            return Ok(Vec::new());
        };
        let link = ar.borrow().link_source(key).unwrap_or_else(|| key.to_vec());
        ar.borrow_mut().open_entry_fp(&link, false).map_err(|err| {
            PharError::bad_method(format!(
                "phar error: Cannot retrieve contents, \"{}\" in phar \"{}\": {err}",
                s(&e.filename),
                s(&fname)
            ))
        })?;
        let mut data = ar.borrow_mut().read_entry_raw(&link);
        let size = ar
            .borrow()
            .manifest
            .get(&link)
            .map_or(0, |l| l.uncompressed_filesize);
        data.truncate(size as usize);
        Ok(data)
    }

    /// `compress()`.
    pub fn compress(&self, method: i64) -> PharResult<bool> {
        let e = self.entry()?;
        let (ar, er) = self.get()?;
        let fname = ar.borrow().fname.clone();
        if e.is_tar {
            return Err(PharError::bad_method(
                "Cannot compress with Gzip compression, not possible with tar-based phar archives",
            ));
        }
        if e.is_dir {
            return Err(PharError::bad_method(
                "Phar entry is a directory, cannot set compression",
            ));
        }
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method(
                "Phar is readonly, cannot change compression",
            ));
        }
        if e.is_deleted {
            return Err(PharError::bad_method("Cannot compress deleted file"));
        }
        self.cow()?;
        let (ar, _) = self.get()?;
        let key = er.key().unwrap_or_default().to_vec();
        let (target, other, other_name, this_name) = match method {
            m if m == i64::from(ENT_COMPRESSED_GZ) => {
                (ENT_COMPRESSED_GZ, ENT_COMPRESSED_BZ2, "bzip2", "gzip")
            }
            m if m == i64::from(ENT_COMPRESSED_BZ2) => {
                (ENT_COMPRESSED_BZ2, ENT_COMPRESSED_GZ, "gzip", "bzip2")
            }
            _ => return Err(PharError::bad_method("Unknown compression type specified")),
        };
        if e.flags & target != 0 {
            return Ok(true);
        }
        let (has_target, has_other) = if target == ENT_COMPRESSED_GZ {
            (reg::has_zlib(), reg::has_bz2())
        } else {
            (reg::has_bz2(), reg::has_zlib())
        };
        if e.flags & other != 0 {
            if !has_other {
                return Err(PharError::bad_method(format!(
                    "Cannot compress with {this_name} compression, file is already compressed with {other_name} compression and {} extension is not enabled, cannot decompress",
                    if target == ENT_COMPRESSED_GZ { "bz2" } else { "zlib" }
                )));
            }
            ar.borrow_mut().open_entry_fp(&key, true).map_err(|err| {
                PharError::bad_method(format!(
                    "phar error: Cannot decompress {other_name}-compressed file \"{}\" in phar \"{}\" in order to compress with {this_name}: {err}",
                    s(&e.filename),
                    s(&fname)
                ))
            })?;
        }
        if !has_target {
            return Err(PharError::bad_method(format!(
                "Cannot compress with {this_name} compression, {} extension is not enabled",
                if target == ENT_COMPRESSED_GZ {
                    "zlib"
                } else {
                    "bz2"
                }
            )));
        }
        let ar = self.update(|e| {
            e.old_flags = e.flags;
            e.flags &= !ENT_COMPRESSION_MASK;
            e.flags |= target;
            e.is_modified = true;
        })?;
        ar.borrow_mut().is_modified = true;
        flush_err(&ar)?;
        Ok(true)
    }

    /// `decompress()`.
    pub fn decompress(&self) -> PharResult<bool> {
        let e = self.entry()?;
        let (ar, er) = self.get()?;
        let fname = ar.borrow().fname.clone();
        if e.is_dir {
            return Err(PharError::bad_method(
                "Phar entry is a directory, cannot set compression",
            ));
        }
        if e.flags & ENT_COMPRESSION_MASK == 0 {
            return Ok(true);
        }
        if reg::readonly() && !ar.borrow().is_data {
            return Err(PharError::bad_method("Phar is readonly, cannot decompress"));
        }
        if e.is_deleted {
            return Err(PharError::bad_method("Cannot compress deleted file"));
        }
        if e.flags & ENT_COMPRESSED_GZ != 0 && !reg::has_zlib() {
            return Err(PharError::bad_method(
                "Cannot decompress Gzip-compressed file, zlib extension is not enabled",
            ));
        }
        if e.flags & ENT_COMPRESSED_BZ2 != 0 && !reg::has_bz2() {
            return Err(PharError::bad_method(
                "Cannot decompress Bzip2-compressed file, bz2 extension is not enabled",
            ));
        }
        self.cow()?;
        let (ar, _) = self.get()?;
        let kind = match e.flags & ENT_COMPRESSION_MASK {
            ENT_COMPRESSED_GZ => "gzip",
            ENT_COMPRESSED_BZ2 => "bz2",
            _ => {
                return Err(PharError::bad_method(
                    "Cannot decompress file compressed with unknown compression type",
                ))
            }
        };
        let key = er.key().unwrap_or_default().to_vec();
        ar.borrow_mut().open_entry_fp(&key, true).map_err(|err| {
            PharError::bad_method(format!(
                "Phar error: Cannot decompress {kind}-compressed file \"{}\" in phar \"{}\": {err}",
                s(&e.filename),
                s(&fname)
            ))
        })?;
        let ar = self.update(|e| {
            e.old_flags = e.flags;
            e.flags &= !ENT_COMPRESSION_MASK;
            e.is_modified = true;
        })?;
        ar.borrow_mut().is_modified = true;
        flush_err(&ar)?;
        Ok(true)
    }

    /// The underlying entry (for stat-like accessors).
    pub fn entry_info(&self) -> PharResult<Entry> {
        self.entry()
    }
}
