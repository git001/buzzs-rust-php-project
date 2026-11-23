// buzzs-rust-php-project - Copyright (c) 2026 git001 (Aleks)
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Cache encoding of a whole [`CompiledUnit`]: functions, op arrays,
//! classes and declaration tables, with the identity of every shared
//! `Rc` preserved.
//!
//! Each shared object is written once, at its first use, and referred to
//! by id afterwards (the `zend_shared_alloc_get_xlat_entry` translation
//! table of `zend_file_cache_serialize`). Ids are assigned in first-use
//! order on both sides: the encoder numbers an object before writing its
//! body, the decoder reserves the slot before reading it, so nested
//! objects get the same ids. Declaration tables are the exception: as in
//! [`crate::persistence`]'s `ImageBuilder`, a table is created empty at
//! its first use and filled after the unit's main code, so a function
//! whose table lists functions cannot recurse into itself.
//!
//! Runtime-only state (call caches, linked-class `Weak`s, static
//! variable slots, the decoded tape, the lazy call shape) is not stored;
//! a decoded unit starts with all of it empty, as a freshly compiled one.
//!
//! ref: `ext/opcache/zend_file_cache.c` (`zend_file_cache_serialize_op_array`,
//! `zend_file_cache_serialize_class`, `zend_file_cache_serialize_func`)

use super::codec::{Codec, DResult, DecodeError, EResult, Reader, Writer};
use crate::class::{
    AttributeInfo, ClassDecl, ClassKind, ConstDecl, ConstExpr, MethodDecl, PropDecl, PropHooks,
    TraitRule,
};
use crate::compiler::CompiledUnit;
use crate::func::{ClosureUse, UserFunction};
use crate::opcode::{DeclTable, OpArray, ParamInfo};
use crate::types::{TypeDecl, TypeMember};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Shared objects already written, by the address of their `Rc`, with
/// the id they were given.
#[derive(Default)]
struct Ids {
    map: HashMap<usize, u32>,
    next: u32,
}

impl Ids {
    /// The id of `ptr` and whether it is new (its body must be written).
    fn get<T>(&mut self, ptr: *const T) -> (u32, bool) {
        let key = ptr as usize;
        if let Some(&id) = self.map.get(&key) {
            return (id, false);
        }
        let id = self.next;
        self.next += 1;
        self.map.insert(key, id);
        (id, true)
    }
}

/// Writes a compiled unit.
struct Encoder<'w> {
    w: &'w mut Writer,
    functions: Ids,
    code: Ids,
    classes: Ids,
    hooks: Ids,
    attr_lists: Ids,
    tables: Ids,
    /// Tables referenced but not yet written.
    pending: Vec<(u32, Rc<DeclTable>)>,
}

impl Encoder<'_> {
    fn table(&mut self, table: &Rc<DeclTable>) {
        let (id, new) = self.tables.get(Rc::as_ptr(table));
        self.w.u32(id);
        if new {
            self.pending.push((id, table.clone()));
        }
    }

    fn expr(&mut self, expr: &ConstExpr) -> EResult {
        match expr {
            ConstExpr::Value(v) => {
                self.w.u8(0);
                v.encode(self.w)
            }
            ConstExpr::Code(code) => {
                self.w.u8(1);
                self.code(code)
            }
        }
    }

    fn opt_expr(&mut self, expr: Option<&ConstExpr>) -> EResult {
        match expr {
            None => {
                self.w.u8(0);
                Ok(())
            }
            Some(e) => {
                self.w.u8(1);
                self.expr(e)
            }
        }
    }

    fn attributes(&mut self, attrs: &[AttributeInfo]) -> EResult {
        self.w.count(attrs.len());
        for a in attrs {
            self.w.zstr(&a.name);
            self.w.count(a.args.len());
            for (name, value) in &a.args {
                name.encode(self.w)?;
                self.expr(value)?;
            }
            self.w.u32(a.line);
            self.w.bool(a.strict);
        }
        Ok(())
    }

    fn attr_list(&mut self, attrs: &Rc<Vec<AttributeInfo>>) -> EResult {
        let (id, new) = self.attr_lists.get(Rc::as_ptr(attrs));
        self.w.u32(id);
        if new {
            self.attributes(attrs)?;
        }
        Ok(())
    }

    fn params(&mut self, params: &[ParamInfo]) -> EResult {
        self.w.count(params.len());
        for p in params {
            self.w.zstr(&p.name);
            self.w.bool(p.by_ref);
            self.w.bool(p.variadic);
            p.ty.encode(self.w)?;
            self.w.bool(p.has_default);
            p.default_text.encode(self.w)?;
            self.w.u32(p.promote);
            self.w.bool(p.sensitive);
            self.opt_expr(p.default_expr.as_ref())?;
            self.attributes(&p.attributes)?;
        }
        Ok(())
    }

    fn code(&mut self, code: &Rc<OpArray>) -> EResult {
        let (id, new) = self.code.get(Rc::as_ptr(code));
        self.w.u32(id);
        if !new {
            return Ok(());
        }
        code.ops.encode(self.w)?;
        code.lines.encode(self.w)?;
        code.literals.encode(self.w)?;
        code.places.encode(self.w)?;
        code.cv_names.encode(self.w)?;
        self.w.u32(code.num_tmps);
        self.w.u32(code.num_iters);
        self.w.u32(code.num_finally);
        self.w.u32(code.num_statics);
        code.static_vars.encode(self.w)?;
        code.try_table.encode(self.w)?;
        code.live_tmps.encode(self.w)?;
        self.w.zstr(&code.filename);
        self.w.bool(code.strict_types);
        self.table(&code.decls);
        Ok(())
    }

    fn function(&mut self, f: &Rc<UserFunction>) -> EResult {
        let (id, new) = self.functions.get(Rc::as_ptr(f));
        self.w.u32(id);
        if !new {
            return Ok(());
        }
        self.w.zstr(&f.name);
        self.params(&f.params)?;
        self.w.u32(f.required);
        f.return_type.encode(self.w)?;
        self.w.u32(f.flags);
        self.code(&f.code)?;
        self.w.u32(f.line_start);
        self.w.u32(f.line_end);
        f.doc_comment.encode(self.w)?;
        f.uses.encode(self.w)?;
        f.class_name.encode(self.w)?;
        self.attributes(&f.attributes)?;
        f.hook_prop.encode(self.w)
    }

    fn opt_function(&mut self, f: Option<&Rc<UserFunction>>) -> EResult {
        match f {
            None => {
                self.w.u8(0);
                Ok(())
            }
            Some(f) => {
                self.w.u8(1);
                self.function(f)
            }
        }
    }

    fn hooks(&mut self, h: &Rc<PropHooks>) -> EResult {
        let (id, new) = self.hooks.get(Rc::as_ptr(h));
        self.w.u32(id);
        if !new {
            return Ok(());
        }
        self.opt_function(h.get.as_ref())?;
        self.opt_function(h.set.as_ref())?;
        self.w.bool(h.final_get);
        self.w.bool(h.final_set);
        self.w.bool(h.abstract_get);
        self.w.bool(h.abstract_set);
        self.w.zstr(&h.class);
        Ok(())
    }

    fn class(&mut self, c: &Rc<ClassDecl>) -> EResult {
        let (id, new) = self.classes.get(Rc::as_ptr(c));
        self.w.u32(id);
        if !new {
            return Ok(());
        }
        self.w.zstr(&c.name);
        c.kind.encode(self.w)?;
        self.w.u32(c.flags);
        c.parent.encode(self.w)?;
        c.interfaces.encode(self.w)?;
        c.traits.encode(self.w)?;
        c.trait_rules.encode(self.w)?;
        self.w.count(c.consts.len());
        for k in &c.consts {
            self.w.zstr(&k.name);
            self.expr(&k.value)?;
            self.w.u32(k.flags);
            k.ty.encode(self.w)?;
            k.doc_comment.encode(self.w)?;
            self.attributes(&k.attributes)?;
            self.w.u32(k.line);
            self.w.u32(k.cases_before);
        }
        self.w.count(c.props.len());
        for p in &c.props {
            self.w.zstr(&p.name);
            self.opt_expr(p.default.as_ref())?;
            self.w.u32(p.flags);
            p.ty.encode(self.w)?;
            p.doc_comment.encode(self.w)?;
            self.attributes(&p.attributes)?;
            self.w.u32(p.line);
            match &p.hooks {
                None => self.w.u8(0),
                Some(h) => {
                    self.w.u8(1);
                    self.hooks(h)?;
                }
            }
        }
        self.w.count(c.methods.len());
        for m in &c.methods {
            self.w.zstr(&m.name);
            self.function(&m.func)?;
            self.w.u32(m.flags);
        }
        self.w.count(c.cases.len());
        for (name, value, line) in &c.cases {
            self.w.zstr(name);
            self.opt_expr(value.as_ref())?;
            self.w.u32(*line);
        }
        self.w.count(c.case_attributes.len());
        for attrs in &c.case_attributes {
            self.attributes(attrs)?;
        }
        c.case_doc_comments.encode(self.w)?;
        c.backing.encode(self.w)?;
        self.attributes(&c.attributes)?;
        c.doc_comment.encode(self.w)?;
        self.w.zstr(&c.filename);
        self.w.u32(c.line_start);
        self.w.u32(c.line_end);
        Ok(())
    }

    /// Writes the bodies of the referenced tables until none is left;
    /// a body may reference further tables. A `0` ends the list.
    fn tables(&mut self) -> EResult {
        while let Some((id, table)) = self.pending.pop() {
            self.w.u8(1);
            self.w.u32(id);
            let functions = table.functions.borrow().clone();
            self.w.count(functions.len());
            for f in &functions {
                self.function(f)?;
            }
            let classes = table.classes.borrow().clone();
            self.w.count(classes.len());
            for c in &classes {
                self.class(c)?;
            }
            let attrs = table.const_attributes.borrow().clone();
            self.w.count(attrs.len());
            for a in &attrs {
                self.attr_list(a)?;
            }
        }
        self.w.u8(0);
        Ok(())
    }

    fn unit(&mut self, unit: &CompiledUnit) -> EResult {
        self.function(&unit.main)?;
        unit.hoisted_functions.encode(self.w)?;
        unit.hoisted_classes.encode(self.w)?;
        self.table(&unit.decls);
        unit.warnings.encode(self.w)?;
        self.w.u32(unit.anon_end);
        self.w.u32(unit.meta.anon_base);
        unit.meta.halt_offset.encode(self.w)?;
        unit.meta.auto_globals.encode(self.w)?;
        self.tables()
    }
}

/// Appends `unit` to `w`.
///
/// # Errors
/// When the unit holds a value without a compile-time form; such a unit
/// is not cached.
pub fn encode_unit(unit: &CompiledUnit, w: &mut Writer) -> EResult {
    Encoder {
        w,
        functions: Ids::default(),
        code: Ids::default(),
        classes: Ids::default(),
        hooks: Ids::default(),
        attr_lists: Ids::default(),
        tables: Ids::default(),
        pending: Vec::new(),
    }
    .unit(unit)
}

/// Objects decoded so far, by id; `None` while an object's body is
/// being read (a reference to it then would be a cycle).
struct Slots<T>(Vec<Option<Rc<T>>>);

impl<T> Default for Slots<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Slots<T> {
    /// The object `id` refers to, or `None` when `id` is the next new
    /// one (its slot is then reserved and its body follows).
    fn lookup(&mut self, id: u32) -> DResult<Option<Rc<T>>> {
        let idx = id as usize;
        match idx.cmp(&self.0.len()) {
            std::cmp::Ordering::Less => self.0[idx]
                .clone()
                .map(Some)
                .ok_or(DecodeError("reference cycle")),
            std::cmp::Ordering::Equal => {
                self.0.push(None);
                Ok(None)
            }
            std::cmp::Ordering::Greater => Err(DecodeError("object id out of order")),
        }
    }

    fn fill(&mut self, id: u32, obj: Rc<T>) -> Rc<T> {
        self.0[id as usize] = Some(obj.clone());
        obj
    }
}

/// Reads a compiled unit.
struct Decoder<'r, 'a> {
    r: &'r mut Reader<'a>,
    functions: Slots<UserFunction>,
    code: Slots<OpArray>,
    classes: Slots<ClassDecl>,
    hooks: Slots<PropHooks>,
    attr_lists: Slots<Vec<AttributeInfo>>,
    /// Tables are created empty at first reference and filled later.
    tables: Vec<Rc<DeclTable>>,
    filled: Vec<bool>,
}

impl Decoder<'_, '_> {
    fn table(&mut self) -> DResult<Rc<DeclTable>> {
        let idx = self.r.u32()? as usize;
        match idx.cmp(&self.tables.len()) {
            std::cmp::Ordering::Less => Ok(self.tables[idx].clone()),
            std::cmp::Ordering::Equal => {
                let t = Rc::new(DeclTable::default());
                self.tables.push(t.clone());
                self.filled.push(false);
                Ok(t)
            }
            std::cmp::Ordering::Greater => Err(DecodeError("table id out of order")),
        }
    }

    fn expr(&mut self) -> DResult<ConstExpr> {
        match self.r.u8()? {
            0 => Ok(ConstExpr::Value(php_runtime::Value::decode(self.r)?)),
            1 => Ok(ConstExpr::Code(self.code()?)),
            _ => Err(DecodeError("invalid const expr tag")),
        }
    }

    fn opt_expr(&mut self) -> DResult<Option<ConstExpr>> {
        match self.r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.expr()?)),
            _ => Err(DecodeError("invalid option tag")),
        }
    }

    fn attributes(&mut self) -> DResult<Vec<AttributeInfo>> {
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let name = self.r.zstr()?;
            let arg_count = self.r.count()?;
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: count limited by the cache input"
            )]
            let mut args = Vec::with_capacity(arg_count);
            for _ in 0..arg_count {
                let arg_name = Option::decode(self.r)?;
                args.push((arg_name, self.expr()?));
            }
            out.push(AttributeInfo {
                name,
                args,
                line: self.r.u32()?,
                strict: self.r.bool()?,
            });
        }
        Ok(out)
    }

    fn attr_list(&mut self) -> DResult<Rc<Vec<AttributeInfo>>> {
        let id = self.r.u32()?;
        if let Some(a) = self.attr_lists.lookup(id)? {
            return Ok(a);
        }
        let attrs = Rc::new(self.attributes()?);
        Ok(self.attr_lists.fill(id, attrs))
    }

    fn params(&mut self) -> DResult<Vec<ParamInfo>> {
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(ParamInfo {
                name: self.r.zstr()?,
                by_ref: self.r.bool()?,
                variadic: self.r.bool()?,
                ty: Option::decode(self.r)?,
                has_default: self.r.bool()?,
                default_text: Option::decode(self.r)?,
                promote: self.r.u32()?,
                sensitive: self.r.bool()?,
                default_expr: self.opt_expr()?,
                attributes: self.attributes()?,
            });
        }
        Ok(out)
    }

    fn code(&mut self) -> DResult<Rc<OpArray>> {
        let id = self.r.u32()?;
        if let Some(c) = self.code.lookup(id)? {
            return Ok(c);
        }
        let code = Rc::new(OpArray {
            ops: Vec::decode(self.r)?,
            lines: Vec::decode(self.r)?,
            literals: Vec::decode(self.r)?,
            places: Vec::decode(self.r)?,
            cv_names: Vec::decode(self.r)?,
            num_tmps: self.r.u32()?,
            num_iters: self.r.u32()?,
            num_finally: self.r.u32()?,
            num_statics: self.r.u32()?,
            static_vars: Vec::decode(self.r)?,
            try_table: Vec::decode(self.r)?,
            live_tmps: Vec::decode(self.r)?,
            filename: self.r.zstr()?,
            strict_types: self.r.bool()?,
            decls: self.table()?,
            fcall_cache: RefCell::new(Vec::new()),
            class_cache: RefCell::new(Vec::new()),
            rt_cache: crate::rtcache::RuntimeCache::default(),
            prop_cache: RefCell::new(Vec::new()),
            method_cache: RefCell::new(Vec::new()),
            tape: crate::exec::tape::Tape::default(),
        });
        if code.lines.len() != code.ops.len() {
            return Err(DecodeError("line table does not match ops"));
        }
        Ok(self.code.fill(id, code))
    }

    fn function(&mut self) -> DResult<Rc<UserFunction>> {
        let id = self.r.u32()?;
        if let Some(f) = self.functions.lookup(id)? {
            return Ok(f);
        }
        let f = Rc::new(UserFunction {
            name: self.r.zstr()?,
            params: self.params()?,
            required: self.r.u32()?,
            return_type: Option::decode(self.r)?,
            flags: self.r.u32()?,
            code: self.code()?,
            line_start: self.r.u32()?,
            line_end: self.r.u32()?,
            doc_comment: Option::decode(self.r)?,
            statics: RefCell::new(Vec::new()),
            uses: Vec::decode(self.r)?,
            class_name: Option::decode(self.r)?,
            attributes: self.attributes()?,
            hook_prop: Option::decode(self.r)?,
            call_shape: crate::func::CallShape::default(),
        });
        Ok(self.functions.fill(id, f))
    }

    fn opt_function(&mut self) -> DResult<Option<Rc<UserFunction>>> {
        match self.r.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.function()?)),
            _ => Err(DecodeError("invalid option tag")),
        }
    }

    fn hooks(&mut self) -> DResult<Rc<PropHooks>> {
        let id = self.r.u32()?;
        if let Some(h) = self.hooks.lookup(id)? {
            return Ok(h);
        }
        let h = Rc::new(PropHooks {
            get: self.opt_function()?,
            set: self.opt_function()?,
            final_get: self.r.bool()?,
            final_set: self.r.bool()?,
            abstract_get: self.r.bool()?,
            abstract_set: self.r.bool()?,
            class: self.r.zstr()?,
        });
        Ok(self.hooks.fill(id, h))
    }

    fn class(&mut self) -> DResult<Rc<ClassDecl>> {
        let id = self.r.u32()?;
        if let Some(c) = self.classes.lookup(id)? {
            return Ok(c);
        }
        let name = self.r.zstr()?;
        let kind = ClassKind::decode(self.r)?;
        let flags = self.r.u32()?;
        let parent = Option::decode(self.r)?;
        let interfaces = Vec::decode(self.r)?;
        let traits = Vec::decode(self.r)?;
        let trait_rules = Vec::decode(self.r)?;
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut consts = Vec::with_capacity(n);
        for _ in 0..n {
            consts.push(ConstDecl {
                name: self.r.zstr()?,
                value: self.expr()?,
                flags: self.r.u32()?,
                ty: Option::decode(self.r)?,
                doc_comment: Option::decode(self.r)?,
                attributes: self.attributes()?,
                line: self.r.u32()?,
                cases_before: self.r.u32()?,
            });
        }
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut props = Vec::with_capacity(n);
        for _ in 0..n {
            props.push(PropDecl {
                name: self.r.zstr()?,
                default: self.opt_expr()?,
                flags: self.r.u32()?,
                ty: Option::decode(self.r)?,
                doc_comment: Option::decode(self.r)?,
                attributes: self.attributes()?,
                line: self.r.u32()?,
                hooks: match self.r.u8()? {
                    0 => None,
                    1 => Some(self.hooks()?),
                    _ => return Err(DecodeError("invalid option tag")),
                },
            });
        }
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut methods = Vec::with_capacity(n);
        for _ in 0..n {
            methods.push(MethodDecl {
                name: self.r.zstr()?,
                func: self.function()?,
                flags: self.r.u32()?,
            });
        }
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut cases = Vec::with_capacity(n);
        for _ in 0..n {
            cases.push((self.r.zstr()?, self.opt_expr()?, self.r.u32()?));
        }
        let n = self.r.count()?;
        #[expect(
            clippy::disallowed_methods,
            reason = "bounded: count limited by the cache input"
        )]
        let mut case_attributes = Vec::with_capacity(n);
        for _ in 0..n {
            case_attributes.push(self.attributes()?);
        }
        let c = Rc::new(ClassDecl {
            name,
            kind,
            flags,
            parent,
            interfaces,
            traits,
            trait_rules,
            consts,
            props,
            methods,
            cases,
            case_attributes,
            case_doc_comments: Vec::decode(self.r)?,
            backing: Option::decode(self.r)?,
            attributes: self.attributes()?,
            doc_comment: Option::decode(self.r)?,
            filename: self.r.zstr()?,
            line_start: self.r.u32()?,
            line_end: self.r.u32()?,
        });
        Ok(self.classes.fill(id, c))
    }

    fn tables(&mut self) -> DResult<()> {
        loop {
            match self.r.u8()? {
                0 => break,
                1 => {}
                _ => return Err(DecodeError("invalid table marker")),
            }
            let idx = self.r.u32()? as usize;
            if self.filled.get(idx).copied() != Some(false) {
                return Err(DecodeError("table body for unknown or filled table"));
            }
            self.filled[idx] = true;
            let table = self.tables[idx].clone();
            let n = self.r.count()?;
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: count limited by the cache input"
            )]
            let mut functions = Vec::with_capacity(n);
            for _ in 0..n {
                functions.push(self.function()?);
            }
            let n = self.r.count()?;
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: count limited by the cache input"
            )]
            let mut classes = Vec::with_capacity(n);
            for _ in 0..n {
                classes.push(self.class()?);
            }
            let n = self.r.count()?;
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: count limited by the cache input"
            )]
            let mut attrs = Vec::with_capacity(n);
            for _ in 0..n {
                attrs.push(self.attr_list()?);
            }
            *table.functions.borrow_mut() = functions;
            *table.classes.borrow_mut() = classes;
            *table.const_attributes.borrow_mut() = attrs;
        }
        if self.filled.iter().all(|&f| f) {
            Ok(())
        } else {
            Err(DecodeError("declaration table without body"))
        }
    }

    fn unit(&mut self) -> DResult<CompiledUnit> {
        let main = self.function()?;
        let unit = CompiledUnit {
            main,
            hoisted_functions: Vec::decode(self.r)?,
            hoisted_classes: Vec::decode(self.r)?,
            decls: self.table()?,
            warnings: Vec::decode(self.r)?,
            anon_end: self.r.u32()?,
            meta: crate::compiler::UnitMeta {
                anon_base: self.r.u32()?,
                halt_offset: Option::decode(self.r)?,
                auto_globals: Vec::decode(self.r)?,
            },
        };
        self.tables()?;
        Ok(unit)
    }
}

/// Reads a unit written by [`encode_unit`].
///
/// # Errors
/// When the input is truncated or malformed.
pub fn decode_unit(r: &mut Reader<'_>) -> DResult<CompiledUnit> {
    Decoder {
        r,
        functions: Slots::default(),
        code: Slots::default(),
        classes: Slots::default(),
        hooks: Slots::default(),
        attr_lists: Slots::default(),
        tables: Vec::new(),
        filled: Vec::new(),
    }
    .unit()
}

impl Codec for ClassKind {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.u8(match self {
            ClassKind::Class => 0,
            ClassKind::Interface => 1,
            ClassKind::Trait => 2,
            ClassKind::Enum => 3,
        });
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(match r.u8()? {
            0 => ClassKind::Class,
            1 => ClassKind::Interface,
            2 => ClassKind::Trait,
            3 => ClassKind::Enum,
            _ => return Err(DecodeError("invalid class kind")),
        })
    }
}

impl Codec for TraitRule {
    fn encode(&self, w: &mut Writer) -> EResult {
        match self {
            TraitRule::Precedence {
                trait_name,
                method,
                insteadof,
            } => {
                w.u8(0);
                w.zstr(trait_name);
                w.zstr(method);
                insteadof.encode(w)
            }
            TraitRule::Alias {
                trait_name,
                method,
                flags,
                alias,
            } => {
                w.u8(1);
                trait_name.encode(w)?;
                w.zstr(method);
                w.u32(*flags);
                alias.encode(w)
            }
        }
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(match r.u8()? {
            0 => TraitRule::Precedence {
                trait_name: r.zstr()?,
                method: r.zstr()?,
                insteadof: Vec::decode(r)?,
            },
            1 => TraitRule::Alias {
                trait_name: Option::decode(r)?,
                method: r.zstr()?,
                flags: r.u32()?,
                alias: Option::decode(r)?,
            },
            _ => return Err(DecodeError("invalid trait rule")),
        })
    }
}

impl Codec for TypeMember {
    fn encode(&self, w: &mut Writer) -> EResult {
        match self {
            TypeMember::Class(i) => {
                w.u8(0);
                w.count(*i);
            }
            TypeMember::Intersection(i) => {
                w.u8(1);
                w.count(*i);
            }
            TypeMember::Iterable => w.u8(2),
        }
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(match r.u8()? {
            0 => TypeMember::Class(usize::decode(r)?),
            1 => TypeMember::Intersection(usize::decode(r)?),
            2 => TypeMember::Iterable,
            _ => return Err(DecodeError("invalid type member")),
        })
    }
}

impl Codec for TypeDecl {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.u32(self.mask);
        self.classes.encode(w)?;
        self.intersections.encode(w)?;
        w.bool(self.pure_intersection);
        self.members.encode(w)
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(TypeDecl {
            mask: r.u32()?,
            classes: Vec::decode(r)?,
            intersections: Vec::decode(r)?,
            pure_intersection: r.bool()?,
            members: Vec::decode(r)?,
            lc_cache: std::cell::OnceCell::new(),
        })
    }
}

impl Codec for ClosureUse {
    fn encode(&self, w: &mut Writer) -> EResult {
        w.zstr(&self.name);
        w.bool(self.by_ref);
        w.u32(self.parent_cv);
        w.u32(self.cv);
        w.bool(self.implicit);
        Ok(())
    }
    fn decode(r: &mut Reader<'_>) -> DResult<Self> {
        Ok(ClosureUse {
            name: r.zstr()?,
            by_ref: r.bool()?,
            parent_cv: r.u32()?,
            cv: r.u32()?,
            implicit: r.bool()?,
        })
    }
}
