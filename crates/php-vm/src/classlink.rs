// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Class declaration and linking (`zend_do_inheritance`,
//! `zend_do_bind_traits`, interface implementation), class lookup with
//! autoloading, class constants and static properties.

use crate::class::{
    cflags, name_map_with_capacity, Class, ClassConst, ClassDecl, ClassKind, ConstExpr,
    InternalKind, Magic, NameMap, PropInfo, StaticProp, TraitRule,
};
use crate::func::{flags, Func, Method, UserFunction};
use crate::object;
use crate::opcode::ClassSpec;
use crate::vm::{Frame, FrameKind, Throw, Vm, VmResult};
use php_runtime::{Value, ZStr};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// Builds magic-method shortcuts from a method table.
pub(crate) fn build_magic(methods: &NameMap<Rc<Method>>) -> Magic {
    // Up to this many methods, one pass over the table that only looks at
    // `__` names is cheaper than sixteen hashed lookups.
    const SCAN_MAX: usize = 32;
    if methods.len() <= SCAN_MAX {
        let mut m = Magic::default();
        for (lc, method) in methods {
            if !lc.as_bytes().starts_with(b"__") {
                continue;
            }
            let slot = match lc.as_bytes() {
                b"__construct" => &mut m.construct,
                b"__destruct" => &mut m.destruct,
                b"__get" => &mut m.get,
                b"__set" => &mut m.set,
                b"__isset" => &mut m.isset,
                b"__unset" => &mut m.unset,
                b"__call" => &mut m.call,
                b"__callstatic" => &mut m.call_static,
                b"__tostring" => &mut m.to_string,
                b"__invoke" => &mut m.invoke,
                b"__clone" => &mut m.clone,
                b"__debuginfo" => &mut m.debug_info,
                b"__serialize" => &mut m.serialize,
                b"__unserialize" => &mut m.unserialize,
                b"__sleep" => &mut m.sleep,
                b"__wakeup" => &mut m.wakeup,
                _ => continue,
            };
            *slot = Some(method.clone());
        }
        return m;
    }
    let g = |n: &str| methods.get(n.as_bytes()).cloned();
    Magic {
        construct: g("__construct"),
        destruct: g("__destruct"),
        get: g("__get"),
        set: g("__set"),
        isset: g("__isset"),
        unset: g("__unset"),
        call: g("__call"),
        call_static: g("__callstatic"),
        to_string: g("__tostring"),
        invoke: g("__invoke"),
        clone: g("__clone"),
        debug_info: g("__debuginfo"),
        serialize: g("__serialize"),
        unserialize: g("__unserialize"),
        sleep: g("__sleep"),
        wakeup: g("__wakeup"),
    }
}

impl Vm {
    /// Looks up a class, autoloading it if needed.
    pub fn lookup_class(&mut self, name: &ZStr) -> VmResult<Option<Rc<Class>>> {
        let bytes = name
            .as_bytes()
            .strip_prefix(b"\\")
            .unwrap_or(name.as_bytes());
        let lc = ZStr::from(bytes.to_ascii_lowercase());
        if let Some(c) = self.classes.get(&lc) {
            return Ok(Some(c.clone()));
        }
        // `zend_lookup_class_ex`: a class being linked sits unlinked in the
        // class table: it is not found, and not autoloaded either.
        if unlinked_decl(bytes).is_some() {
            return Ok(None);
        }
        self.autoload(&ZStr::from(bytes), &lc)?;
        Ok(self.classes.get(&lc).cloned())
    }

    /// Looks up a class without autoloading.
    pub fn find_class(&self, name: &[u8]) -> Option<Rc<Class>> {
        let bytes = name.strip_prefix(b"\\").unwrap_or(name);
        self.classes
            .get(&ZStr::from(bytes.to_ascii_lowercase()))
            .cloned()
    }

    fn autoload(&mut self, name: &ZStr, lc: &ZStr) -> VmResult<()> {
        if self.autoloaders.is_empty() || self.autoloading.contains(lc) {
            return Ok(());
        }
        if !is_valid_class_name(name.as_bytes()) {
            return Ok(());
        }
        self.autoloading.insert(lc.clone());
        let r = self.run_class_loaders(name, lc);
        self.autoloading.remove(lc);
        r
    }

    /// `zend_perform_class_autoload`: calls the registered loaders until
    /// the class exists. The live table is walked, as loaders may be
    /// registered or unregistered by a running loader: one added at the
    /// end is still called for this class, a removed one is skipped.
    fn run_class_loaders(&mut self, name: &ZStr, lc: &ZStr) -> VmResult<()> {
        let mut pos = 0;
        while let Some(loader) = self.autoloaders.get(pos).cloned() {
            let arg = Value::Str(name.clone());
            match loader.fcc.clone() {
                Some(mut call) => {
                    call.args = vec![Some(arg)];
                    self.call_pending(call)?;
                }
                None => {
                    self.call_value(&loader.callable, vec![arg])?;
                }
            }
            if self.classes.contains_key(lc) {
                break;
            }
            // Continue after the loader just called; when it unregistered
            // itself, its successor now sits at its position.
            pos = self
                .autoloaders
                .iter()
                .position(|l| l.is_same(&loader))
                .map_or(pos, |i| i + 1);
        }
        Ok(())
    }

    /// Looks up a class or throws `Class "X" not found`.
    pub(crate) fn lookup_class_or_error(&mut self, name: &ZStr) -> VmResult<Rc<Class>> {
        match self.lookup_class(name)? {
            Some(c) => Ok(c),
            None => Err(self.make_throw(
                "Error",
                &format!("Class \"{}\" not found", name.to_string_lossy()),
            )),
        }
    }

    /// Resolves a [`ClassSpec`] at runtime.
    #[inline]
    pub(crate) fn resolve_class_spec(&mut self, spec: &ClassSpec) -> VmResult<Rc<Class>> {
        match spec {
            ClassSpec::Named(l) => {
                let slot = *l as usize;
                let cached = self
                    .frame()
                    .code
                    .class_cache
                    .borrow()
                    .get(slot)
                    .and_then(Weak::upgrade);
                if let Some(c) = cached {
                    return Ok(c);
                }
                let n = self.lit_str(*l);
                let c = self.lookup_class_or_error(&n)?;
                let mut cache = self.frame().code.class_cache.borrow_mut();
                if cache.len() <= slot {
                    crate::rtcache::grow_slots(&mut cache, slot + 1, Weak::new);
                }
                cache[slot] = Rc::downgrade(&c);
                Ok(c)
            }
            ClassSpec::SelfClass => match self.scope() {
                Some(c) => Ok(c),
                None => Err(self.make_throw(
                    "Error",
                    "Cannot access \"self\" when no class scope is active",
                )),
            },
            ClassSpec::Parent => match self.scope() {
                Some(c) => match &c.parent {
                    Some(p) => Ok(p.clone()),
                    None => Err(self.make_throw(
                        "Error",
                        "Cannot access \"parent\" when current class scope has no parent",
                    )),
                },
                None => Err(self.make_throw(
                    "Error",
                    "Cannot access \"parent\" when no class scope is active",
                )),
            },
            ClassSpec::Static => {
                let c = self
                    .frames
                    .iter()
                    .rev()
                    .find(|f| f.kind != FrameKind::Builtin)
                    .and_then(|f| f.called.clone());
                match c {
                    Some(c) => Ok(c),
                    None => Err(self.make_throw(
                        "Error",
                        "Cannot access \"static\" when no class scope is active",
                    )),
                }
            }
            ClassSpec::Dyn(op) => {
                let v = self.read(*op)?;
                match v {
                    Value::Object(o) => Ok(object::class_of(&o).clone()),
                    Value::Str(s) => self.lookup_class_or_error(&s),
                    _ => {
                        Err(self
                            .make_throw("Error", "Class name must be a valid object or a string"))
                    }
                }
            }
        }
    }

    /// `A::class` and friends. `const_expr` marks a fetch compiled inline
    /// in an evaluated constant expression (see [`Op::FetchClassName`]).
    ///
    /// [`Op::FetchClassName`]: crate::opcode::Op::FetchClassName
    pub(crate) fn op_fetch_class_name(
        &mut self,
        spec: &ClassSpec,
        const_expr: bool,
    ) -> VmResult<Value> {
        if let ClassSpec::Dyn(op) = spec {
            let v = self.read(*op)?;
            return match v {
                Value::Object(o) => Ok(Value::Str(object::class_of(&o).name.clone())),
                other => Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "Cannot use \"::class\" on {}",
                        crate::typecheck::given_type(&other)
                    ),
                )),
            };
        }
        // `ZEND_FETCH_CLASS_NAME` (and `ZEND_AST_CLASS_NAME` in constant
        // expressions) word their errors "Cannot use", unlike the class
        // fetches of `resolve_class_spec` ("Cannot access").
        let kw = match spec {
            ClassSpec::SelfClass => "self",
            ClassSpec::Parent => "parent",
            ClassSpec::Static => "static",
            _ => "",
        };
        if !kw.is_empty() {
            let Some(scope) = self.scope() else {
                let msg = if const_expr || self.frame().kind == FrameKind::ConstExpr {
                    "Cannot use \"self\" when no class scope is active".to_string()
                } else {
                    format!("Cannot use \"{kw}\" in the global scope")
                };
                return Err(self.make_throw("Error", &msg));
            };
            let c = match spec {
                ClassSpec::Parent => match &scope.parent {
                    Some(p) => p.clone(),
                    None => {
                        return Err(self.make_throw(
                            "Error",
                            "Cannot use \"parent\" when current class scope has no parent",
                        ))
                    }
                },
                ClassSpec::Static => self
                    .frames
                    .iter()
                    .rev()
                    .find(|f| f.kind != FrameKind::Builtin)
                    .and_then(|f| f.called.clone())
                    .unwrap_or(scope),
                _ => scope,
            };
            return Ok(Value::Str(c.name.clone()));
        }
        let c = self.resolve_class_spec(spec)?;
        Ok(Value::Str(c.name.clone()))
    }

    /// `$a instanceof B`.
    pub(crate) fn op_instanceof(&mut self, v: &Value, spec: &ClassSpec) -> VmResult<bool> {
        let cls = match spec {
            // `ZEND_INSTANCEOF` caches the class once found (it never
            // autoloads, so a miss is looked up again next time).
            ClassSpec::Named(l) => {
                if let Some(c) = self.peek_named_class(*l) {
                    c
                } else {
                    let n = self.lit_str(*l);
                    match self.find_class(n.as_bytes()) {
                        Some(c) => {
                            self.remember_named_class(*l, &c);
                            c
                        }
                        None => return Ok(false),
                    }
                }
            }
            ClassSpec::Dyn(op) => {
                let c = self.read(*op)?;
                match c {
                    Value::Object(o) => object::class_of(&o).clone(),
                    Value::Str(s) => match self.find_class(s.as_bytes()) {
                        Some(c) => c,
                        None => return Ok(false),
                    },
                    _ => {
                        return Err(self
                            .make_throw("Error", "Class name must be a valid object or a string"))
                    }
                }
            }
            other => self.resolve_class_spec(other)?,
        };
        Ok(match v {
            Value::Object(o) => object::class_of(o).is_subclass_of(&cls),
            _ => false,
        })
    }

    // ------------------------------------------------------------------
    // Constants
    // ------------------------------------------------------------------

    /// Evaluates a constant expression in a class scope.
    pub(crate) fn eval_const_expr(
        &mut self,
        e: &ConstExpr,
        scope: Option<&Rc<Class>>,
    ) -> VmResult<Value> {
        match e {
            ConstExpr::Value(v) => Ok(v.clone()),
            ConstExpr::Code(code) => {
                let mut f = Frame::new(code.clone(), FrameKind::ConstExpr);
                f.scope = scope.cloned();
                f.called = scope.cloned();
                self.frames.push(f);
                let base = self.frames.len() - 1;
                self.execute(base)
            }
        }
    }

    /// `A::NAME`.
    pub(crate) fn op_fetch_class_const(
        &mut self,
        spec: &ClassSpec,
        name: Value,
    ) -> VmResult<Value> {
        // `ZEND_FETCH_CLASS_CONSTANT` fetches the class before checking the
        // name's type.
        let cls = self.resolve_class_spec(spec)?;
        let name = match name {
            Value::Str(s) => s,
            other => {
                return Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "Cannot use value of type {} as class constant name",
                        crate::typecheck::given_type(&other)
                    ),
                ))
            }
        };
        // Trait constants are only reachable through a using class.
        if cls.is_trait()
            && cls
                .find_const(name.as_bytes())
                .is_some_and(|c| c.flags & flags::VISIBILITY == flags::PUBLIC)
        {
            return Err(self.make_throw(
                "Error",
                &format!(
                    "Cannot access trait constant {}::{} directly",
                    cls.name,
                    name.to_string_lossy()
                ),
            ));
        }
        // Inside a constant expression php-src goes through
        // `zend_get_class_constant_ex`, whose errors name the class as
        // written (`self::X`) and which marks the constant while evaluating
        // it; the `FETCH_CLASS_CONSTANT` opcode does neither.
        let in_const_expr = self
            .frames
            .last()
            .is_some_and(|f| f.kind == FrameKind::ConstExpr);
        if in_const_expr {
            let shown = match spec {
                ClassSpec::SelfClass => ZStr::from("self"),
                ClassSpec::Parent => ZStr::from("parent"),
                ClassSpec::Static => ZStr::from("static"),
                ClassSpec::Named(l) => self.lit_str(*l),
                ClassSpec::Dyn(_) => cls.name.clone(),
            };
            self.class_constant_ex(&cls, &name, Some(&shown), true)?;
        } else if cls
            .find_const(name.as_bytes())
            .is_some_and(|c| !c.is_case && c.value.borrow().is_none())
        {
            self.class_constant_ex(&cls, &name, None, false)?;
        }
        self.fetch_class_constant(&cls, &name)
    }

    /// Reads a class constant (evaluating it lazily).
    pub(crate) fn class_constant(&mut self, cls: &Rc<Class>, name: &ZStr) -> VmResult<Value> {
        self.class_constant_ex(cls, name, None, true)
    }

    /// `zend_update_class_constant` (from `zend_update_class_constants`):
    /// evaluates a constant without marking it as visited, so a nested
    /// read of it (say, from an autoloader) evaluates it again.
    pub(crate) fn update_class_constant(&mut self, cls: &Rc<Class>, name: &ZStr) -> VmResult<()> {
        self.class_constant_ex(cls, name, None, false)?;
        Ok(())
    }

    /// [`Self::class_constant`] with the class named `shown` in errors
    /// (instead of its resolved name) and optional recursion marking of an
    /// unevaluated constant (`mark`).
    fn class_constant_ex(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
        shown: Option<&ZStr>,
        mark: bool,
    ) -> VmResult<Value> {
        let shown_name = |c: &ZStr| shown.map_or_else(|| object::display_name(c), Clone::clone);
        if name.as_bytes() == b"class" {
            return Ok(Value::Str(cls.name.clone()));
        }
        let Some(c) = cls.find_const(name.as_bytes()).cloned() else {
            return Err(self.make_throw(
                "Error",
                &format!(
                    "Undefined constant {}::{}",
                    shown_name(&cls.name),
                    name.to_string_lossy()
                ),
            ));
        };
        // Visibility.
        if c.flags & flags::VISIBILITY != flags::PUBLIC {
            let scope = self.scope();
            let decl = self.find_class(c.class.as_bytes());
            let ok = match (c.flags & flags::VISIBILITY, &scope, &decl) {
                (flags::PRIVATE, Some(s), Some(d)) => Rc::ptr_eq(s, d),
                (flags::PROTECTED, Some(s), Some(d)) => s.is_subclass_of(d) || d.is_subclass_of(s),
                _ => false,
            };
            if !ok {
                let vis = if c.flags & flags::PRIVATE != 0 {
                    "private"
                } else {
                    "protected"
                };
                return Err(self.make_throw(
                    "Error",
                    &format!(
                        "Cannot access {vis} constant {}::{}",
                        shown_name(&cls.name),
                        name.to_string_lossy()
                    ),
                ));
            }
        }
        if let Some(v) = c.value.borrow().as_ref() {
            return Ok(v.clone());
        }
        if c.is_case {
            return self.enum_case(cls, name);
        }
        if c.evaluating.get() {
            return Err(self.make_throw(
                "Error",
                &format!(
                    "Cannot declare self-referencing constant {}::{}",
                    shown_name(&c.class),
                    name.to_string_lossy()
                ),
            ));
        }
        if mark {
            c.evaluating.set(true);
        }
        let decl = self
            .find_class(c.class.as_bytes())
            .unwrap_or_else(|| cls.clone());
        let r = self.eval_const_expr(&c.expr, Some(&decl));
        if mark {
            c.evaluating.set(false);
        }
        let mut v = r?;
        if let Some(ty) = &c.ty {
            let ctx = crate::typecheck::TypeCtx {
                scope: Some(&decl),
                called: Some(&decl),
                strict: true,
                internal_arg: None,
            };
            let ty = ty.clone();
            if !self.verify_type(&ty, &mut v, &ctx)? {
                return Err(self.make_throw(
                    "TypeError",
                    &format!(
                        "Cannot assign {} to class constant {}::{} of type {}",
                        crate::typecheck::given_type(&v),
                        object::display_name(&c.class),
                        name.to_string_lossy(),
                        ty.to_php_string()
                    ),
                ));
            }
        }
        *c.value.borrow_mut() = Some(v.clone());
        Ok(v)
    }

    /// Defines a global constant (`const X = ...;` / `define`).
    pub(crate) fn declare_constant(&mut self, name: ZStr, v: Value) -> VmResult<bool> {
        // `zend_register_constant()` identifier deprecations (the namespace
        // part is already lowercased by the callers).
        let short = name
            .as_bytes()
            .rsplit(|&c| c == b'\\')
            .next()
            .unwrap_or_default();
        if short == b"_" {
            self.deprecated("Calling a constant \"_\" is deprecated since 8.6")?;
        } else if short.eq_ignore_ascii_case(b"let") {
            self.deprecated("Calling a constant \"let\" is deprecated since 8.6")?;
        } else if short.eq_ignore_ascii_case(b"is") {
            self.deprecated("Calling a constant \"is\" is deprecated since 8.6")?;
        }
        if self.constants.contains_key(&name) {
            self.warning(&format!(
                "Constant {} already defined, this will be an error in PHP 9",
                name.to_string_lossy()
            ))?;
            return Ok(false);
        }
        self.user_constants.push(name.clone());
        let (file, _) = self.current_location();
        self.const_files.insert(name.clone(), file);
        self.constants.insert(name, v);
        Ok(true)
    }

    /// Reads a global constant.
    pub(crate) fn fetch_constant(&mut self, name: u32, fallback: Option<u32>) -> VmResult<Value> {
        let n = self.lit_str(name);
        if let Some(v) = self.constants.get(&n).cloned() {
            self.constant_deprecation(&n)?;
            return Ok(v);
        }
        if let Some(v) = self.special_constant(n.as_bytes()) {
            return Ok(v);
        }
        if let Some(fb) = fallback {
            let f = self.lit_str(fb);
            if let Some(v) = self.constants.get(&f).cloned() {
                self.constant_deprecation(&f)?;
                return Ok(v);
            }
            if let Some(v) = self.special_constant(f.as_bytes()) {
                return Ok(v);
            }
        }
        // Namespaced constant names are case-insensitive in the namespace part.
        if let Some(p) = n.as_bytes().iter().rposition(|&c| c == b'\\') {
            let (ns, c) = n.as_bytes().split_at(p);
            let mut k = ns.to_ascii_lowercase();
            k.extend_from_slice(c);
            if let Some(v) = self.constants.get(&ZStr::from(k)) {
                return Ok(v.clone());
            }
        }
        Err(self.make_throw(
            "Error",
            &format!("Undefined constant \"{}\"", n.to_string_lossy()),
        ))
    }

    fn special_constant(&mut self, name: &[u8]) -> Option<Value> {
        match name {
            b"__COMPILER_HALT_OFFSET__" => {
                let file = self.frame().code.filename.clone();
                self.ext.halt_offsets.get(&file).map(|o| Value::Int(*o))
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------------
    // Static properties
    // ------------------------------------------------------------------

    /// The slow path of [`Vm::static_prop_cell`] when `cls` has no static
    /// `$name`: Zend checks visibility even when the declaration is not
    /// static, and `properties_info` also carries an ancestor's private
    /// statics, so `B::$x` naming `A`'s private `$x` is a visibility error
    /// (or `A`'s own slot when accessed from `A`).
    #[cold]
    #[inline(never)]
    fn static_prop_miss(&mut self, cls: &Rc<Class>, name: &ZStr) -> VmResult<StaticProp> {
        if let Some(pi) = cls.prop_map.get(name) {
            let scope = self.scope();
            let mut decl = self.find_class(pi.class.as_bytes());
            if pi.flags & flags::PROTECTED != 0 {
                while let Some(parent) = decl.as_ref().and_then(|d| d.parent.clone()) {
                    if parent.prop_map.get(name).is_some_and(|p| {
                        p.visibility() != flags::PRIVATE && p.flags & flags::STATIC == 0
                    }) {
                        decl = Some(parent);
                    } else {
                        break;
                    }
                }
            }
            let accessible = match (pi.flags & flags::VISIBILITY, &scope, &decl) {
                (flags::PUBLIC, _, _) => true,
                (flags::PRIVATE, Some(s), Some(d)) => Rc::ptr_eq(s, d),
                (flags::PROTECTED, Some(s), Some(d)) => s.is_subclass_of(d) || d.is_subclass_of(s),
                _ => false,
            };
            if !accessible {
                let vis = if pi.flags & flags::PRIVATE != 0 {
                    "private"
                } else {
                    "protected"
                };
                return Err(self.make_throw(
                    "Error",
                    &format!(
                        "Cannot access {vis} property {}::${}",
                        object::display_name(&cls.name),
                        name.to_string_lossy()
                    ),
                ));
            }
        }
        inherited_private_static(cls, name.as_bytes()).ok_or_else(|| {
            self.make_throw(
                "Error",
                &format!(
                    "Access to undeclared static property {}::${}",
                    object::display_name(&cls.name),
                    name.to_string_lossy()
                ),
            )
        })
    }

    /// The storage slot of `A::$name` for `isset()`/`empty()`
    /// (`zend_std_get_static_property_with_info` with `BP_VAR_IS`): an
    /// undeclared or inaccessible static property yields no slot instead
    /// of an error. One lookup for the common (public, declared) case.
    pub(crate) fn static_prop_cell_is(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
    ) -> VmResult<Option<Rc<RefCell<Value>>>> {
        if let Some(sp) = cls.find_static(name.as_bytes()) {
            if sp.flags & flags::VISIBILITY == flags::PUBLIC {
                return self.static_prop_slot(cls, name, sp).map(Some);
            }
        }
        self.static_prop_cell_is_slow(cls, name)
    }

    #[cold]
    #[inline(never)]
    fn static_prop_cell_is_slow(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
    ) -> VmResult<Option<Rc<RefCell<Value>>>> {
        let Some(sp) = cls
            .find_static(name.as_bytes())
            .cloned()
            .or_else(|| inherited_private_static(cls, name.as_bytes()))
        else {
            return Ok(None);
        };
        if sp.flags & flags::VISIBILITY != flags::PUBLIC {
            let scope = self.scope();
            let decl = self.find_class(sp.class.as_bytes());
            let ok = match (sp.flags & flags::VISIBILITY, &scope, &decl) {
                (flags::PRIVATE, Some(s), Some(d)) => Rc::ptr_eq(s, d),
                (flags::PROTECTED, Some(s), Some(d)) => s.is_subclass_of(d) || d.is_subclass_of(s),
                _ => false,
            };
            if !ok {
                return Ok(None);
            }
        }
        self.static_prop_slot(cls, name, &sp).map(Some)
    }

    /// The storage slot of `A::$name`.
    #[inline]
    pub(crate) fn static_prop_cell(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
    ) -> VmResult<Rc<RefCell<Value>>> {
        self.static_prop_cell_flags(cls, name).map(|(cell, _)| cell)
    }

    /// [`Vm::static_prop_cell`] with the property's flags (for the set
    /// visibility check of a write).
    pub(crate) fn static_prop_cell_flags(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
    ) -> VmResult<(Rc<RefCell<Value>>, u32)> {
        // One lookup on the hot path, borrowing from `cls` (owned by the
        // caller, not the VM); misses (instance-property visibility errors,
        // ancestors' private statics, undeclared) are cold.
        let missed;
        let sp = if let Some(sp) = cls.find_static(name.as_bytes()) {
            sp
        } else {
            missed = self.static_prop_miss(cls, name)?;
            &missed
        };
        if sp.flags & flags::VISIBILITY != flags::PUBLIC {
            let scope = self.scope();
            let decl = self.find_class(sp.class.as_bytes());
            let ok = match (sp.flags & flags::VISIBILITY, &scope, &decl) {
                (flags::PRIVATE, Some(s), Some(d)) => Rc::ptr_eq(s, d),
                (flags::PROTECTED, Some(s), Some(d)) => s.is_subclass_of(d) || d.is_subclass_of(s),
                _ => false,
            };
            if !ok {
                let vis = if sp.flags & flags::PRIVATE != 0 {
                    "private"
                } else {
                    "protected"
                };
                return Err(self.make_throw(
                    "Error",
                    &format!(
                        "Cannot access {vis} property {}::${}",
                        object::display_name(&cls.name),
                        name.to_string_lossy()
                    ),
                ));
            }
        }
        let fl = sp.flags;
        self.static_prop_slot(cls, name, sp).map(|cell| (cell, fl))
    }

    /// The tail of a static property fetch once `sp` is known accessible:
    /// the trait deprecation, lazy default evaluation and the slot.
    #[inline]
    fn static_prop_slot(
        &mut self,
        cls: &Rc<Class>,
        name: &ZStr,
        sp: &StaticProp,
    ) -> VmResult<Rc<RefCell<Value>>> {
        if cls.is_trait() {
            self.error(
                crate::errors::level::DEPRECATED,
                &format!(
                    "Accessing static trait property {}::${} is deprecated, it should only be accessed on a class using the trait",
                    cls.name,
                    name.to_string_lossy()
                ),
            )?;
        }
        if !sp.initialized.get() {
            sp.initialized.set(true);
            if let Some(d) = &sp.default {
                let decl = self
                    .find_class(sp.class.as_bytes())
                    .unwrap_or_else(|| cls.clone());
                let v = self.eval_const_expr(d, Some(&decl))?;
                *sp.cell.borrow_mut() = v;
            }
        }
        crate::typed_refs::note_static_prop(sp);
        Ok(sp.cell.clone())
    }

    // ------------------------------------------------------------------
    // Functions
    // ------------------------------------------------------------------

    /// Declares a user function.
    pub(crate) fn declare_function(&mut self, f: &Rc<UserFunction>) -> VmResult<()> {
        let lc = f.name.to_ascii_lowercase();
        if let Some(existing) = self.functions.get(&lc) {
            // `preload_remove_declares`: a preloaded declaration is gone.
            if self.is_preloaded_function(f) {
                return Ok(());
            }
            let msg = match existing {
                Func::User(u) => format!(
                    "Cannot redeclare function {}() (previously declared in {}:{})",
                    f.name, u.code.filename, u.line_start
                ),
                Func::Builtin(_) => format!("Cannot redeclare function {}()", f.name),
            };
            return Err(self.fatal_at(&msg, f.line_start));
        }
        self.observer_declared(&lc, false);
        crate::builtins::info::note_user_function(&lc);
        self.functions.insert(lc, Func::User(f.clone()));
        Ok(())
    }

    // ------------------------------------------------------------------
    // Class declaration
    // ------------------------------------------------------------------

    /// Declares (links) a class from its compiled declaration.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn declare_class(
        &mut self,
        decl: &Rc<ClassDecl>,
        is_anon: bool,
    ) -> VmResult<Rc<Class>> {
        let key = Rc::as_ptr(decl) as usize;
        if let Some(c) = self.bound_decls.get(&key) {
            return Ok(c.clone());
        }
        let lc = decl.name.to_ascii_lowercase();
        if let Some(old) = self.classes.get(&lc) {
            // `zend_class_redeclaration_error(E_COMPILE_ERROR, ...)`: names
            // the class already declared under that name (its kind and
            // spelling).
            let msg = class_redeclaration_message(&old.name, old);
            let (file, _) = self.current_location();
            self.report_compile_error(&msg, &file, decl.line_start, false);
            return Err(Throw::Fatal);
        }
        let (cls, recorded) = self.link_class(decl, is_anon)?;
        self.bound_decls.insert(key, cls.clone());
        // (Already declared if it waited for variance dependencies.)
        if !self.classes.get(&lc).is_some_and(|c| Rc::ptr_eq(c, &cls)) {
            self.register_class(cls.clone());
        }
        self.observer_declared(&lc, true);
        // `zend_emit_recorded_errors`: warnings raised while linking are
        // emitted once the class is declared (an exception thrown by an
        // error handler then leaves the class declared).
        for (level, msg, l) in recorded {
            self.error_at(level, &msg, &decl.filename, l)?;
        }
        Ok(cls)
    }

    /// `ZEND_DECLARE_CLASS`: binds a class declaration statement. An
    /// early-bound (hoisted) declaration is consumed by its first
    /// execution; executing the statement again (a declaration inside a
    /// function called twice, or in a loop) redeclares the class, as the
    /// runtime-definition key is gone (`do_bind_class`, `Zend/zend_compile.c`).
    pub(crate) fn declare_class_stmt(&mut self, decl: &Rc<ClassDecl>) -> VmResult<()> {
        let key = Rc::as_ptr(decl) as usize;
        if self.bound_decls.remove(&key).is_none() {
            // `preload_remove_declares`: a preloaded declaration is gone.
            if self.is_preloaded_class(decl) {
                return Ok(());
            }
            self.declare_class(decl, false)?;
            self.bound_decls.remove(&key);
        }
        Ok(())
    }

    /// Registers a linked class.
    pub(crate) fn register_class(&mut self, cls: Rc<Class>) {
        let lc = cls.lc_name.clone();
        self.class_order.push(lc.clone());
        if self.classes.insert(lc.clone(), cls).is_none() {
            crate::builtins::info::note_new_class(&lc);
        }
    }

    /// Whether all dependencies of a declaration are available (for early
    /// binding).
    pub(crate) fn can_early_bind(&self, decl: &ClassDecl) -> bool {
        let has = |n: &ZStr| self.find_class(n.as_bytes()).is_some();
        decl.parent.as_ref().is_none_or(has)
            && decl.interfaces.iter().all(has)
            && decl.traits.iter().all(has)
            && !self.has_unresolved_variance(decl)
    }

    fn has_unresolved_constant_variance(&self, decl: &ClassDecl, own: &[u8]) -> bool {
        for constant in &decl.consts {
            let overridden = decl
                .parent
                .iter()
                .chain(decl.interfaces.iter())
                .any(|base| {
                    self.find_class(base.as_bytes()).is_some_and(|class| {
                        class
                            .find_const(constant.name.as_bytes())
                            .is_some_and(|proto| proto.ty.is_some())
                    })
                });
            if overridden {
                if let Some(ty) = &constant.ty {
                    if ty
                        .classes
                        .iter()
                        .chain(ty.intersections.iter().flatten())
                        .any(|name| {
                            !matches!(name.as_bytes(), b"self" | b"parent" | b"static")
                                && !name.eq_ignore_ascii_case(own)
                                && self.find_class(name.as_bytes()).is_none()
                        })
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// `zend_try_early_bind` gives up when a method signature check against
    /// the parent is `INHERITANCE_UNRESOLVED`: approximated as an overriding
    /// method whose signature differs from the parent's in a class name
    /// that is not declared yet. The class is then declared when its
    /// statement runs, where the check can report the error.
    fn has_unresolved_variance(&self, decl: &ClassDecl) -> bool {
        fn names(f: &UserFunction, out: &mut Vec<Vec<u8>>) {
            sig_names(&f.params, f.return_type.as_ref(), out);
        }
        fn sig_names(
            params: &[crate::opcode::ParamInfo],
            ret: Option<&crate::types::TypeDecl>,
            out: &mut Vec<Vec<u8>>,
        ) {
            let types = params.iter().filter_map(|p| p.ty.as_ref()).chain(ret);
            for t in types {
                for n in t.classes.iter().chain(t.intersections.iter().flatten()) {
                    out.push(n.as_bytes().to_ascii_lowercase());
                }
            }
        }
        let own = decl.name.as_bytes().to_ascii_lowercase();
        if self.has_unresolved_constant_variance(decl, &own) {
            return true;
        }
        for prop in &decl.props {
            if let Some(setter) = prop.hooks.as_ref().and_then(|hooks| hooks.set.as_ref()) {
                let mut required = Vec::new();
                names(setter, &mut required);
                if let Some(ty) = &prop.ty {
                    required.extend(
                        ty.classes
                            .iter()
                            .chain(ty.intersections.iter().flatten())
                            .map(|name| name.as_bytes().to_ascii_lowercase()),
                    );
                }
                if required.iter().any(|name| {
                    !matches!(name.as_slice(), b"self" | b"parent" | b"static")
                        && *name != own
                        && self.find_class(name).is_none()
                }) {
                    return true;
                }
            }
        }
        let Some(parent) = decl
            .parent
            .as_ref()
            .and_then(|p| self.find_class(p.as_bytes()))
        else {
            return false;
        };
        for prop in &decl.props {
            let Some(proto) = parent
                .prop_map
                .get(&prop.name)
                .filter(|p| p.visibility() != flags::PRIVATE)
            else {
                continue;
            };
            let required = prop
                .ty
                .iter()
                .chain(proto.ty.iter())
                .flat_map(|ty| ty.classes.iter().chain(ty.intersections.iter().flatten()));
            if required
                .filter(|name| {
                    !matches!(name.as_bytes(), b"self" | b"parent" | b"static")
                        && !name.eq_ignore_ascii_case(&own)
                })
                .any(|name| self.find_class(name.as_bytes()).is_none())
            {
                return true;
            }
        }
        decl.methods.iter().any(|md| {
            let Some(pm) = parent.find_method(&md.name.as_bytes().to_ascii_lowercase()) else {
                return false;
            };
            if pm.visibility() == flags::PRIVATE {
                return false;
            }
            // A non-abstract parent constructor is not signature-checked.
            if md.name.eq_ignore_ascii_case(b"__construct") && !pm.is_abstract() {
                return false;
            }
            let (mut a, mut b) = (Vec::new(), Vec::new());
            names(&md.func, &mut a);
            // An internal parent's arginfo types count too (GH-13680:
            // `DateTime::createFromFormat()` overridden with an undeclared
            // return class).
            match &pm.func {
                Func::User(pf) => names(pf, &mut b),
                Func::Builtin(pb) => sig_names(&pb.params, pb.return_type.as_ref(), &mut b),
            }
            a.iter()
                .filter(|n| !b.contains(n))
                .chain(b.iter().filter(|n| !a.contains(n)))
                .any(|n| {
                    !matches!(n.as_slice(), b"self" | b"parent" | b"static")
                        && *n != own.as_slice()
                        && self.find_class(n).is_none()
                })
        })
    }

    /// `zend_traits_init_trait_structures`: resolves the traits named by
    /// `insteadof` and `as` rules (without autoloading) and validates them.
    #[allow(clippy::too_many_lines)]
    fn init_trait_structures(
        &self,
        decl: &ClassDecl,
        traits: &[Rc<Class>],
    ) -> Result<TraitRules, String> {
        let find = |n: &ZStr| -> Result<usize, String> {
            let Some(t) = self.find_class(n.as_bytes()).filter(|c| c.linked.get()) else {
                return Err(format!("Could not find trait {n}"));
            };
            // `zend_check_trait_usage`.
            if !t.is_trait() {
                return Err(format!(
                    "Class {} is not a trait, Only traits may be used in 'as' and 'insteadof' statements",
                    t.name
                ));
            }
            traits
                .iter()
                .position(|x| Rc::ptr_eq(x, &t))
                .ok_or_else(|| format!("Required Trait {} wasn't added to {}", t.name, decl.name))
        };
        let mut out = TraitRules {
            excluded: Vec::new(),
            alias_trait: vec![None; decl.trait_rules.len()],
        };
        for r in &decl.trait_rules {
            let TraitRule::Precedence {
                trait_name,
                method,
                insteadof,
            } = r
            else {
                continue;
            };
            let ti = find(trait_name)?;
            let lc = method.to_ascii_lowercase();
            if !traits[ti].methods.contains_key(&lc) {
                return Err(format!(
                    "A precedence rule was defined for {}::{method} but this method does not exist",
                    traits[ti].name
                ));
            }
            for ex in insteadof {
                let ei = find(ex)?;
                if out.excluded.contains(&(ei, lc.clone())) {
                    return Err(format!(
                        "Failed to evaluate a trait precedence ({method}). Method of trait {} was defined to be excluded multiple times",
                        traits[ei].name
                    ));
                }
                out.excluded.push((ei, lc.clone()));
                if ei == ti {
                    let t = &traits[ti].name;
                    return Err(format!(
                        "Inconsistent insteadof definition. The method {method} is to be used from {t}, but {t} is also on the exclude list"
                    ));
                }
            }
        }
        for (i, r) in decl.trait_rules.iter().enumerate() {
            let TraitRule::Alias {
                trait_name,
                method,
                alias,
                ..
            } = r
            else {
                continue;
            };
            let lc = method.to_ascii_lowercase();
            if let Some(tn) = trait_name {
                let ti = find(tn)?;
                if !traits[ti].methods.contains_key(&lc) {
                    return Err(format!(
                        "An alias was defined for {}::{method} but this method does not exist",
                        traits[ti].name
                    ));
                }
                out.alias_trait[i] = Some(ti);
                continue;
            }
            let mut found: Option<usize> = None;
            for (j, t) in traits.iter().enumerate() {
                if !t.methods.contains_key(&lc) {
                    continue;
                }
                if let Some(f) = found {
                    let (a, b) = (&traits[f].name, &t.name);
                    return Err(format!(
                        "An alias was defined for method {method}(), which exists in both {a} and {b}. Use {a}::{method} or {b}::{method} to resolve the ambiguity"
                    ));
                }
                found = Some(j);
            }
            if found.is_none() {
                return Err(match alias {
                    Some(a) => format!(
                        "An alias ({a}) was defined for method {method}(), but this method does not exist"
                    ),
                    None => format!(
                        "The modifiers of the trait method {method}() are changed, but this method does not exist. Error"
                    ),
                });
            }
            out.alias_trait[i] = found;
        }
        Ok(out)
    }

    /// `do_inheritance_check_on_method` (with `ZEND_INHERITANCE_CHECK_PROTO`
    /// but no visibility check) of the method satisfying an abstract trait
    /// method: the final/static errors (message, file, line), else the
    /// signature check to perform.
    fn check_trait_abstract(
        cls: &Rc<Class>,
        c: &TraitAbstractCheck,
    ) -> Result<crate::variance::Obligation, (String, ZStr, u32)> {
        let (child, proto) = (&c.child, &c.proto);
        // `fixup_trait_scope`: `self` in trait methods is the using class.
        let child_scope =
            if c.child_trait.is_some() || Weak::ptr_eq(&child.scope, &Rc::downgrade(cls)) {
                cls.clone()
            } else {
                child.scope()
            };
        let child_name = c
            .child_trait
            .clone()
            .unwrap_or_else(|| object::display_name(&child_scope.name));
        let proto_name = object::display_name(&proto.scope().name);
        let (file, line) = match &child.func {
            Func::User(u) => (u.code.filename.clone(), u.line_start),
            Func::Builtin(_) => (ZStr::from("Unknown"), 0),
        };
        if proto.flags & flags::FINAL != 0 {
            return Err((
                format!(
                    "Cannot override final method {proto_name}::{}()",
                    child.name
                ),
                file,
                line,
            ));
        }
        if child.is_static() != proto.is_static() {
            let msg = if child.is_static() {
                format!(
                    "Cannot make non static method {proto_name}::{}() static in class {child_name}",
                    child.name
                )
            } else {
                format!(
                    "Cannot make static method {proto_name}::{}() non static in class {child_name}",
                    child.name
                )
            };
            return Err((msg, file, line));
        }
        Ok(crate::variance::Obligation {
            child: child.clone(),
            child_name,
            child_scope,
            proto: proto.clone(),
            proto_name,
            proto_scope: cls.clone(),
        })
    }

    /// `load_delayed_classes`: autoloads the classes that pending variance
    /// checks need (nested linking may take some of them first).
    fn load_delayed_classes(&mut self, ce_name: &ZStr) -> VmResult<()> {
        loop {
            let next = LINK_STATE.with(|st| {
                let mut st = st.borrow_mut();
                (!st.delayed.is_empty()).then(|| st.delayed.remove(0))
            });
            let Some(n) = next else {
                return Ok(());
            };
            if self.find_class(n.as_bytes()).is_none() && unlinked_decl(n.as_bytes()).is_none() {
                match self.lookup_class(&n) {
                    Ok(_) => {}
                    Err(Throw::Exception(e)) => {
                        // `zend_exception_uncaught_error`: the class cannot
                        // be unlinked any more.
                        let message = self.to_zstr(&Value::Object(e))?;
                        return Err(self.fatal(&format!(
                            "During inheritance of {ce_name}, while autoloading {n}: Uncaught {message}"
                        )));
                    }
                    Err(t) => return Err(t),
                }
            }
        }
    }

    /// `resolve_delayed_variance_obligations`: the dependencies are
    /// resolved first, then the pending checks must succeed now.
    /// Deprecations go to `recorded` (or are raised right away).
    fn resolve_pending(
        &mut self,
        cls: &Rc<Class>,
        recorded: Option<&RecordedErrors>,
    ) -> VmResult<()> {
        let Some(p) = LINK_STATE.with(|st| {
            let mut st = st.borrow_mut();
            let i = st.pending.iter().position(|p| Rc::ptr_eq(&p.cls, cls))?;
            Some(st.pending.remove(i))
        }) else {
            return Ok(());
        };
        for d in &p.deps {
            if is_pending(d) {
                self.resolve_pending(d, None)?;
            }
        }
        for ob in &p.obligations {
            let mut unresolved = Vec::new();
            let st = ob.check(self, cls, &mut unresolved);
            match ob.message(st, unresolved.first()) {
                None => {}
                Some((m, file, l)) if st == crate::variance::Status::Warning => match recorded {
                    Some(r) => r.push((crate::errors::level::DEPRECATED, m, l)),
                    None => self.error_at(crate::errors::level::DEPRECATED, &m, &file, l)?,
                },
                Some((m, file, l)) => {
                    self.report_fatal(&m, &file, l);
                    return Err(Throw::Fatal);
                }
            }
        }
        for ob in &p.constants {
            let mut unresolved = Vec::new();
            if ob.check(self, cls, &mut unresolved) != crate::variance::Status::Ok {
                let (message, file, line) = ob.message();
                self.report_fatal(&message, &file, line);
                return Err(Throw::Fatal);
            }
        }
        for ob in &p.properties {
            let mut unresolved = Vec::new();
            if ob.check(self, cls, &mut unresolved) != crate::variance::Status::Ok {
                let (message, file, line) = ob.message();
                self.report_fatal(&message, &file, line);
                return Err(Throw::Fatal);
            }
        }
        Ok(())
    }

    /// A link-time fatal at `line` of the class being linked: like php-src
    /// (`CG(compiled_filename)` / the class's `info.user.filename`), the file
    /// is the declaring file, not the running frame's, which differs when
    /// the class is bound while its includer is still executing (a relative
    /// `require_once`).
    fn fatal_at(&mut self, msg: &str, line: u32) -> Throw {
        let linking = LINK_STATE.with(|st| st.borrow().linking.last().map(|d| d.filename.clone()));
        let file = match linking {
            Some(f) if !f.is_empty() => f,
            _ => self.current_location().0,
        };
        self.report_fatal(msg, &file, line);
        Throw::Fatal
    }

    /// A hierarchy dependency cannot fail gracefully once variance used the unlinked class.
    fn lookup_class_for_inheritance(
        &mut self,
        name: &ZStr,
        decl: &Rc<ClassDecl>,
    ) -> VmResult<Option<Rc<Class>>> {
        match self.lookup_class(name) {
            Err(Throw::Exception(e))
                if LINK_STATE.with(|st| {
                    st.borrow()
                        .unlinked_uses
                        .iter()
                        .any(|d| Rc::ptr_eq(d, decl))
                }) =>
            {
                let message = self.to_zstr(&Value::Object(e))?;
                Err(self.fatal(&format!(
                    "During inheritance of {} with variance dependencies: Uncaught {message}",
                    decl.name
                )))
            }
            result => result,
        }
    }

    #[allow(clippy::too_many_lines)]
    /// Links a class; also returns the warnings recorded while linking
    /// (level, message, line), to be emitted after the class is declared.
    fn link_class(
        &mut self,
        decl: &Rc<ClassDecl>,
        is_anon: bool,
    ) -> VmResult<(Rc<Class>, Recorded)> {
        // While it is linked, the class is visible (unlinked) to the
        // variance checks of the classes its dependencies declare.
        let guard = LinkingGuard::enter(decl);
        // Nearly linked parent / interfaces (`add_dependency_obligation`).
        let mut deps: Vec<Rc<Class>> = Vec::new();
        let name = decl.name.clone();
        let display = object::display_name(&name);
        // Messages formatted with `%pS` print the whole name, including the
        // NUL-separated suffix of an anonymous class.
        let full = name.to_string_lossy();
        let line = decl.line_start;
        // Parent.
        let parent = match &decl.parent {
            Some(p) => {
                let Some(pc) = self.lookup_class_for_inheritance(p, decl)? else {
                    let kind = if decl.kind == ClassKind::Interface {
                        "Interface"
                    } else {
                        "Class"
                    };
                    return Err(self.make_throw(
                        "Error",
                        &format!("{kind} \"{}\" not found", p.to_string_lossy()),
                    ));
                };
                if pc.is_interface() && decl.kind == ClassKind::Class {
                    return Err(self.fatal_at(
                        &format!("Class {full} cannot extend interface {}", pc.name),
                        line,
                    ));
                }
                if pc.is_trait() {
                    return Err(self.fatal_at(
                        &format!("Class {full} cannot extend trait {}", pc.name),
                        line,
                    ));
                }
                if pc.flags & cflags::FINAL != 0 {
                    let what = if pc.is_enum() { "enum" } else { "final class" };
                    return Err(self.fatal_at(
                        &format!("Class {full} cannot extend {what} {}", pc.name),
                        line,
                    ));
                }
                if pc.flags & cflags::READONLY != decl.flags & cflags::READONLY {
                    let (a, b) = if decl.flags & cflags::READONLY != 0 {
                        ("Readonly class", "non-readonly class")
                    } else {
                        ("Non-readonly class", "readonly class")
                    };
                    return Err(
                        self.fatal_at(&format!("{a} {full} cannot extend {b} {}", pc.name), line)
                    );
                }
                if is_pending(&pc) {
                    deps.push(pc.clone());
                }
                Some(pc)
            }
            None => None,
        };
        // Interfaces (for interfaces: parents).
        let mut interfaces: Vec<Rc<Class>> = Vec::new();
        let mut direct_ifaces: Vec<Rc<Class>> = Vec::new();
        if let Some(p) = &parent {
            for i in &p.interfaces {
                interfaces.push(i.clone());
            }
        }
        let mut iface_names = decl.interfaces.clone();
        if decl
            .methods
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(b"__tostring"))
            && !iface_names
                .iter()
                .any(|i| i.eq_ignore_ascii_case(b"Stringable"))
            && decl.kind != ClassKind::Trait
        {
            iface_names.push(ZStr::from("Stringable"));
        }
        if decl.kind == ClassKind::Enum {
            iface_names.insert(0, ZStr::from("UnitEnum"));
            if decl.backing.is_some() {
                iface_names.insert(1, ZStr::from("BackedEnum"));
            }
        }
        for iname in &iface_names {
            let Some(ic) = self.lookup_class_for_inheritance(iname, decl)? else {
                return Err(self.make_throw(
                    "Error",
                    &format!("Interface \"{}\" not found", iname.to_string_lossy()),
                ));
            };
            if !ic.is_interface() {
                if decl.kind == ClassKind::Interface {
                    return Err(self.fatal_at(
                        &format!(
                            "{display} cannot implement {} - it is not an interface",
                            ic.name
                        ),
                        line,
                    ));
                }
                return Err(self.fatal_at(
                    &format!(
                        "{display} cannot implement {} - it is not an interface",
                        ic.name
                    ),
                    line,
                ));
            }
            if decl.kind == ClassKind::Class
                && (ic.lc_name.as_bytes() == b"unitenum" || ic.lc_name.as_bytes() == b"backedenum")
            {
                // `do_interface_implementation` implements the parent
                // interfaces (UnitEnum of BackedEnum) before the interface.
                let first = ic
                    .interfaces
                    .iter()
                    .find(|i| i.lc_name.as_bytes() == b"unitenum")
                    .unwrap_or(&ic);
                return Err(self.fatal_at(
                    &format!(
                        "Non-enum class {display} cannot implement interface {}",
                        first.name
                    ),
                    line,
                ));
            }
            if direct_ifaces.iter().any(|x| Rc::ptr_eq(x, &ic)) {
                // `zend_do_implement_interfaces`: the same interface named
                // twice (e.g. through a `class_alias()`).
                return Err(dup_interface_error(self, decl.kind, &full, &ic.name, line));
            }
            if is_pending(&ic) {
                deps.push(ic.clone());
            }
            direct_ifaces.push(ic.clone());
            for sub in &ic.interfaces {
                if !interfaces.iter().any(|x| Rc::ptr_eq(x, sub)) {
                    interfaces.push(sub.clone());
                }
            }
            if !interfaces.iter().any(|x| Rc::ptr_eq(x, &ic)) {
                interfaces.push(ic.clone());
            }
        }
        // Traversable must be implemented via Iterator or IteratorAggregate.
        // (`zend_implement_traversable`: an abstract class may implement
        // Traversable alone; the error is an E_CORE_ERROR without location.)
        if matches!(decl.kind, ClassKind::Class | ClassKind::Enum)
            && decl.flags & cflags::EXPLICIT_ABSTRACT == 0
            && interfaces
                .iter()
                .any(|i| i.lc_name.as_bytes() == b"traversable")
            && !interfaces
                .iter()
                .any(|i| matches!(i.lc_name.as_bytes(), b"iterator" | b"iteratoraggregate"))
            && parent
                .as_ref()
                .is_none_or(|p| p.flags & cflags::INTERNAL == 0)
        {
            // `zend_get_object_type_uc`.
            let what = if decl.kind == ClassKind::Enum {
                "Enum"
            } else {
                "Class"
            };
            self.report_fatal(
                &format!(
                    "{what} {display} must implement interface Traversable as part of either Iterator or IteratorAggregate"
                ),
                &ZStr::from("Unknown"),
                0,
            );
            return Err(Throw::Fatal);
        }
        // `zend_implement_aggregate` / `zend_implement_iterator`: a class
        // (not an interface) may not be both an Iterator and an
        // IteratorAggregate, whichever one it implements first.
        if decl.kind != ClassKind::Interface
            && interfaces
                .iter()
                .any(|i| i.lc_name.as_bytes() == b"iterator")
            && interfaces
                .iter()
                .any(|i| i.lc_name.as_bytes() == b"iteratoraggregate")
        {
            return Err(self.fatal_at(
                &format!(
                    "Class {full} cannot implement both Iterator and IteratorAggregate at the same time"
                ),
                line,
            ));
        }
        // `zend_implement_backed_enum`: BackedEnum (named directly or
        // through another interface) needs a backed enum.
        if decl.kind == ClassKind::Enum && decl.backing.is_none() {
            if let Some(ic) = interfaces
                .iter()
                .find(|i| i.lc_name.as_bytes() == b"backedenum")
            {
                return Err(self.fatal_at(
                    &format!(
                        "Non-backed enum {display} cannot implement interface {}",
                        ic.name
                    ),
                    line,
                ));
            }
        }
        // An enum-only interface reached through another interface
        // (`zend_implement_unit_enum` runs for every implemented interface).
        if decl.kind == ClassKind::Class {
            if let Some(ic) = interfaces
                .iter()
                .find(|i| matches!(i.lc_name.as_bytes(), b"unitenum" | b"backedenum"))
            {
                return Err(self.fatal_at(
                    &format!(
                        "Non-enum class {display} cannot implement interface {}",
                        ic.name
                    ),
                    line,
                ));
            }
        }
        // Traits.
        let mut traits = Vec::new();
        for tn in &decl.traits {
            let Some(tc) = self.lookup_class(tn)? else {
                return Err(self.make_throw(
                    "Error",
                    &format!("Trait \"{}\" not found", tn.to_string_lossy()),
                ));
            };
            if !tc.is_trait() {
                return Err(self.make_throw(
                    "Error",
                    &format!("{display} cannot use {} - it is not a trait", tc.name),
                ));
            }
            if tc
                .attributes
                .iter()
                .any(|a| a.name.eq_ignore_ascii_case(b"Deprecated"))
            {
                // `zend_use_of_deprecated_trait`.
                let suffix = self.deprecation_suffix(&tc.attributes, Some(&tc))?;
                let code = if tc.flags & cflags::INTERNAL != 0 {
                    crate::errors::level::DEPRECATED
                } else {
                    crate::errors::level::USER_DEPRECATED
                };
                let mut msg =
                    format!("Trait {} used by {display} is deprecated", tc.name).into_bytes();
                msg.extend_from_slice(&suffix);
                self.error_bytes(code, &msg)?;
            }
            // `zend_do_link_class` skips duplicated trait uses.
            if !traits.iter().any(|t: &Rc<Class>| Rc::ptr_eq(t, &tc)) {
                traits.push(tc);
            }
        }
        // `zend_do_bind_traits`: a `__toString()` imported from a trait
        // implements Stringable as well, appended after the other
        // interfaces (bug #81582).
        if decl.kind == ClassKind::Class
            && !interfaces
                .iter()
                .any(|i| i.lc_name.as_bytes() == b"stringable")
        {
            let lc = ZStr::from("__tostring");
            if traits.iter().any(|t| t.methods.contains_key(&lc)) {
                if let Some(sc) = self.find_class(b"stringable") {
                    interfaces.push(sc);
                }
            }
        }
        let trait_rules = if traits.is_empty() {
            TraitRules::default()
        } else {
            match self.init_trait_structures(decl, &traits) {
                Ok(r) => r,
                Err(e) => return Err(self.fatal_at(&e, line)),
            }
        };
        let mut cflag = decl.flags;
        if let Some(p) = &parent {
            if p.flags & cflags::ALLOW_DYNAMIC != 0 {
                cflag |= cflags::ALLOW_DYNAMIC;
            }
        }
        let decl_c = decl.clone();
        let parent_c = parent.clone();
        let traits_c = traits.clone();
        let mut err: Option<String> = None;
        let mut link_out = LinkOut::default();
        let interfaces_c = interfaces.clone();
        let cls = Rc::new_cyclic(|weak: &Weak<Class>| {
            build_class(
                &decl_c,
                weak,
                parent_c,
                interfaces_c,
                traits_c,
                &trait_rules,
                cflag,
                is_anon,
                &mut err,
                &mut link_out,
            )
        });
        if let Some(e) = err {
            return Err(self.fatal_at(&e, line));
        }
        let recorded = RecordedErrors::begin(
            decl.filename.clone(),
            link_out
                .warnings
                .iter()
                .map(|w| (crate::errors::level::COMPILE_WARNING, w.clone(), line))
                .collect(),
        );
        for (a, b, msg) in &link_out.value_checks {
            // A failed evaluation makes the values incompatible.
            let mut eval = |e: &ConstExpr| match self.eval_const_expr(e, Some(&cls)) {
                Ok(v) => Ok(Some(v)),
                Err(Throw::Exception(_)) => Ok(None),
                Err(t) => Err(t),
            };
            let identical = match (eval(a)?, eval(b)?) {
                (Some(x), Some(y)) => php_runtime::ops::is_identical(&x, &y).unwrap_or(false),
                _ => false,
            };
            if !identical {
                return Err(self.fatal_at(msg, line));
            }
        }
        // Inheritance checks for overridden methods.
        if let Some((e, l)) = check_method_inheritance(&cls) {
            return Err(self.fatal_at(&e, l));
        }
        let mut cc = crate::variance::ClassCheck::default();
        for c in &link_out.trait_checks {
            match Self::check_trait_abstract(&cls, c) {
                Ok(ob) => {
                    if cc.record(self, &cls, ob) {
                        break;
                    }
                }
                Err((e, file, l)) => {
                    self.report_fatal(&e, &file, l);
                    return Err(Throw::Fatal);
                }
            }
        }
        if cc.error.is_none() {
            crate::variance::check_class(self, &cls, &mut cc);
        }
        // Deprecations of the checks before a failing one are emitted
        // ahead of its fatal error.
        for (w, l) in std::mem::take(&mut cc.warnings) {
            recorded.push((crate::errors::level::DEPRECATED, w, l));
        }
        if let Some((e, file, l)) = cc.error.take() {
            self.report_fatal(&e, &file, l);
            return Err(Throw::Fatal);
        }
        // `do_implement_interface` runs the interface hooks while binding
        // the interfaces, before `zend_verify_abstract_class` (not for an
        // interface extending them).
        let ifaces = if cls.is_interface() {
            Vec::new()
        } else {
            cls.interfaces.clone()
        };
        for iface in ifaces {
            if let Some(f) = iface
                .hooks
                .as_ref()
                .and_then(|h| h.interface_gets_implemented)
            {
                f(self, &cls)?;
            }
        }
        // `zend_implement_serializable` (the Serializable hook, run while
        // binding the interfaces: before `zend_verify_abstract_class` and
        // `zend_verify_enum`).
        if cls.flags & cflags::EXPLICIT_ABSTRACT == 0
            && !cls.is_interface()
            && cls
                .interfaces
                .iter()
                .any(|i| i.lc_name.as_bytes() == b"serializable")
            && (cls.find_method(b"__serialize").is_none()
                || cls.find_method(b"__unserialize").is_none())
        {
            let msg = format!(
                "{} implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary)",
                object::display_name(&cls.name)
            );
            recorded.push((crate::errors::level::DEPRECATED, msg, line));
        }
        // Abstract method check.
        // (`zend_verify_abstract_class`: an explicitly abstract class must
        // still implement its private abstract methods.)
        let explicit_abstract = cls.flags & cflags::EXPLICIT_ABSTRACT != 0;
        if !cls.is_interface() && !cls.is_trait() {
            let mut missing: Vec<String> = Vec::new();
            for lcname in &cls.method_order {
                if let Some(m) = cls.methods.get(lcname) {
                    if m.is_abstract() && (!explicit_abstract || m.visibility() == flags::PRIVATE) {
                        missing.push(format!(
                            "{}::{}",
                            object::display_name(&m.scope().name),
                            m.name
                        ));
                    }
                }
            }
            for pi in &cls.props {
                if explicit_abstract
                    || !cls
                        .prop_map
                        .get(&pi.name)
                        .is_some_and(|x| Rc::ptr_eq(x, pi))
                {
                    continue;
                }
                if let Some(h) = &pi.hooks {
                    let declaring = |hook: &Option<Rc<crate::func::UserFunction>>| {
                        let name = hook
                            .as_ref()
                            .and_then(|g| g.class_name.as_ref())
                            .unwrap_or(&h.class);
                        if self
                            .find_class(name.as_bytes())
                            .is_some_and(|c| c.is_trait())
                        {
                            object::display_name(&pi.class)
                        } else {
                            object::display_name(name)
                        }
                    };
                    if h.abstract_get {
                        missing.push(format!("{}::${}::get", declaring(&h.get), pi.name));
                    }
                    if h.abstract_set {
                        missing.push(format!("{}::${}::set", declaring(&h.set), pi.name));
                    }
                }
            }
            if !explicit_abstract {
                for interface in &cls.interfaces {
                    for pi in &interface.props {
                        if let Some(own) = cls.prop_map.get(&pi.name) {
                            // `inherit_property_hook`: a backed readonly
                            // property does not implement an abstract `set`.
                            if pi.hooks.as_ref().is_some_and(|h| h.abstract_set)
                                && own.flags & flags::READONLY != 0
                                && !own.is_virtual()
                                && own.hooks.as_ref().is_none_or(|h| h.set.is_none())
                            {
                                missing.push(format!("{}::${}::set", interface.name, pi.name));
                            }
                            continue;
                        }
                        if let Some(h) = &pi.hooks {
                            if h.abstract_get {
                                missing.push(format!("{}::${}::get", interface.name, pi.name));
                            }
                            if h.abstract_set {
                                missing.push(format!("{}::${}::set", interface.name, pi.name));
                            }
                        }
                    }
                }
            }
            if !missing.is_empty() {
                let n = missing.len();
                let shown: Vec<String> = missing.iter().take(3).cloned().collect();
                let mut list = shown.join(", ");
                if n > 3 {
                    list.push_str(", ...");
                }
                let what = if cls.is_enum() { "Enum" } else { "Class" };
                let s = if n == 1 { "" } else { "s" };
                let msg = if explicit_abstract
                    || cls.is_enum()
                    || cls.flags & crate::class::cflags::ANON != 0
                {
                    format!("{what} {full} must implement {n} abstract method{s} ({list})")
                } else {
                    format!(
                        "{what} {full} contains {n} abstract method{s} and must therefore be declared abstract or implement the remaining method{s} ({list})"
                    )
                };
                return Err(self.fatal_at(&msg, line));
            }
        }
        if cls.is_enum() {
            if let Some(e) = self.verify_enum(&cls) {
                return Err(self.fatal_at(&e, line));
            }
        }
        if let Some((e, l)) = crate::attrs::check_override(&cls) {
            return Err(self.fatal_at(&e, if l == 0 { line } else { l }));
        }
        if !cc.pending.is_empty()
            || !cc.pending_props.is_empty()
            || !cc.pending_consts.is_empty()
            || !deps.is_empty()
        {
            // Nearly linked: declared, so that the classes the pending
            // checks need can be autoloaded and refer back to it.
            guard.declared();
            if !self
                .classes
                .get(&cls.lc_name)
                .is_some_and(|c| Rc::ptr_eq(c, &cls))
            {
                self.register_class(cls.clone());
            }
            LINK_STATE.with(|st| {
                let mut st = st.borrow_mut();
                for n in &cc.unresolved {
                    if !st
                        .delayed
                        .iter()
                        .any(|d| d.eq_ignore_ascii_case(n.as_bytes()))
                    {
                        st.delayed.push(n.clone());
                    }
                }
                st.pending.push(PendingClass {
                    cls: cls.clone(),
                    deps,
                    obligations: cc.pending,
                    properties: cc.pending_props,
                    constants: cc.pending_consts,
                });
            });
            self.load_delayed_classes(&cls.name)?;
            if is_pending(&cls) {
                self.resolve_pending(&cls, Some(&recorded))?;
            }
        }
        Ok((cls, recorded.finish()))
    }
}

/// Linking state shared by nested class declarations: the classes being
/// linked (in the class table but unlinked in php-src), the classes to
/// autoload for pending variance checks (`CG(delayed_autoloads)`) and the
/// nearly linked classes with pending checks
/// (`CG(delayed_variance_obligations)`). (Thread-local rather than a `Vm`
/// field so that it stays inside this file; it is empty between
/// top-level declarations.)
#[derive(Default)]
struct LinkState {
    /// Number of `link_class` calls in progress.
    depth: usize,
    linking: Vec<Rc<ClassDecl>>,
    unlinked_uses: Vec<Rc<ClassDecl>>,
    delayed: Vec<ZStr>,
    pending: Vec<PendingClass>,
}

/// A nearly linked class (`ZEND_ACC_NEARLY_LINKED`).
struct PendingClass {
    cls: Rc<Class>,
    /// Nearly linked parent/interfaces (`OBLIGATION_DEPENDENCY`).
    deps: Vec<Rc<Class>>,
    /// `OBLIGATION_COMPATIBILITY`.
    obligations: Vec<crate::variance::Obligation>,
    properties: Vec<crate::variance::PropertyObligation>,
    constants: Vec<crate::variance::ConstantObligation>,
}

thread_local! {
    static LINK_STATE: RefCell<LinkState> = RefCell::new(LinkState::default());
    /// `EG(errors)` while `EG(record_errors)`: the warnings recorded by the
    /// classes being linked (outermost first), with their file.
    static RECORDED: RefCell<Vec<(ZStr, Recorded)>> = const { RefCell::new(Vec::new()) };
}

/// A class whose linking is in progress, by name.
/// Recorded link-time errors: (level, message, line).
type Recorded = Vec<(i64, String, u32)>;

/// The errors one class link records (`zend_begin_record_errors`): they are
/// emitted once the class is declared, or before a fatal error raised in
/// the meantime ([`emit_recorded_before_fatal`]). Dropping it (any exit)
/// discards them (`zend_free_recorded_errors`).
pub(crate) struct RecordedErrors {
    depth: usize,
}

impl RecordedErrors {
    fn begin(file: ZStr, initial: Recorded) -> Self {
        RECORDED.with(|r| {
            let mut r = r.borrow_mut();
            r.push((file, initial));
            Self { depth: r.len() }
        })
    }

    fn push(&self, e: (i64, String, u32)) {
        RECORDED.with(|r| {
            if let Some((_, v)) = r.borrow_mut().get_mut(self.depth - 1) {
                v.push(e);
            }
        });
    }

    /// The errors to emit after the class is declared.
    fn finish(self) -> Recorded {
        RECORDED.with(|r| {
            r.borrow_mut()
                .get_mut(self.depth - 1)
                .map(|(_, v)| std::mem::take(v))
                .unwrap_or_default()
        })
    }
}

impl Drop for RecordedErrors {
    fn drop(&mut self) {
        RECORDED.with(|r| r.borrow_mut().truncate(self.depth - 1));
    }
}

/// `zend_error_impl` before a fatal error: the errors recorded by the
/// classes being linked are emitted first, with the user error handler
/// disabled (Zend/zend.c, "Emit any delayed error before handling fatal
/// error").
#[cold]
#[inline(never)]
pub(crate) fn emit_recorded_before_fatal(vm: &mut Vm) {
    let pending: Vec<(ZStr, Recorded)> = RECORDED.with(|r| {
        r.borrow_mut()
            .iter_mut()
            .filter(|(_, v)| !v.is_empty())
            .map(|(f, v)| (f.clone(), std::mem::take(v)))
            .collect()
    });
    if pending.is_empty() {
        return;
    }
    let handlers = std::mem::take(&mut vm.error_handlers);
    // The warnings below must not clear the backtrace the pending fatal
    // error already recorded (`note_error_backtrace`).
    let fatal_trace = vm.last_fatal_error_backtrace.take();
    for (file, errors) in pending {
        for (level, msg, line) in errors {
            let _ = vm.error_at(level, &msg, &file, line);
        }
    }
    vm.last_fatal_error_backtrace = fatal_trace;
    vm.error_handlers = handlers;
}

pub(crate) fn unlinked_decl(name: &[u8]) -> Option<Rc<ClassDecl>> {
    LINK_STATE.with(|st| {
        st.borrow()
            .linking
            .iter()
            .rev()
            .find(|d| d.name.eq_ignore_ascii_case(name))
            .cloned()
    })
}

/// Looks up an unlinked class for variance and records the hierarchy dependency.
pub(crate) fn variance_decl(name: &[u8]) -> Option<Rc<ClassDecl>> {
    let decl = unlinked_decl(name)?;
    LINK_STATE.with(|st| {
        let mut st = st.borrow_mut();
        if !st.unlinked_uses.iter().any(|d| Rc::ptr_eq(d, &decl)) {
            st.unlinked_uses.push(decl.clone());
        }
    });
    Some(decl)
}

/// The `%s %pS cannot implement previously implemented interface %pS`
/// compile error of `zend_do_implement_interfaces` (`Zend/zend_inheritance.c`).
#[cold]
fn dup_interface_error(vm: &mut Vm, kind: ClassKind, name: &str, iface: &ZStr, line: u32) -> Throw {
    // `zend_get_object_type_uc`.
    let kind = match kind {
        ClassKind::Trait => "Trait",
        ClassKind::Interface => "Interface",
        ClassKind::Enum => "Enum",
        ClassKind::Class => "Class",
    };
    vm.fatal_at(
        &format!("{kind} {name} cannot implement previously implemented interface {iface}"),
        line,
    )
}

/// `zend_class_redeclaration_error_ex` (`Zend/zend_API.c)`: the message for
/// declaring `new_name` (a class declaration or a `class_alias()` alias)
/// while `old` already holds that name.
#[cold]
pub(crate) fn class_redeclaration_message(new_name: &ZStr, old: &Class) -> String {
    // `zend_get_object_type`.
    let kind = if old.is_trait() {
        "trait"
    } else if old.is_interface() {
        "interface"
    } else if old.is_enum() {
        "enum"
    } else {
        "class"
    };
    match &old.filename {
        Some(file) if old.flags & cflags::INTERNAL == 0 => format!(
            "Cannot redeclare {kind} {new_name} (previously declared in {file}:{})",
            old.line_start
        ),
        _ => format!("Cannot redeclare {kind} {new_name}"),
    }
}

/// Whether a declared class still waits for variance checks.
fn is_pending(cls: &Rc<Class>) -> bool {
    LINK_STATE.with(|st| st.borrow().pending.iter().any(|p| Rc::ptr_eq(&p.cls, cls)))
}

/// Keeps a declaration on the [`LinkState::linking`] stack while it is
/// linked.
struct LinkingGuard(Rc<ClassDecl>);

impl LinkingGuard {
    fn enter(decl: &Rc<ClassDecl>) -> Self {
        LINK_STATE.with(|st| {
            let mut st = st.borrow_mut();
            if st.depth == 0 {
                // A previous top-level declaration ended in a fatal error.
                st.linking.clear();
                st.unlinked_uses.clear();
                st.delayed.clear();
                st.pending.clear();
            }
            st.depth += 1;
            st.linking.push(decl.clone());
        });
        LinkingGuard(decl.clone())
    }

    /// The class is declared (nearly linked): no longer unlinked.
    fn declared(&self) {
        LINK_STATE.with(|st| {
            let mut st = st.borrow_mut();
            if let Some(i) = st.linking.iter().rposition(|d| Rc::ptr_eq(d, &self.0)) {
                st.linking.remove(i);
            }
        });
    }
}

impl Drop for LinkingGuard {
    fn drop(&mut self) {
        self.declared();
        LINK_STATE.with(|st| {
            let mut st = st.borrow_mut();
            st.unlinked_uses.retain(|d| !Rc::ptr_eq(d, &self.0));
            st.depth = st.depth.saturating_sub(1);
        });
    }
}

/// What [`build_class`] leaves for [`Vm::link_class`] to do with VM access.
#[derive(Default)]
struct LinkOut {
    /// Abstract trait methods to check against the method that satisfies
    /// them.
    trait_checks: Vec<TraitAbstractCheck>,
    /// `E_COMPILE_WARNING`s raised while binding traits.
    warnings: Vec<String>,
    /// Trait constant values and property defaults that must be identical
    /// once evaluated (`check_trait_property_or_constant_value_compatibility`),
    /// with the error to raise if they are not.
    value_checks: Vec<(ConstExpr, ConstExpr, String)>,
}

/// An abstract trait method (`proto`, still scoped to its trait) that an
/// existing method (`child`) of the class must be compatible with
/// (`zend_add_trait_method` -> `do_inheritance_check_on_method`).
struct TraitAbstractCheck {
    child: Rc<Method>,
    /// For a child that is itself a not yet fixed up trait method: the
    /// trait it comes from (its scope in messages).
    child_trait: Option<ZStr>,
    proto: Rc<Method>,
}

/// A copy of a trait method for the using class, under the name it is
/// added with: it gets its own static variables (`zend_add_trait_method`
/// copies the op array).
fn trait_method_copy(f: &Func, name: &ZStr) -> Func {
    match f {
        Func::User(u) => Func::User(Rc::new(UserFunction {
            // `new_fn->common.function_name = name` (an alias).
            name: name.clone(),
            params: u.params.clone(),
            required: u.required,
            return_type: u.return_type.clone(),
            flags: u.flags,
            code: u.code.clone(),
            line_start: u.line_start,
            line_end: u.line_end,
            doc_comment: u.doc_comment.clone(),
            statics: RefCell::new(Vec::new()),
            uses: u.uses.clone(),
            class_name: u.class_name.clone(),
            attributes: u.attributes.clone(),
            hook_prop: u.hook_prop.clone(),
            call_shape: crate::func::CallShape::default(),
        })),
        Func::Builtin(_) => f.clone(),
    }
}

/// Whether two functions share their code (`existing_fn->op_array.opcodes
/// == fn->op_array.opcodes`).
fn same_code(a: &Func, b: &Func) -> bool {
    match (a, b) {
        (Func::User(x), Func::User(y)) => Rc::ptr_eq(&x.code, &y.code),
        _ => Func::ptr_eq(a, b),
    }
}

/// Applies the modifiers of an `as` rule to a trait method's flags.
fn alias_flags(modifiers: u32, fn_flags: u32) -> u32 {
    if modifiers & flags::VISIBILITY != 0 {
        modifiers | (fn_flags & !flags::VISIBILITY)
    } else {
        modifiers | fn_flags
    }
}

/// `zend_do_traits_method_binding` / `zend_traits_copy_functions` /
/// `zend_add_trait_method` over the method table being built.
struct TraitMethodBinder<'a> {
    class: &'a ZStr,
    weak: &'a Weak<Class>,
    parent: Option<&'a Rc<Class>>,
    traits: &'a [Rc<Class>],
    rules: &'a TraitRules,
    methods: &'a mut NameMap<Rc<Method>>,
    order: &'a mut Vec<ZStr>,
    /// Methods copied from a trait whose scope has not been fixed up to
    /// the class yet: lowercase name -> trait index.
    unfixed: NameMap<usize>,
    checks: &'a mut Vec<TraitAbstractCheck>,
    warnings: &'a mut Vec<String>,
    err: &'a mut Option<String>,
}

impl TraitMethodBinder<'_> {
    /// Binds the non-abstract (`abstract_pass == false`) or the abstract
    /// methods of all used traits.
    fn bind(&mut self, decl: &ClassDecl, abstract_pass: bool) {
        for ti in 0..self.traits.len() {
            let t = self.traits[ti].clone();
            for lcm in &t.method_order {
                let Some(tm) = t.methods.get(lcm) else {
                    continue;
                };
                if tm.is_abstract() != abstract_pass {
                    continue;
                }
                // Aliases with a new name.
                for (ri, r) in decl.trait_rules.iter().enumerate() {
                    let TraitRule::Alias {
                        method,
                        flags: f,
                        alias: Some(a),
                        ..
                    } = r
                    else {
                        continue;
                    };
                    if self.rules.alias_trait.get(ri).copied().flatten() != Some(ti)
                        || !method.eq_ignore_ascii_case(lcm.as_bytes())
                    {
                        continue;
                    }
                    let fl = alias_flags(*f, tm.flags);
                    self.check_private_final(tm.flags, fl, a);
                    self.add(a, a.to_ascii_lowercase(), tm, fl, ti);
                }
                if self
                    .rules
                    .excluded
                    .iter()
                    .any(|(i, ml)| *i == ti && ml == lcm)
                {
                    continue;
                }
                // Aliases that only change the modifiers.
                let mut fl = tm.flags;
                for (ri, r) in decl.trait_rules.iter().enumerate() {
                    if let TraitRule::Alias {
                        method,
                        flags: f,
                        alias: None,
                        ..
                    } = r
                    {
                        if *f != 0
                            && self.rules.alias_trait.get(ri).copied().flatten() == Some(ti)
                            && method.eq_ignore_ascii_case(lcm.as_bytes())
                        {
                            fl = alias_flags(*f, tm.flags);
                        }
                    }
                }
                self.check_private_final(tm.flags, fl, lcm);
                self.add(&tm.name, lcm.clone(), tm, fl, ti);
            }
        }
    }

    /// `zend_traits_check_private_final_inheritance`.
    fn check_private_final(&mut self, orig: u32, fl: u32, name: &ZStr) {
        if orig & flags::FINAL == 0
            && fl & (flags::PRIVATE | flags::FINAL) == flags::PRIVATE | flags::FINAL
            && !name.eq_ignore_ascii_case(b"__construct")
        {
            self.warnings.push(
                "Private methods cannot be final as they are never overridden by other classes"
                    .to_string(),
            );
        }
    }

    /// `zend_add_trait_method`.
    fn add(&mut self, name: &ZStr, key: ZStr, tm: &Rc<Method>, fl: u32, ti: usize) {
        if let Some(existing) = self.methods.get(&key) {
            let ex_trait = self.unfixed.get(&key).copied();
            if ex_trait.is_some()
                && same_code(&existing.func, &tm.func)
                && existing.visibility() == fl & flags::VISIBILITY
            {
                return;
            }
            if fl & flags::ABSTRACT != 0 {
                // The requirement must be satisfied by the existing method.
                self.checks.push(TraitAbstractCheck {
                    child: existing.clone(),
                    child_trait: ex_trait.map(|i| self.traits[i].name.clone()),
                    proto: tm.clone(),
                });
                return;
            }
            match ex_trait {
                // Members of the class itself override trait methods.
                None if Weak::ptr_eq(&existing.scope, self.weak) => return,
                Some(ei) if !existing.is_abstract() => {
                    if self.err.is_none() {
                        *self.err = Some(format!(
                            "Trait method {}::{} has not been applied as {}::{name}, because of collision with {}::{}",
                            self.traits[ti].name,
                            tm.name,
                            self.class,
                            self.traits[ei].name,
                            existing.name
                        ));
                    }
                    return;
                }
                _ => {}
            }
        }
        let proto = inherited_proto_scope(self.parent, &key, self.weak);
        let m = Rc::new(Method {
            name: name.clone(),
            func: trait_method_copy(&tm.func, name),
            flags: fl | flags::FROM_TRAIT,
            scope: self.weak.clone(),
            proto_scope: proto,
        });
        if self.methods.insert(key.clone(), m).is_none() {
            self.order.push(key.clone());
        }
        self.unfixed.insert(key, ti);
    }
}

/// The resolved trait adaptation rules of a class
/// (`zend_traits_init_trait_structures`).
#[derive(Default)]
struct TraitRules {
    /// (index into the used traits, lowercase method) excluded by
    /// `insteadof`.
    excluded: Vec<(usize, ZStr)>,
    /// For each rule of the declaration: the index of the trait an `as`
    /// rule applies to (`None` for precedence rules).
    alias_trait: Vec<Option<usize>>,
}

/// The nearest ancestor's private static `$name` that `cls` does not
/// inherit into its own static table.
fn inherited_private_static(cls: &Class, name: &[u8]) -> Option<StaticProp> {
    let mut cur = cls.parent.as_ref();
    while let Some(c) = cur {
        if let Some(sp) = c.find_static(name) {
            return (sp.flags & flags::PRIVATE != 0).then(|| sp.clone());
        }
        cur = c.parent.as_ref();
    }
    None
}

/// The prototype root (`zend_get_function_root_class()`) of a method that
/// overrides the parent's same-named one (`do_inheritance_check_on_method()`):
/// a private parent method gives none, and a constructor only keeps an
/// abstract prototype (declared abstract or by an interface).
fn inherited_proto_scope(parent: Option<&Rc<Class>>, lc: &ZStr, own: &Weak<Class>) -> Weak<Class> {
    let Some(pm) = parent.and_then(|p| p.methods.get(lc)) else {
        return own.clone();
    };
    if pm.visibility() == flags::PRIVATE {
        return own.clone();
    }
    if lc.as_bytes() == b"__construct"
        && !pm.is_abstract()
        && !pm
            .proto_scope
            .upgrade()
            .and_then(|p| p.methods.get(lc).cloned())
            .is_some_and(|m| m.is_abstract())
    {
        return own.clone();
    }
    pm.proto_scope.clone()
}

fn is_valid_class_name(n: &[u8]) -> bool {
    !n.is_empty()
        && n.iter()
            .all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'\\' || c >= 0x80)
}

/// `inherit_property_hook`: a redeclared property keeps the parent's hooks
/// it does not declare itself. An abstract parent hook is satisfied by a
/// backed property (`get` always, `set` unless readonly) and otherwise
/// stays abstract.
fn inherit_prop_hooks(
    parent: Option<&Rc<PropInfo>>,
    child: Option<&Rc<crate::class::PropHooks>>,
    child_flags: u32,
) -> Option<Rc<crate::class::PropHooks>> {
    let Some(ph) = parent.and_then(|p| p.hooks.as_ref()) else {
        return child.cloned();
    };
    let backed = child_flags & flags::VIRTUAL == 0;
    let readonly = child_flags & flags::READONLY != 0;
    let mut h = match child {
        Some(c) => (**c).clone(),
        None => crate::class::PropHooks {
            class: ph.class.clone(),
            ..Default::default()
        },
    };
    let mut changed = false;
    if h.get.is_none() && ph.get.is_some() && !(ph.abstract_get && backed) {
        h.get.clone_from(&ph.get);
        h.final_get = ph.final_get;
        h.abstract_get = ph.abstract_get;
        changed = true;
    }
    if h.set.is_none() && ph.set.is_some() && !(ph.abstract_set && backed && !readonly) {
        h.set.clone_from(&ph.set);
        h.final_set = ph.final_set;
        h.abstract_set = ph.abstract_set;
        changed = true;
    }
    if !changed {
        return child.cloned();
    }
    Some(Rc::new(h))
}

/// The `inherits both ... which is ambiguous` (or final override) error
/// of `do_inherit_constant_check` for an interface constant `c` whose name
/// is already inherited (`existing`) from the parent or another interface.
#[cold]
fn inherited_constant_conflict(
    decl: &ClassDecl,
    display: &ZStr,
    existing: &ClassConst,
    c: &ClassConst,
) -> Option<String> {
    if existing.class == c.class {
        return None;
    }
    if c.flags & flags::FINAL != 0 {
        return Some(format!(
            "{}::{} cannot override final constant {}::{}",
            object::display_name(&existing.class),
            c.name,
            object::display_name(&c.class),
            c.name
        ));
    }
    // `zend_get_object_type_uc(ce)`.
    let kind = match decl.kind {
        ClassKind::Interface => "Interface",
        ClassKind::Trait => "Trait",
        ClassKind::Enum => "Enum",
        ClassKind::Class => "Class",
    };
    Some(format!(
        "{kind} {display} inherits both {}::{} and {}::{}, which is ambiguous",
        object::display_name(&existing.class),
        c.name,
        object::display_name(&c.class),
        c.name
    ))
}

/// The `Access level to C::X must be ... (as in class P)` check of
/// `do_inherit_constant_check` for an own constant (`name`, `fl`) overriding the
/// inherited `parent`.
fn constant_access_level_error(
    display: &ZStr,
    (name, fl): (&ZStr, u32),
    parent: &ClassConst,
    interfaces: &[Rc<Class>],
) -> Option<String> {
    let pv = parent.flags & flags::VISIBILITY;
    if fl & flags::VISIBILITY <= pv {
        return None;
    }
    // `zend_get_object_type(parent_constant->ce)`.
    let kind = if interfaces.iter().any(|i| i.name == parent.class) {
        "interface"
    } else {
        "class"
    };
    Some(format!(
        "Access level to {display}::{name} must be {} (as in {kind} {}){}",
        visibility_name(pv),
        object::display_name(&parent.class),
        if pv == flags::PUBLIC {
            ""
        } else {
            " or weaker"
        }
    ))
}

/// `do_trait_constant_check` for trait constant `tc` of `traits[ti]` that
/// meets `existing`, a constant the class already has (its own, an enum
/// case or one of an earlier trait). Values that need evaluating are
/// compared later by [`Vm::link_class`].
fn trait_constant_conflict(
    class: &ZStr,
    traits: &[Rc<Class>],
    ti: usize,
    existing: &Rc<ClassConst>,
    tc: &Rc<ClassConst>,
    link_out: &mut LinkOut,
) -> Option<String> {
    if existing.is_case {
        return Some(format!(
            "Cannot use trait {t}, because {t}::{n} conflicts with enum case {class}::{n}",
            t = tc.class,
            n = tc.name
        ));
    }
    // `find_first_constant_definition`.
    let first = traits[..ti]
        .iter()
        .find(|t| t.consts.iter().any(|c| c.name == tc.name))
        .map_or_else(|| class.clone(), |t| t.name.clone());
    let msg = format!(
        "{first} and {} define the same constant ({}) in the composition of {class}. However, the definition differs and is considered incompatible. Class was composed",
        tc.class, tc.name
    );
    let mask = flags::VISIBILITY | flags::FINAL;
    if existing.flags & mask != tc.flags & mask || existing.ty != tc.ty {
        return Some(msg);
    }
    match (&existing.expr, &tc.expr) {
        (ConstExpr::Value(a), ConstExpr::Value(b)) => {
            (!php_runtime::ops::is_identical(a, b).unwrap_or(false)).then_some(msg)
        }
        (a, b) => {
            link_out.value_checks.push((a.clone(), b.clone(), msg));
            None
        }
    }
}

/// The property declarations of a trait, including those it got from the
/// traits it uses (its `properties_info` after its own composition).
fn trait_prop_decls(t: &Rc<Class>) -> Vec<crate::class::PropDecl> {
    let mut out: Vec<crate::class::PropDecl> =
        t.decl.as_ref().map(|d| d.props.clone()).unwrap_or_default();
    for inner in &t.traits {
        for p in trait_prop_decls(inner) {
            if !out.iter().any(|q| q.name == p.name) {
                out.push(p);
            }
        }
    }
    out
}

/// `zend_do_traits_property_binding`: the error for a property of trait
/// `traits[ti]` that collides with an own (or earlier trait's) property of
/// `class` in an incompatible way. Traits are bound before inheritance, so
/// inherited properties are not considered here.
#[allow(clippy::mutable_key_type, clippy::too_many_arguments)] // `ZStr` keys hash by content only.
fn trait_property_conflict(
    class: &ZStr,
    traits: &[Rc<Class>],
    ti: usize,
    pd: &crate::class::PropDecl,
    prop_map: &NameMap<Rc<PropInfo>>,
    static_map: &NameMap<usize>,
    static_props: &[StaticProp],
    link_out: &mut LinkOut,
) -> Option<String> {
    let (cflags_, cty, cdefault, chooked) =
        if let Some(e) = prop_map.get(&pd.name).filter(|e| e.class == *class) {
            (
                e.flags,
                e.ty.as_ref(),
                e.default.as_ref(),
                e.hooks.is_some(),
            )
        } else {
            let sp = static_map
                .get(&pd.name)
                .map(|&i| &static_props[i])
                .filter(|sp| sp.class == *class)?;
            (sp.flags, sp.ty.as_ref(), sp.default.as_ref(), false)
        };
    // `find_first_property_definition`.
    let first = traits[..ti]
        .iter()
        .find(|t| trait_prop_decls(t).iter().any(|p| p.name == pd.name))
        .map_or_else(|| class.clone(), |t| t.name.clone());
    let tname = &traits[ti].name;
    if chooked || pd.hooks.is_some() {
        return Some(format!(
            "{first} and {tname} define the same hooked property (${}) in the composition of {class}. Conflict resolution between hooked properties is currently not supported. Class was composed",
            pd.name
        ));
    }
    let msg = format!(
        "{first} and {tname} define the same property (${}) in the composition of {class}. However, the definition differs and is considered incompatible. Class was composed",
        pd.name
    );
    let mask = flags::VISIBILITY | flags::STATIC | flags::READONLY;
    if cflags_ & mask != pd.flags & mask || cty != pd.ty.as_ref() {
        return Some(msg);
    }
    // `check_trait_property_or_constant_value_compatibility`; defaults that
    // need evaluating are compared by `Vm::link_class`.
    match (cdefault, pd.default.as_ref()) {
        (None, None) => None,
        (Some(ConstExpr::Value(x)), Some(ConstExpr::Value(y))) => {
            (!php_runtime::ops::is_identical(x, y).unwrap_or(false)).then_some(msg)
        }
        (Some(a @ ConstExpr::Code(_)), Some(b)) | (Some(a), Some(b @ ConstExpr::Code(_))) => {
            link_out.value_checks.push((a.clone(), b.clone(), msg));
            None
        }
        _ => Some(msg),
    }
}

fn visibility_name(f: u32) -> &'static str {
    match f & flags::VISIBILITY {
        flags::PRIVATE => "private",
        flags::PROTECTED => "protected",
        _ => "public",
    }
}

/// `do_inherit_property` for a static property `pd` declared in class
/// `name`: an inherited non-static property cannot become static, and an
/// inherited static one may only widen its visibility.
#[cold]
#[allow(clippy::mutable_key_type)] // ZStr's cached-hash cell never changes Hash/Eq
fn static_prop_override_error(
    pd: &crate::class::PropDecl,
    name: &ZStr,
    prop_map: &NameMap<Rc<PropInfo>>,
    static_map: &NameMap<usize>,
    static_props: &[StaticProp],
) -> Option<String> {
    if let Some(e) = prop_map
        .get(&pd.name)
        .filter(|e| e.class != *name && e.visibility() != flags::PRIVATE)
    {
        return Some(format!(
            "Cannot redeclare non static {}::${} as static {}::${}",
            object::display_name(&e.class),
            pd.name,
            object::display_name(name),
            pd.name
        ));
    }
    let sp = &static_props[*static_map.get(&pd.name)?];
    let parent_vis = sp.flags & flags::VISIBILITY;
    if sp.class == *name
        || parent_vis == flags::PRIVATE
        || pd.flags & flags::VISIBILITY <= parent_vis
    {
        return None;
    }
    Some(format!(
        "Access level to {}::${} must be {} (as in class {}){}",
        object::display_name(name),
        pd.name,
        visibility_name(sp.flags),
        object::display_name(&sp.class),
        if parent_vis == flags::PUBLIC {
            ""
        } else {
            " or weaker"
        }
    ))
}

/// Checks overriding rules; returns (message, line).
fn check_method_inheritance(cls: &Rc<Class>) -> Option<(String, u32)> {
    if let Some(parent) = &cls.parent {
        if let Some(e) = check_parent_methods(cls, parent) {
            return Some(e);
        }
    }
    check_interface_methods(cls)
}

/// `do_inherit_method` for the methods of interfaces the parent does not
/// already implement: an implementing method (own, from a trait or
/// inherited) must be public and keep the staticness. Interfaces are
/// checked too: an own method, or one taken from an earlier parent
/// interface, against each parent interface's method
/// (`do_interface_implementation`).
fn check_interface_methods(cls: &Rc<Class>) -> Option<(String, u32)> {
    for iface in &cls.interfaces {
        if cls
            .parent
            .as_ref()
            .is_some_and(|p| p.interfaces.iter().any(|i| Rc::ptr_eq(i, iface)))
        {
            continue;
        }
        for lc in &iface.method_order {
            let (Some(im), Some(m)) = (iface.methods.get(lc), cls.methods.get(lc)) else {
                continue;
            };
            if Rc::ptr_eq(im, m) {
                continue;
            }
            let line = crate::variance::func_location(&m.func).1;
            let iname = object::display_name(&iface.name);
            let cname = object::display_name(&m.scope().name);
            if im.is_static() != m.is_static() {
                let msg = if m.is_static() {
                    format!(
                        "Cannot make non static method {iname}::{}() static in class {cname}",
                        m.name
                    )
                } else {
                    format!(
                        "Cannot make static method {iname}::{}() non static in class {cname}",
                        m.name
                    )
                };
                return Some((msg, line));
            }
            if m.visibility() != flags::PUBLIC {
                return Some((
                    format!(
                        "Access level to {cname}::{}() must be public (as in class {iname})",
                        m.name
                    ),
                    line,
                ));
            }
        }
    }
    None
}

/// `do_inheritance_check_on_method` (all but the signature) for the
/// methods overriding a parent method.
fn check_parent_methods(cls: &Rc<Class>, parent: &Rc<Class>) -> Option<(String, u32)> {
    for (lc, m) in &cls.methods {
        let Some(pm) = parent.methods.get(lc) else {
            continue;
        };
        if Rc::ptr_eq(m, pm) {
            continue;
        }
        if !Rc::ptr_eq(&m.scope(), cls) {
            continue;
        }
        let line = match &m.func {
            Func::User(u) => u.line_start,
            Func::Builtin(_) => 0,
        };
        // A private parent method imposes nothing unless it is abstract or
        // the constructor (`do_inheritance_check_on_method`).
        if pm.visibility() == flags::PRIVATE && !pm.is_abstract() && lc.as_bytes() != b"__construct"
        {
            continue;
        }
        if pm.flags & flags::FINAL != 0 {
            return Some((
                format!(
                    "Cannot override final method {}::{}()",
                    object::display_name(&pm.scope().name),
                    pm.name
                ),
                line,
            ));
        }
        if pm.is_static() && !m.is_static() {
            return Some((
                format!(
                    "Cannot make static method {}::{}() non static in class {}",
                    object::display_name(&pm.scope().name),
                    pm.name,
                    object::display_name(&cls.name)
                ),
                line,
            ));
        }
        if !pm.is_static() && m.is_static() {
            return Some((
                format!(
                    "Cannot make non static method {}::{}() static in class {}",
                    object::display_name(&pm.scope().name),
                    pm.name,
                    object::display_name(&cls.name)
                ),
                line,
            ));
        }
        if !pm.is_abstract() && m.is_abstract() {
            return Some((
                format!(
                    "Cannot make non abstract method {}::{}() abstract in class {}",
                    object::display_name(&pm.scope().name),
                    pm.name,
                    object::display_name(&cls.name)
                ),
                line,
            ));
        }
        // Constructors only inherit a prototype (and its visibility) from
        // an abstract or interface declaration, which may sit further up
        // than the parent (`parent = proto` in
        // `do_inheritance_check_on_method`).
        let mut as_in = pm.scope();
        if lc.as_bytes() == b"__construct" {
            let proto = pm.proto_scope.upgrade();
            let proto_abstract = proto.as_ref().is_some_and(|p| {
                p.is_interface() || p.methods.get(lc).is_some_and(|pr| pr.is_abstract())
            });
            if !pm.is_abstract() && !proto_abstract {
                continue;
            }
            if let Some(p) = proto {
                as_in = p;
            }
        }
        let pv = pm.visibility();
        let v = m.visibility();
        let weaker = match pv {
            flags::PUBLIC => v != flags::PUBLIC,
            flags::PROTECTED => v == flags::PRIVATE,
            _ => false,
        };
        if weaker {
            return Some((
                format!(
                    "Access level to {}::{}() must be {} (as in class {}){}",
                    object::display_name(&cls.name),
                    m.name,
                    visibility_name(pv),
                    object::display_name(&as_in.name),
                    if pv == flags::PROTECTED {
                        " or weaker"
                    } else {
                        ""
                    }
                ),
                line,
            ));
        }
    }
    None
}

/// Builds the class entry (inside `Rc::new_cyclic`).
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn build_class(
    decl: &Rc<ClassDecl>,
    weak: &Weak<Class>,
    parent: Option<Rc<Class>>,
    interfaces: Vec<Rc<Class>>,
    traits: Vec<Rc<Class>>,
    rules: &TraitRules,
    cflag: u32,
    is_anon: bool,
    err: &mut Option<String>,
    link_out: &mut LinkOut,
) -> Class {
    let name = decl.name.clone();
    let display = object::display_name(&name);
    // Upper bounds of the table sizes (inherited members may be overridden
    // in place), so the tables are built without regrowing, as
    // `zend_hash_extend` sizes them in `zend_do_inheritance`.
    let nconsts = parent.as_ref().map_or(0, |p| p.consts.len())
        + interfaces.iter().map(|i| i.consts.len()).sum::<usize>()
        + traits.iter().map(|t| t.consts.len()).sum::<usize>()
        + decl.consts.len()
        + decl.cases.len();
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: members of the declared class"
    )]
    let mut consts: Vec<Rc<ClassConst>> = Vec::with_capacity(nconsts);
    let mut const_map: NameMap<usize> = name_map_with_capacity(nconsts);
    let add_const =
        |c: Rc<ClassConst>, consts: &mut Vec<Rc<ClassConst>>, map: &mut NameMap<usize>| {
            if let Some(&i) = map.get(&c.name) {
                consts[i] = c;
            } else {
                map.insert(c.name.clone(), consts.len());
                consts.push(c);
            }
        };
    if let Some(p) = &parent {
        for c in &p.consts {
            if c.flags & flags::PRIVATE == 0 {
                add_const(c.clone(), &mut consts, &mut const_map);
            }
        }
    }
    // `do_inherit_iface_constant` / `do_inherit_constant_check`: own and
    // trait constants (bound first in `zend_do_link_class`) shadow the
    // interface ones, which are only checked against them below.
    let is_own = |n: &ZStr| {
        decl.consts.iter().any(|cd| cd.name == *n)
            || traits
                .iter()
                .any(|t| t.consts.iter().any(|tc| tc.name == *n))
    };
    let mut iface_overridden: Vec<Rc<ClassConst>> = Vec::new();
    for i in &interfaces {
        for c in &i.consts {
            if is_own(&c.name) {
                iface_overridden.push(c.clone());
            } else if let Some(&idx) = const_map.get(&c.name) {
                if err.is_none() {
                    *err = inherited_constant_conflict(decl, &display, &consts[idx], c);
                }
            } else {
                add_const(c.clone(), &mut consts, &mut const_map);
            }
        }
    }
    // Own constants and enum cases in declaration order (`zend_compile_
    // class_decl` adds both to `constants_table` as they appear, the order
    // `ReflectionClass::getConstants()` shows).
    for member in decl.own_constants() {
        let (is_case, idx) = match member {
            crate::class::OwnConstant::Case(idx) => (true, idx),
            crate::class::OwnConstant::Const(idx) => (false, idx),
        };
        if is_case {
            let (cname, value, _line) = &decl.cases[idx];
            add_const(
                Rc::new(ClassConst {
                    name: cname.clone(),
                    value: RefCell::new(None),
                    expr: value.clone().unwrap_or(ConstExpr::Value(Value::Null)),
                    flags: flags::PUBLIC,
                    class: name.clone(),
                    ty: None,
                    is_case: true,
                    evaluating: Cell::new(false),
                    doc_comment: decl.case_doc_comments.get(idx).cloned().flatten(),
                    attributes: decl.case_attributes.get(idx).cloned().unwrap_or_default(),
                }),
                &mut consts,
                &mut const_map,
            );
            continue;
        }
        let cd = &decl.consts[idx];
        if let Some(&i) = const_map.get(&cd.name) {
            let existing = &consts[i];
            if existing.flags & flags::FINAL != 0 && err.is_none() {
                *err = Some(format!(
                    "{}::{} cannot override final constant {}::{}",
                    display,
                    cd.name,
                    object::display_name(&existing.class),
                    cd.name
                ));
            }
            if err.is_none() {
                *err = constant_access_level_error(
                    &display,
                    (&cd.name, cd.flags),
                    existing,
                    &interfaces,
                );
            }
        }
        for ic in iface_overridden.iter().filter(|c| c.name == cd.name) {
            if ic.flags & flags::FINAL != 0 && err.is_none() {
                *err = Some(format!(
                    "{display}::{} cannot override final constant {}::{}",
                    cd.name,
                    object::display_name(&ic.class),
                    cd.name
                ));
            }
            if err.is_none() {
                *err = constant_access_level_error(&display, (&cd.name, cd.flags), ic, &interfaces);
            }
        }
        let value = match &cd.value {
            ConstExpr::Value(v) if cd.ty.is_none() => Some(v.clone()),
            _ => None,
        };
        add_const(
            Rc::new(ClassConst {
                name: cd.name.clone(),
                value: RefCell::new(value),
                expr: cd.value.clone(),
                flags: cd.flags,
                class: name.clone(),
                ty: cd.ty.clone(),
                is_case: false,
                evaluating: Cell::new(false),
                doc_comment: cd.doc_comment.clone(),
                attributes: cd.attributes.clone(),
            }),
            &mut consts,
            &mut const_map,
        );
    }

    // `zend_do_traits_constant_binding` (traits are bound before
    // inheritance, so a constant inherited from the parent or an interface
    // is overridden, subject to `do_inherit_constant_check`).
    for (ti, t) in traits.iter().enumerate() {
        for c in &t.consts {
            if let Some(&i) = const_map.get(&c.name) {
                let existing = consts[i].clone();
                if existing.class == name {
                    if err.is_none() {
                        *err = trait_constant_conflict(&name, &traits, ti, &existing, c, link_out);
                    }
                    continue;
                }
                if existing.flags & flags::FINAL != 0 && err.is_none() {
                    *err = Some(format!(
                        "{display}::{} cannot override final constant {}::{}",
                        c.name,
                        object::display_name(&existing.class),
                        c.name
                    ));
                }
            }
            for ic in iface_overridden.iter().filter(|ic| ic.name == c.name) {
                if ic.flags & flags::FINAL != 0 && err.is_none() {
                    *err = Some(format!(
                        "{display}::{} cannot override final constant {}::{}",
                        c.name,
                        object::display_name(&ic.class),
                        c.name
                    ));
                }
                if err.is_none() {
                    *err =
                        constant_access_level_error(&display, (&c.name, c.flags), ic, &interfaces);
                }
            }
            let nc = ClassConst {
                class: name.clone(),
                value: RefCell::new(None),
                evaluating: Cell::new(false),
                ..(**c).clone()
            };
            add_const(Rc::new(nc), &mut consts, &mut const_map);
        }
    }

    // Properties: (declaration, owner, index of the trait it comes from).
    let mut all_props: Vec<(crate::class::PropDecl, ZStr, Option<usize>)> = Vec::new();
    if decl.kind == ClassKind::Enum {
        let mk = |n: &str, ty: crate::types::TypeDecl| crate::class::PropDecl {
            name: ZStr::from(n),
            default: None,
            flags: flags::PUBLIC | flags::READONLY,
            ty: Some(ty),
            doc_comment: None,
            attributes: Vec::new(),
            line: decl.line_start,
            hooks: None,
        };
        all_props.push((
            mk(
                "name",
                crate::types::TypeDecl::simple(crate::types::mask::STRING),
            ),
            name.clone(),
            None,
        ));
        if let Some(b) = decl.backing {
            all_props.push((
                mk("value", crate::types::TypeDecl::simple(b)),
                name.clone(),
                None,
            ));
        }
    }
    // Own properties first; trait properties are bound after them
    // (`zend_do_traits_property_binding`).
    for p in &decl.props {
        all_props.push((p.clone(), name.clone(), None));
    }
    for (ti, t) in traits.iter().enumerate() {
        for p in trait_prop_decls(t) {
            all_props.push((p, name.clone(), Some(ti)));
        }
    }
    let nstatic_decls = all_props
        .iter()
        .filter(|(pd, _, _)| pd.flags & flags::STATIC != 0)
        .count();
    let ninstance_decls = all_props.len() - nstatic_decls;
    let mut props: Vec<Rc<PropInfo>> = parent.as_ref().map(|p| p.props.clone()).unwrap_or_default();
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: members of the declared class"
    )]
    props.reserve(ninstance_decls);
    let mut prop_map: NameMap<Rc<PropInfo>> =
        name_map_with_capacity(parent.as_ref().map_or(0, |p| p.prop_map.len()) + ninstance_decls);
    let nstatics = parent.as_ref().map_or(0, |p| p.static_props.len()) + nstatic_decls;
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: members of the declared class"
    )]
    let mut static_props: Vec<StaticProp> = Vec::with_capacity(nstatics);
    let mut static_map: NameMap<usize> = name_map_with_capacity(nstatics);
    if let Some(p) = &parent {
        for (k, pi) in &p.prop_map {
            if pi.visibility() != flags::PRIVATE {
                prop_map.insert(k.clone(), pi.clone());
            }
        }
        for sp in &p.static_props {
            if sp.flags & flags::PRIVATE == 0 {
                static_map.insert(sp.name.clone(), static_props.len());
                static_props.push(sp.clone());
            }
        }
    }
    for (pd, owner, from_trait) in all_props {
        if let Some(ti) = from_trait {
            if let Some(e) = trait_property_conflict(
                &name,
                &traits,
                ti,
                &pd,
                &prop_map,
                &static_map,
                &static_props,
                link_out,
            ) {
                if err.is_none() {
                    *err = Some(e);
                }
                continue;
            }
            let own = |c: &ZStr| *c == name;
            if prop_map.get(&pd.name).is_some_and(|e| own(&e.class))
                || static_map
                    .get(&pd.name)
                    .is_some_and(|&i| own(&static_props[i].class))
            {
                // Compatible with the own declaration: nothing to add.
                continue;
            }
            if cflag & cflags::READONLY != 0 && pd.flags & flags::READONLY == 0 && err.is_none() {
                *err = Some(format!(
                    "Readonly class {} cannot use trait with a non-readonly property {}::${}",
                    name, traits[ti].name, pd.name
                ));
            }
        }
        // `do_inherit_property`: a final parent property cannot be redeclared.
        let inherited_final = prop_map
            .get(&pd.name)
            .filter(|e| e.class != name)
            .map(|e| (e.flags, e.class.clone()))
            .or_else(|| {
                static_map
                    .get(&pd.name)
                    .map(|&i| &static_props[i])
                    .filter(|sp| sp.class != name)
                    .map(|sp| (sp.flags, sp.class.clone()))
            });
        if let Some((f, owner_class)) = inherited_final {
            if f & flags::FINAL != 0 && err.is_none() {
                *err = Some(format!(
                    "Cannot override final property {owner_class}::${}",
                    pd.name
                ));
            }
        }
        if let (Some(ph), Some(ch)) = (
            prop_map
                .get(&pd.name)
                .filter(|e| e.class != name)
                .and_then(|e| e.hooks.as_ref()),
            pd.hooks.as_ref(),
        ) {
            let fin = if ph.final_get && ch.get.is_some() {
                Some("get")
            } else if ph.final_set && ch.set.is_some() {
                Some("set")
            } else {
                None
            };
            if let Some(kind) = fin {
                if err.is_none() {
                    *err = Some(format!(
                        "Cannot override final property hook {}::${}::{kind}()",
                        ph.class, pd.name
                    ));
                }
            }
        }
        if pd.flags & flags::STATIC != 0 {
            if err.is_none() {
                *err =
                    static_prop_override_error(&pd, &name, &prop_map, &static_map, &static_props);
            }
            let sp = StaticProp {
                name: pd.name.clone(),
                flags: pd.flags,
                ty: pd.ty.clone(),
                class: owner.clone(),
                cell: Rc::new(RefCell::new(Value::Null)),
                default: pd.default.clone(),
                initialized: Rc::new(Cell::new(false)),
                doc_comment: pd.doc_comment.clone(),
            };
            if let Some(&i) = static_map.get(&pd.name) {
                static_props[i] = sp;
            } else {
                static_map.insert(pd.name.clone(), static_props.len());
                static_props.push(sp);
            }
            continue;
        }
        let existing = prop_map.get(&pd.name).cloned();
        let slot = match &existing {
            Some(e) => {
                if e.visibility() == flags::PRIVATE {
                    u32::try_from(props.len()).unwrap_or(0)
                } else {
                    let weaker = match e.visibility() {
                        flags::PUBLIC => pd.flags & flags::VISIBILITY != flags::PUBLIC,
                        flags::PROTECTED => pd.flags & flags::PRIVATE != 0,
                        _ => false,
                    };
                    if weaker && err.is_none() {
                        *err = Some(format!(
                            "Access level to {}::${} must be {} (as in class {}){}",
                            display,
                            pd.name,
                            visibility_name(e.flags),
                            object::display_name(&e.class),
                            if e.visibility() == flags::PROTECTED {
                                " or weaker"
                            } else {
                                ""
                            }
                        ));
                    }
                    // `do_inherit_property`: an abstract (hooked) parent
                    // property may be implemented by a readonly one.
                    if e.flags & flags::READONLY != pd.flags & flags::READONLY
                        && e.flags & flags::ABSTRACT == 0
                        && err.is_none()
                    {
                        *err =
                            Some(format!(
                            "Cannot redeclare {}readonly property {}::${} as {}readonly {}::${}",
                            if e.flags & flags::READONLY != 0 { "" } else { "non-" },
                            object::display_name(&e.class),
                            pd.name,
                            if pd.flags & flags::READONLY != 0 { "" } else { "non-" },
                            display,
                            pd.name
                        ));
                    }
                    // Set visibility may only be the parent's (or weaker);
                    // get-only virtual parents have none.
                    let parent_get_only = e.is_virtual()
                        && e.hooks
                            .as_ref()
                            .is_some_and(|h| h.set.is_none() && !h.abstract_set);
                    if pd.flags & flags::SET_MASK != 0 && !parent_get_only && err.is_none() {
                        let parent_set = match e.flags & flags::SET_MASK {
                            0 => match e.visibility() {
                                flags::PRIVATE => flags::SET_PRIVATE,
                                flags::PROTECTED => flags::SET_PROTECTED,
                                _ => flags::SET_PUBLIC,
                            },
                            s => s,
                        };
                        if pd.flags & flags::SET_MASK > parent_set {
                            let shown = if e.flags & flags::SET_PRIVATE != 0 {
                                "private(set)"
                            } else if e.flags & flags::SET_PROTECTED != 0 {
                                "protected(set)"
                            } else {
                                "omitted"
                            };
                            *err = Some(format!(
                                "Set access level of {}::${} must be {shown} (as in class {}){}",
                                name,
                                pd.name,
                                e.class,
                                if e.flags & flags::SET_MASK == 0 {
                                    ""
                                } else {
                                    " or weaker"
                                }
                            ));
                        }
                    }
                    e.slot
                }
            }
            None => u32::try_from(props.len()).unwrap_or(0),
        };
        if let Some(sp) = static_map.get(&pd.name) {
            let sp = &static_props[*sp];
            if err.is_none() {
                *err = Some(format!(
                    "Cannot redeclare static {}::${} as non static {}::${}",
                    object::display_name(&sp.class),
                    pd.name,
                    display,
                    pd.name
                ));
            }
        }
        let mut pflags = pd.flags;
        if existing
            .as_ref()
            .is_some_and(|e| e.visibility() != flags::PRIVATE && !e.is_virtual())
        {
            pflags &= !flags::VIRTUAL;
        }
        let pi = Rc::new(PropInfo {
            name: pd.name.clone(),
            slot,
            flags: pflags,
            ty: pd.ty.clone(),
            class: owner,
            default: pd.default.clone().or_else(|| {
                (pflags & flags::VIRTUAL == 0 && pd.ty.is_none())
                    .then_some(ConstExpr::Value(Value::Null))
            }),
            doc_comment: pd.doc_comment.clone(),
            attributes: pd.attributes.clone(),
            hooks: inherit_prop_hooks(
                existing
                    .as_ref()
                    .filter(|e| e.visibility() != flags::PRIVATE),
                pd.hooks.as_ref(),
                pflags,
            ),
        });
        if pi.is_virtual() && pi.default.is_some() && err.is_none() {
            *err = Some(format!(
                "Cannot specify default value for virtual hooked property {}::${}",
                display, pd.name
            ));
        }
        if !pi.is_virtual()
            && pi.hooks.as_ref().is_some_and(|h| {
                h.set.is_some()
                    && h.get
                        .as_ref()
                        .is_some_and(|g| g.flags & flags::RETURN_REF != 0)
            })
            && err.is_none()
        {
            *err = Some(format!(
                "Get hook of backed property {}::{} with set hook may not return by reference",
                display, pd.name
            ));
        }
        // `zend_verify_hooked_property`: a virtual property with only one
        // hook cannot have a separate set visibility.
        if pi.is_virtual() && pflags & flags::SET_MASK != 0 && err.is_none() {
            if let Some(h) = pi.hooks.as_ref() {
                if h.get.is_none() || h.set.is_none() {
                    let only = if h.get.is_none() { "set" } else { "get" };
                    *err = Some(format!(
                        "{only}-only virtual property {display}::${} must not specify asymmetric visibility",
                        pd.name
                    ));
                }
            }
        }
        if (slot as usize) < props.len() {
            props[slot as usize] = pi.clone();
        } else {
            props.push(pi.clone());
        }
        prop_map.insert(pd.name.clone(), pi);
    }
    // Private props of this class must be accessible by name too.

    // Methods. A concrete class has every interface method among its own,
    // trait or inherited ones; only an interface or an abstract class can
    // take new entries from its interfaces.
    let ntrait_methods = traits.iter().map(|t| t.method_order.len()).sum::<usize>();
    let niface_methods = if decl.kind == ClassKind::Interface || cflag & cflags::ABSTRACT != 0 {
        interfaces
            .iter()
            .map(|i| i.method_order.len())
            .sum::<usize>()
    } else {
        0
    };
    let nmethods = decl.methods.len()
        + if decl.kind == ClassKind::Enum {
            crate::builtins::enum_methods().len()
        } else {
            0
        }
        + ntrait_methods
        + decl.trait_rules.len()
        + parent.as_ref().map_or(0, |p| p.method_order.len())
        + niface_methods;
    let mut methods: NameMap<Rc<Method>> = name_map_with_capacity(nmethods);
    #[expect(
        clippy::disallowed_methods,
        reason = "bounded: members of the declared class"
    )]
    let mut method_order: Vec<ZStr> = Vec::with_capacity(nmethods);
    // Own methods first (for get_class_methods order), then trait methods,
    // then inherited ones.
    for md in &decl.methods {
        let lc = md.name.to_ascii_lowercase();
        let proto = inherited_proto_scope(parent.as_ref(), &lc, weak);
        let m = Rc::new(Method {
            name: md.name.clone(),
            func: Func::User(md.func.clone()),
            flags: md.flags,
            scope: weak.clone(),
            proto_scope: proto,
        });
        if methods.insert(lc.clone(), m).is_none() {
            method_order.push(lc);
        }
    }
    if decl.kind == ClassKind::Enum {
        for (i, (sig, fl, h)) in crate::builtins::enum_methods().into_iter().enumerate() {
            if i > 0 && decl.backing.is_none() {
                break;
            }
            let s = crate::builtins::sig::parse_sig_borrowed(sig);
            let lc = ZStr::from(s.name.to_ascii_lowercase());
            if methods.contains_key(&lc) {
                // `zend_enum_register_func` (the name is the known string
                // it registers: `cases`, `from`, `tryfrom`).
                if err.is_none() {
                    *err = Some(format!("Cannot redeclare {display}::{lc}()"));
                }
                continue;
            }
            let name = s.name;
            let b = crate::func::Builtin::new(ZStr::from(name), s.into(), fl, h);
            method_order.push(lc.clone());
            methods.insert(
                lc,
                Rc::new(Method {
                    name: ZStr::from(name),
                    func: Func::Builtin(Rc::new(b)),
                    flags: fl,
                    scope: weak.clone(),
                    proto_scope: weak.clone(),
                }),
            );
        }
    }
    // `zend_do_traits_method_binding`: non-abstract trait methods are bound
    // before inheritance, abstract ones (requirements) after it.
    let mut binder = TraitMethodBinder {
        class: &name,
        weak,
        parent: parent.as_ref(),
        traits: &traits,
        rules,
        methods: &mut methods,
        order: &mut method_order,
        unfixed: name_map_with_capacity(ntrait_methods),
        checks: &mut link_out.trait_checks,
        warnings: &mut link_out.warnings,
        err,
    };
    binder.bind(decl, false);
    // `zend_fixup_trait_method`: the copies now belong to the class.
    binder.unfixed.clear();
    if let Some(p) = &parent {
        for lc in &p.method_order {
            if binder.methods.contains_key(lc) {
                continue;
            }
            if let Some(m) = p.methods.get(lc) {
                binder.order.push(lc.clone());
                binder.methods.insert(lc.clone(), m.clone());
            }
        }
    }
    binder.bind(decl, true);
    for i in &interfaces {
        for lc in &i.method_order {
            if methods.contains_key(lc) {
                continue;
            }
            if let Some(m) = i.methods.get(lc) {
                method_order.push(lc.clone());
                methods.insert(lc.clone(), m.clone());
            }
        }
    }
    let mut cflag = cflag;
    if decl.kind == ClassKind::Interface {
        cflag |= cflags::ABSTRACT;
    }
    let magic = build_magic(&methods);
    let internal = if parent
        .as_ref()
        .is_some_and(|p| p.internal == InternalKind::Throwable)
    {
        InternalKind::Throwable
    } else {
        InternalKind::None
    };
    let _ = is_anon;
    let hooks = parent.as_ref().and_then(|p| p.hooks.clone());
    Class {
        lc_name: name.to_ascii_lowercase(),
        name,
        flags: cflag,
        parent,
        interfaces,
        traits,
        consts,
        const_map,
        props,
        prop_map,
        static_props,
        static_map,
        methods,
        method_order,
        magic,
        defaults: RefCell::new(None),
        backing: decl.backing,
        enum_cases: RefCell::new(NameMap::default()),
        decl: Some(decl.clone()),
        internal,
        attributes: decl.attributes.clone(),
        filename: Some(decl.filename.clone()),
        line_start: decl.line_start,
        line_end: decl.line_end,
        doc_comment: decl.doc_comment.clone(),
        linked: Cell::new(true),
        hooks,
    }
}
