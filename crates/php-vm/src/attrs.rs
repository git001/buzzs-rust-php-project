// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Engine-side attribute semantics: `#[\Deprecated]` messages
//! (`get_deprecation_suffix_from_attribute`, `zend_deprecated_*`).

use std::rc::Rc;

use php_runtime::{Obj, Value, ZStr};

use crate::class::{AttributeInfo, Class};
use crate::vm::{PendingCall, Vm, VmResult};

/// Evaluated attribute arguments: positional and named.
pub(crate) type AttrArgs = (Vec<Value>, Vec<(ZStr, Value)>);

impl Vm {
    /// Evaluates an attribute's arguments in `scope`.
    pub(crate) fn attribute_args(
        &mut self,
        info: &AttributeInfo,
        scope: Option<&Rc<Class>>,
    ) -> VmResult<AttrArgs> {
        let mut pos = Vec::new();
        let mut named = Vec::new();
        for (n, e) in &info.args {
            let v = self.eval_const_expr(e, scope)?;
            match n {
                Some(n) => named.push((n.clone(), v)),
                None => pos.push(v),
            }
        }
        Ok((pos, named))
    }

    /// `zend_get_attribute_object`: instantiates `cls` with the attribute's
    /// arguments.
    pub(crate) fn attribute_object(
        &mut self,
        info: &AttributeInfo,
        cls: &Rc<Class>,
        scope: Option<&Rc<Class>>,
    ) -> VmResult<Obj> {
        let (pos, named) = self.attribute_args(info, scope)?;
        self.construct_with_strict(cls, pos, named, info.strict)
    }

    /// `new cls(...pos, ...named)`.
    pub(crate) fn construct_with(
        &mut self,
        cls: &Rc<Class>,
        pos: Vec<Value>,
        named: Vec<(ZStr, Value)>,
    ) -> VmResult<Obj> {
        self.construct_with_strict(cls, pos, named, false)
    }

    /// [`Self::construct_with`] with an explicit argument-coercion mode.
    pub(crate) fn construct_with_strict(
        &mut self,
        cls: &Rc<Class>,
        pos: Vec<Value>,
        named: Vec<(ZStr, Value)>,
        strict: bool,
    ) -> VmResult<Obj> {
        let o = self.instantiate(cls)?;
        if let Some(m) = &cls.magic.construct {
            let mut call = PendingCall {
                this: Some(o.clone()),
                scope: Some(m.scope()),
                called: Some(cls.clone()),
                strict,
                ..PendingCall::new(m.func.clone())
            };
            let mut call_args = self.call_bufs.args.pop().unwrap_or_default();
            call_args.clear();
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: number of arguments of the call"
            )]
            call_args.reserve(pos.len());
            call_args.extend(pos.into_iter().map(Some));
            call.args = call_args;
            for (k, v) in named {
                let Some(p) = m.func.params().iter().position(|p| p.name == k) else {
                    return Err(self.make_throw(
                        "Error",
                        &format!("Unknown named parameter ${}", k.to_string_lossy()),
                    ));
                };
                if call.args.len() <= p {
                    #[expect(
                        clippy::disallowed_methods,
                        reason = "bounded: position of a declared parameter"
                    )]
                    call.args.resize(p + 1, None);
                }
                call.args[p] = Some(v);
                call.has_named = true;
            }
            self.call_pending(call)?;
        }
        Ok(o)
    }

    /// The `" since X, message"` suffix of a `#[\Deprecated]` attribute in
    /// `attrs` (empty without one or without arguments). Errors raised
    /// while constructing the attribute (e.g. a `TypeError`) propagate.
    pub(crate) fn deprecation_suffix(
        &mut self,
        attrs: &[AttributeInfo],
        scope: Option<&Rc<Class>>,
    ) -> VmResult<Vec<u8>> {
        let Some(info) = attrs
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(b"Deprecated"))
        else {
            return Ok(Vec::new());
        };
        if info.args.is_empty() {
            return Ok(Vec::new());
        }
        let Some(cls) = self.find_class(b"Deprecated") else {
            return Ok(Vec::new());
        };
        let info = info.clone();
        let o = self.attribute_object(&info, &cls, scope)?;
        let message = self.read_prop_raw(&o, "message");
        let since = self.read_prop_raw(&o, "since");
        Ok(format_suffix(since.as_ref(), message.as_ref()))
    }

    fn read_prop_raw(&mut self, o: &Obj, name: &str) -> Option<ZStr> {
        match self.get_property(o, name) {
            Ok(Value::Str(s)) => Some(s),
            _ => None,
        }
    }
}

/// `" since %s, %s"` with empty parts left out.
pub(crate) fn format_suffix(since: Option<&ZStr>, message: Option<&ZStr>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(s) = since.filter(|s| !s.is_empty()) {
        out.extend_from_slice(b" since ");
        out.extend_from_slice(s.as_bytes());
    }
    if let Some(m) = message.filter(|m| !m.is_empty()) {
        out.extend_from_slice(b", ");
        out.extend_from_slice(m.as_bytes());
    }
    out
}

impl Vm {
    /// `A::NAME` from code or `constant()`: the value plus the
    /// `#[\Deprecated]` notice (`zend_deprecated_class_constant`).
    pub(crate) fn fetch_class_constant(&mut self, cls: &Rc<Class>, name: &ZStr) -> VmResult<Value> {
        let v = self.class_constant(cls, name)?;
        let Some(c) = cls.find_const(name.as_bytes()).cloned() else {
            return Ok(v);
        };
        if !c
            .attributes
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(b"Deprecated"))
        {
            return Ok(v);
        }
        // Recursion protection (a message referring to the constant itself).
        let key = Rc::as_ptr(&c) as usize;
        if !self.const_deprecation_guard.insert(key) {
            return Ok(v);
        }
        let decl = self
            .find_class(c.class.as_bytes())
            .unwrap_or_else(|| cls.clone());
        let r = self.deprecation_suffix(&c.attributes, Some(&decl));
        self.const_deprecation_guard.remove(&key);
        let suffix = r?;
        let kind = if c.is_case { "Enum case" } else { "Constant" };
        let mut msg = format!(
            "{kind} {}::{} is deprecated",
            crate::object::display_name(&decl.name),
            name.to_string_lossy()
        )
        .into_bytes();
        msg.extend_from_slice(&suffix);
        let code = if decl.flags & crate::class::cflags::INTERNAL != 0 {
            crate::errors::level::DEPRECATED
        } else {
            crate::errors::level::USER_DEPRECATED
        };
        self.error_bytes(code, &msg)?;
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::format_suffix;
    use php_runtime::ZStr;

    #[test]
    fn suffix_forms() {
        let s = ZStr::from("8.4");
        let m = ZStr::from("use x");
        let e = ZStr::from("");
        assert_eq!(format_suffix(None, None), b"");
        assert_eq!(format_suffix(Some(&s), None), b" since 8.4");
        assert_eq!(format_suffix(None, Some(&m)), b", use x");
        assert_eq!(format_suffix(Some(&s), Some(&m)), b" since 8.4, use x");
        assert_eq!(format_suffix(Some(&e), Some(&e)), b"");
    }
}

impl Vm {
    /// `zend_deprecated_constant` for a global constant being read.
    /// Called on every constant fetch: a sorted list of the names' cached
    /// hashes skips both map lookups for the (common) undeprecated case.
    #[inline]
    pub(crate) fn constant_deprecation(&mut self, name: &ZStr) -> VmResult<()> {
        let total = self.deprecated_constants.len() + self.const_attributes.len();
        if total == 0 {
            return Ok(());
        }
        let h = name.hash_value();
        let hit = {
            let mut f = self.const_deprecation_filter.borrow_mut();
            if f.0 != total {
                // php-src keeps the deprecation flag in the constant
                // itself (`ZEND_ACC_DEPRECATED`), so this index is not
                // request memory: building it on the first fetch must not
                // move `memory_get_usage()` (Symfony's
                // NativeSessionStorageTest, the first `STDERR` fetch).
                php_memlimit::persistent(|| {
                    let mut hashes: Vec<u64> = self
                        .deprecated_constants
                        .keys()
                        .chain(self.const_attributes.keys())
                        .map(ZStr::hash_value)
                        .collect();
                    hashes.sort_unstable();
                    *f = (total, hashes);
                });
            }
            f.1.binary_search(&h).is_ok()
        };
        if !hit {
            return Ok(());
        }
        self.constant_deprecation_slow(name)
    }

    #[cold]
    #[inline(never)]
    fn constant_deprecation_slow(&mut self, name: &ZStr) -> VmResult<()> {
        if let Some(suffix) = self.deprecated_constants.get(name).cloned() {
            let mut msg = format!("Constant {} is deprecated", name.to_string_lossy()).into_bytes();
            msg.extend_from_slice(&suffix);
            return self.error_bytes(crate::errors::level::DEPRECATED, &msg);
        }
        let Some(attrs) = self.const_attributes.get(name).cloned() else {
            return Ok(());
        };
        if !attrs
            .iter()
            .any(|a| a.name.eq_ignore_ascii_case(b"Deprecated"))
        {
            return Ok(());
        }
        let key = Rc::as_ptr(&attrs) as usize;
        if !self.const_deprecation_guard.insert(key) {
            return Ok(());
        }
        let r = self.deprecation_suffix(&attrs, None);
        self.const_deprecation_guard.remove(&key);
        let mut msg = format!("Constant {} is deprecated", name.to_string_lossy()).into_bytes();
        msg.extend_from_slice(&r?);
        self.error_bytes(crate::errors::level::USER_DEPRECATED, &msg)
    }
}

fn has_attr(attrs: &[AttributeInfo], lc: &[u8]) -> bool {
    attrs.iter().any(|a| {
        let n = a.name.as_bytes();
        n.strip_prefix(b"\\").unwrap_or(n).eq_ignore_ascii_case(lc)
    })
}

/// `zend_inheritance_check_override`: members marked `#[\Override]` that
/// override nothing (message, line).
pub(crate) fn check_override(cls: &Rc<Class>) -> Option<(String, u32)> {
    use crate::func::{flags, Func};
    if cls.is_trait() {
        return None;
    }
    let parent = cls.parent.as_ref();
    // Methods print `ZEND_FN_SCOPE_NAME` (`%s`, cut at the NUL of an
    // anonymous class name); constants and properties print `%pS` (whole).
    let display = crate::object::display_name(&cls.name);
    let full = cls.name.to_string_lossy();
    for lc in &cls.method_order {
        let Some(m) = cls.methods.get(lc) else {
            continue;
        };
        if !Rc::ptr_eq(&m.scope(), cls) {
            continue;
        }
        let Func::User(uf) = &m.func else { continue };
        if !has_attr(&uf.attributes, b"override") {
            continue;
        }
        let in_parent = if lc.as_bytes() == b"__construct" {
            // `do_inheritance_check_on_method`: a ctor only has a prototype
            // when it is abstract (or comes from an interface); otherwise
            // the check returns before `ZEND_INHERITANCE_RESET_CHILD_OVERRIDE`.
            ctor_has_abstract_proto(parent)
        } else {
            parent
                .and_then(|p| p.find_method(lc.as_bytes()))
                .is_some_and(|pm| pm.visibility() != flags::PRIVATE)
        };
        let in_iface = cls
            .interfaces
            .iter()
            .any(|i| i.find_method(lc.as_bytes()).is_some());
        // An abstract trait method implemented by the class counts too.
        let in_trait = cls.traits.iter().any(|t| {
            t.find_method(lc.as_bytes())
                .is_some_and(|tm| tm.flags & flags::ABSTRACT != 0)
        });
        if !in_parent && !in_iface && !in_trait {
            return Some((
                format!("{display}::{}() has #[\\Override] attribute, but no matching parent method exists", m.name),
                uf.line_start,
            ));
        }
    }
    for c in &cls.consts {
        if c.class.as_bytes() != cls.name.as_bytes() || !has_attr(&c.attributes, b"override") {
            continue;
        }
        let in_parent = parent
            .and_then(|p| p.find_const(c.name.as_bytes()))
            .is_some_and(|pc| pc.flags & flags::PRIVATE == 0);
        let in_iface = cls
            .interfaces
            .iter()
            .any(|i| i.find_const(c.name.as_bytes()).is_some());
        if !in_parent && !in_iface {
            return Some((
                format!(
                    "{full}::{} has #[\\Override] attribute, but no matching parent constant exists",
                    c.name.to_string_lossy()
                ),
                0,
            ));
        }
    }
    for p in &cls.props {
        if p.class.as_bytes() != cls.name.as_bytes() {
            continue;
        }
        let parent_prop = parent
            .and_then(|pc| pc.prop_map.get(&p.name))
            .filter(|pp| pp.visibility() != flags::PRIVATE)
            .cloned()
            .or_else(|| {
                cls.interfaces
                    .iter()
                    .find_map(|i| i.prop_map.get(&p.name).cloned())
            });
        if has_attr(&p.attributes, b"override") && parent_prop.is_none() {
            return Some((
                format!(
                    "{full}::${} has #[\\Override] attribute, but no matching parent property exists",
                    p.name.to_string_lossy()
                ),
                0,
            ));
        }
        let Some(h) = &p.hooks else { continue };
        for (kind, f) in [("get", &h.get), ("set", &h.set)] {
            let Some(f) = f else { continue };
            if !has_attr(&f.attributes, b"override") {
                continue;
            }
            // `property_has_operation`.
            let has_op = parent_prop.as_ref().is_some_and(|pp| {
                let ph = pp.hooks.as_ref();
                if kind == "get" {
                    !pp.is_virtual() || ph.is_some_and(|x| x.get.is_some() || x.abstract_get)
                } else {
                    (!pp.is_virtual() && !pp.is_readonly())
                        || ph.is_some_and(|x| x.set.is_some() || x.abstract_set)
                }
            });
            if !has_op {
                return Some((
                    format!(
                        "{display}::${}::{kind}() has #[\\Override] attribute, but no matching parent method exists",
                        p.name.to_string_lossy()
                    ),
                    f.line_start,
                ));
            }
        }
    }
    // Static properties live outside `props`; their attributes are on the
    // declaration.
    if let Some(decl) = &cls.decl {
        for pd in &decl.props {
            if pd.flags & flags::STATIC == 0 || !has_attr(&pd.attributes, b"override") {
                continue;
            }
            let in_parent = parent
                .and_then(|pc| pc.static_map.get(&pd.name).map(|&i| &pc.static_props[i]))
                .is_some_and(|sp| sp.flags & flags::PRIVATE == 0);
            if !in_parent {
                return Some((
                    format!(
                        "{full}::${} has #[\\Override] attribute, but no matching parent property exists",
                        pd.name.to_string_lossy()
                    ),
                    0,
                ));
            }
        }
    }
    None
}

/// Whether a child `__construct` gets a prototype from `parent`'s
/// constructor: the parent's (or an ancestor's) constructor is abstract.
fn ctor_has_abstract_proto(parent: Option<&Rc<Class>>) -> bool {
    use crate::func::flags;
    let mut cur = parent.cloned();
    while let Some(c) = cur {
        if c.methods
            .get(b"__construct".as_slice())
            .is_some_and(|m| m.flags & flags::ABSTRACT != 0)
        {
            return true;
        }
        cur = c.parent.clone();
    }
    false
}
