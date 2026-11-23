// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Construction of internal (builtin) classes.

use super::sig::{parse_type, sig_head};
use crate::class::{
    cflags, name_map_with_capacity, AttributeInfo, Class, ClassConst, ConstExpr, InternalKind,
    NameMap, PropInfo, StaticProp,
};
use crate::classlink::build_magic;
use crate::func::{flags, Builtin, BuiltinFn, Func, Method};
use crate::vm::Vm;
use php_runtime::{Value, ZStr};
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::rc::{Rc, Weak};

/// A builder for internal (native) classes.
#[derive(Debug)]
pub struct ClassBuilder {
    name: ZStr,
    parent: Option<Rc<Class>>,
    interfaces: Vec<Rc<Class>>,
    flags: u32,
    consts: Vec<(ZStr, Value, u32, Vec<AttributeInfo>)>,
    props: Vec<(ZStr, u32, Option<&'static str>, Option<Value>)>,
    statics: Vec<(ZStr, u32, Value)>,
    methods: Vec<(&'static str, u32, BuiltinFn)>,
    internal: InternalKind,
    hooks: Option<Rc<crate::ext::ClassHooks>>,
    enum_backing: EnumBacking,
    cases: Vec<(ZStr, Option<Value>)>,
}

/// Whether the class under construction is an enum, and of which kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnumBacking {
    /// Not an enum.
    NotEnum,
    /// A unit enum (`enum Name { case A; }`).
    Unit,
    /// A backed enum, with the type mask of its backing type.
    Backed(u32),
}

impl EnumBacking {
    /// The type mask of a backed enum's backing type.
    fn mask(self) -> Option<u32> {
        if let EnumBacking::Backed(m) = self {
            Some(m)
        } else {
            None
        }
    }
}

fn abstract_stub(
    vm: &mut Vm,
    _a: &mut crate::func::Args,
) -> crate::vm::VmResult<php_runtime::Value> {
    Err(vm.make_throw("Error", "Cannot call abstract method"))
}

/// The native method of stub signature `sig` with flags `fl` (public
/// when no visibility is given; `VARIADIC`/`RETURN_REF` from the stub).
/// Only the name and flags are scanned now; the signature is parsed on
/// first use (from `sig`, or from `owned` when the text is not static).
fn builtin_method(sig: &'static str, owned: Option<&str>, fl: u32, h: BuiltinFn) -> Builtin {
    let text = owned.unwrap_or(sig);
    let s = sig_head(text);
    let mut bfl = fl;
    if s.variadic {
        bfl |= flags::VARIADIC;
    }
    if s.by_ref_return {
        bfl |= flags::RETURN_REF;
    }
    if bfl & flags::VISIBILITY == 0 {
        bfl |= flags::PUBLIC;
    }
    let name = ZStr::from(s.name);
    match owned {
        Some(t) => Builtin::from_stub(name, t, bfl, h),
        None => Builtin::from_static_stub(name, sig, bfl, h),
    }
}

/// A native method of `scope` that is not in its method table: the
/// functions a `get_method` handler hands out from a table of its own
/// (`pdo_hash_methods` in `ext/pdo/pdo_dbh.c` builds them from the driver's
/// `zend_function_entry` list with `func.scope = pdo_dbh_ce`). Served
/// through [`crate::ext::ClassHooks::extra_method`].
pub fn native_method(scope: &Rc<Class>, sig: &str, fl: u32, h: BuiltinFn) -> Rc<Method> {
    let b = builtin_method("", Some(sig), fl, h);
    Rc::new(Method {
        name: b.name.clone(),
        flags: b.flags,
        func: Func::Builtin(Rc::new(b)),
        scope: Rc::downgrade(scope),
        proto_scope: Rc::downgrade(scope),
    })
}

impl ClassBuilder {
    /// Starts a class.
    pub fn new(name: &str) -> Self {
        ClassBuilder {
            name: ZStr::from(name),
            parent: None,
            interfaces: Vec::new(),
            flags: cflags::INTERNAL,
            consts: Vec::new(),
            props: Vec::new(),
            statics: Vec::new(),
            methods: Vec::new(),
            internal: InternalKind::None,
            hooks: None,
            enum_backing: EnumBacking::NotEnum,
            cases: Vec::new(),
        }
    }

    /// Makes the class a unit enum (`enum Name { case A; }`): final,
    /// implements `UnitEnum`, has `$name` and `cases()`.
    #[must_use]
    pub fn unit_enum(mut self) -> Self {
        self.enum_backing = EnumBacking::Unit;
        self
    }

    /// Makes the class a backed enum (`enum Name: int|string`): adds
    /// `BackedEnum`, `$value`, `from()`/`tryFrom()`. `backing` is
    /// `"int"` or `"string"`.
    #[must_use]
    pub fn backed_enum(mut self, backing: &str) -> Self {
        let m = if backing.eq_ignore_ascii_case("int") {
            crate::types::mask::LONG
        } else {
            crate::types::mask::STRING
        };
        self.enum_backing = EnumBacking::Backed(m);
        self
    }

    /// Adds an enum case (`value` is the backing value of a backed enum).
    #[must_use]
    pub fn case(mut self, name: &str, value: Option<Value>) -> Self {
        self.cases.push((ZStr::from(name), value));
        self
    }

    /// Sets the parent class (looked up in the VM).
    #[must_use]
    pub fn extends(mut self, vm: &Vm, parent: &str) -> Self {
        self.parent = vm.find_class(parent.as_bytes());
        self
    }

    /// Adds implemented interfaces.
    #[must_use]
    pub fn implements(mut self, vm: &Vm, names: &[&str]) -> Self {
        for n in names {
            if let Some(c) = vm.find_class(n.as_bytes()) {
                self.interfaces.push(c);
            }
        }
        self
    }

    /// Adds class flags.
    #[must_use]
    pub fn flags(mut self, f: u32) -> Self {
        self.flags |= f;
        self
    }

    /// Sets native object handlers.
    #[must_use]
    pub fn hooks(mut self, h: crate::ext::ClassHooks) -> Self {
        self.hooks = Some(Rc::new(h));
        self
    }

    /// Sets the internal kind.
    pub(crate) fn internal(mut self, k: InternalKind) -> Self {
        self.internal = k;
        self
    }

    /// Adds a constant.
    #[must_use]
    pub fn constant(mut self, name: &str, v: Value) -> Self {
        self.consts
            .push((ZStr::from(name), v, flags::PUBLIC, Vec::new()));
        self
    }

    /// Adds a `#[\Deprecated]` constant: reading it emits
    /// `Deprecated: Constant C::NAME is deprecated since X, message`
    /// (`since`/`message` may be empty).
    #[must_use]
    pub fn deprecated_constant(mut self, name: &str, v: Value, since: &str, message: &str) -> Self {
        let mut args = Vec::new();
        if !message.is_empty() {
            args.push((
                Some(ZStr::from("message")),
                ConstExpr::Value(Value::Str(ZStr::from(message))),
            ));
        }
        if !since.is_empty() {
            args.push((
                Some(ZStr::from("since")),
                ConstExpr::Value(Value::Str(ZStr::from(since))),
            ));
        }
        let attr = AttributeInfo {
            name: ZStr::from("Deprecated"),
            args,
            line: 0,
            strict: false,
        };
        self.consts
            .push((ZStr::from(name), v, flags::PUBLIC, vec![attr]));
        self
    }

    /// Adds a property: `ty` is a stub type or `None`; `default` `None`
    /// leaves typed properties uninitialized.
    #[must_use]
    pub fn prop(
        mut self,
        name: &str,
        fl: u32,
        ty: Option<&'static str>,
        default: Option<Value>,
    ) -> Self {
        self.props.push((ZStr::from(name), fl, ty, default));
        self
    }

    /// Adds a static property.
    #[must_use]
    pub fn static_prop(mut self, name: &str, fl: u32, v: Value) -> Self {
        self.statics.push((ZStr::from(name), fl | flags::STATIC, v));
        self
    }

    /// Adds a method with a stub signature.
    #[must_use]
    pub fn method(mut self, sig: &'static str, fl: u32, f: BuiltinFn) -> Self {
        self.methods.push((sig, fl, f));
        self
    }

    /// Adds an abstract (interface) method.
    #[must_use]
    pub fn abstract_method(mut self, sig: &'static str, fl: u32) -> Self {
        self.methods
            .push((sig, fl | flags::ABSTRACT, abstract_stub));
        self
    }

    /// Builds and registers the class.
    #[allow(clippy::too_many_lines)]
    pub fn register(mut self, vm: &mut Vm) -> Rc<Class> {
        if self.enum_backing != EnumBacking::NotEnum {
            let backing = self.enum_backing.mask();
            self.flags |= cflags::ENUM | cflags::FINAL;
            let mut ifaces = Vec::new();
            ifaces.extend(vm.find_class(b"UnitEnum"));
            if backing.is_some() {
                ifaces.extend(vm.find_class(b"BackedEnum"));
            }
            ifaces.append(&mut self.interfaces);
            self.interfaces = ifaces;
            let mut props = vec![(
                ZStr::from("name"),
                flags::PUBLIC | flags::READONLY,
                Some("string"),
                None,
            )];
            if let Some(b) = backing {
                let ty = if b == crate::types::mask::LONG {
                    "int"
                } else {
                    "string"
                };
                props.push((
                    ZStr::from("value"),
                    flags::PUBLIC | flags::READONLY,
                    Some(ty),
                    None,
                ));
            }
            props.append(&mut self.props);
            self.props = props;
            for (i, (sig, fl, h)) in super::enum_methods().into_iter().enumerate() {
                if i > 0 && backing.is_none() {
                    break;
                }
                self.methods.push((sig, fl, h));
            }
        }
        let parent = self.parent.clone();
        let mut interfaces: Vec<Rc<Class>> = parent
            .as_ref()
            .map(|p| p.interfaces.clone())
            .unwrap_or_default();
        for i in &self.interfaces {
            for sub in &i.interfaces {
                if !interfaces.iter().any(|x| Rc::ptr_eq(x, sub)) {
                    interfaces.push(sub.clone());
                }
            }
            if !interfaces.iter().any(|x| Rc::ptr_eq(x, i)) {
                interfaces.push(i.clone());
            }
        }
        let name = self.name.clone();
        let internal = if self.internal == InternalKind::None {
            parent.as_ref().map_or(InternalKind::None, |p| p.internal)
        } else {
            self.internal
        };
        let mut cflag = self.flags;
        if let Some(p) = &parent {
            cflag |= p.flags & cflags::ALLOW_DYNAMIC;
        }
        // Upper bounds of the table sizes (inherited entries may be
        // overridden), so the tables are built without regrowing.
        let nconsts = parent.as_ref().map_or(0, |p| p.consts.len())
            + interfaces.iter().map(|i| i.consts.len()).sum::<usize>()
            + self.consts.len()
            + self.cases.len();
        let nmethods = parent.as_ref().map_or(0, |p| p.method_order.len())
            + interfaces
                .iter()
                .map(|i| i.method_order.len())
                .sum::<usize>()
            + self.methods.len();
        // The parent's interfaces head `interfaces`; their constants and
        // methods are already in the parent's tables (copied first), so
        // only the interfaces new to this class are merged
        // (`do_implement_interface` skips inherited ones the same way).
        let new_ifaces = &interfaces[parent.as_ref().map_or(0, |p| p.interfaces.len())..];
        let cls = Rc::new_cyclic(|weak: &Weak<Class>| {
            // Constants.
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: members of the declared class"
            )]
            let mut consts: Vec<Rc<ClassConst>> = Vec::with_capacity(nconsts);
            let mut const_map = name_map_with_capacity(nconsts);
            let push_const =
                |c: Rc<ClassConst>, consts: &mut Vec<Rc<ClassConst>>, map: &mut NameMap<usize>| {
                    match map.entry(c.name.clone()) {
                        Entry::Occupied(e) => consts[*e.get()] = c,
                        Entry::Vacant(e) => {
                            e.insert(consts.len());
                            consts.push(c);
                        }
                    }
                };
            if let Some(p) = &parent {
                for c in &p.consts {
                    push_const(c.clone(), &mut consts, &mut const_map);
                }
            }
            for i in new_ifaces {
                for c in &i.consts {
                    if let Entry::Vacant(e) = const_map.entry(c.name.clone()) {
                        e.insert(consts.len());
                        consts.push(c.clone());
                    }
                }
            }
            for (n, v, f, attrs) in &self.consts {
                push_const(
                    Rc::new(ClassConst {
                        name: n.clone(),
                        value: RefCell::new(Some(v.clone())),
                        expr: ConstExpr::Value(v.clone()),
                        flags: *f,
                        class: name.clone(),
                        ty: None,
                        is_case: false,
                        evaluating: Cell::new(false),
                        doc_comment: None,
                        attributes: attrs.clone(),
                    }),
                    &mut consts,
                    &mut const_map,
                );
            }
            for (n, v) in &self.cases {
                push_const(
                    Rc::new(ClassConst {
                        name: n.clone(),
                        value: RefCell::new(None),
                        expr: ConstExpr::Value(v.clone().unwrap_or(Value::Null)),
                        flags: flags::PUBLIC,
                        class: name.clone(),
                        ty: None,
                        is_case: true,
                        evaluating: Cell::new(false),
                        doc_comment: None,
                        attributes: Vec::new(),
                    }),
                    &mut consts,
                    &mut const_map,
                );
            }
            // Properties.
            let mut props: Vec<Rc<PropInfo>> =
                parent.as_ref().map(|p| p.props.clone()).unwrap_or_default();
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: members of the declared class"
            )]
            props.reserve(self.props.len());
            let mut prop_map: NameMap<Rc<PropInfo>> = name_map_with_capacity(
                parent.as_ref().map_or(0, |p| p.prop_map.len()) + self.props.len(),
            );
            if let Some(p) = &parent {
                for (k, v) in &p.prop_map {
                    if v.visibility() != flags::PRIVATE {
                        prop_map.insert(k.clone(), v.clone());
                    }
                }
            }
            for (n, f, ty, def) in &self.props {
                let mut f = *f;
                if f & (flags::PUBLIC | flags::READONLY | flags::SET_MASK)
                    == flags::PUBLIC | flags::READONLY
                {
                    f |= flags::SET_PROTECTED;
                }
                let pn = n.clone();
                let slot = match prop_map.get(&pn) {
                    Some(e) => e.slot,
                    None => u32::try_from(props.len()).unwrap_or(0),
                };
                let pi = Rc::new(PropInfo {
                    name: pn.clone(),
                    slot,
                    flags: f,
                    ty: ty.map(parse_type),
                    class: name.clone(),
                    default: def.clone().map(ConstExpr::Value),
                    doc_comment: None,
                    attributes: Vec::new(),
                    hooks: None,
                });
                if (slot as usize) < props.len() {
                    props[slot as usize] = pi.clone();
                } else {
                    props.push(pi.clone());
                }
                prop_map.insert(pn, pi);
            }
            let mut static_props: Vec<StaticProp> = parent
                .as_ref()
                .map(|p| p.static_props.clone())
                .unwrap_or_default();
            let mut static_map: NameMap<usize> = parent
                .as_ref()
                .map(|p| p.static_map.clone())
                .unwrap_or_default();
            static_map.reserve(self.statics.len());
            for (n, f, v) in &self.statics {
                let sp = StaticProp {
                    name: n.clone(),
                    flags: *f,
                    ty: None,
                    class: name.clone(),
                    cell: Rc::new(RefCell::new(v.clone())),
                    default: None,
                    initialized: Rc::new(Cell::new(true)),
                    doc_comment: None,
                };
                static_map.insert(sp.name.clone(), static_props.len());
                static_props.push(sp);
            }
            // Methods.
            let mut methods: NameMap<Rc<Method>> = name_map_with_capacity(nmethods);
            #[expect(
                clippy::disallowed_methods,
                reason = "bounded: members of the declared class"
            )]
            let mut order: Vec<ZStr> = Vec::with_capacity(nmethods);
            for (sig, f, h) in &self.methods {
                let b = builtin_method(*sig, None, *f, *h);
                let bfl = b.flags;
                let lc = b.name.to_ascii_lowercase();
                let proto = parent
                    .as_ref()
                    .and_then(|p| p.methods.get(&lc))
                    .map_or_else(|| weak.clone(), |m| m.proto_scope.clone());
                order.push(lc.clone());
                methods.insert(
                    lc,
                    Rc::new(Method {
                        name: b.name.clone(),
                        func: Func::Builtin(Rc::new(b)),
                        flags: bfl,
                        scope: weak.clone(),
                        proto_scope: proto,
                    }),
                );
            }
            // Inherited methods the class does not override, in the
            // parent's and then each interface's declaration order.
            for from in parent.iter().chain(new_ifaces.iter()) {
                for lc in &from.method_order {
                    if let Entry::Vacant(e) = methods.entry(lc.clone()) {
                        if let Some(m) = from.methods.get(lc) {
                            order.push(lc.clone());
                            e.insert(m.clone());
                        }
                    }
                }
            }
            let magic = build_magic(&methods);
            Class {
                lc_name: name.to_ascii_lowercase(),
                name: name.clone(),
                flags: cflag,
                parent: parent.clone(),
                interfaces: interfaces.clone(),
                traits: Vec::new(),
                consts,
                const_map,
                props,
                prop_map,
                static_props,
                static_map,
                methods,
                method_order: order,
                magic,
                defaults: RefCell::new(None),
                backing: self.enum_backing.mask(),
                enum_cases: RefCell::new(NameMap::default()),
                decl: None,
                internal,
                attributes: Vec::new(),
                filename: None,
                line_start: 0,
                line_end: 0,
                doc_comment: None,
                linked: Cell::new(true),
                hooks: self
                    .hooks
                    .clone()
                    .or_else(|| parent.as_ref().and_then(|p| p.hooks.clone())),
            }
        });
        vm.register_class(cls.clone());
        cls
    }
}

#[cfg(test)]
mod tests {
    use super::ClassBuilder;
    use crate::class::{cflags, Class};
    use crate::func::{flags, Args};
    use crate::vm::{Vm, VmResult};
    use crate::VmOptions;
    use php_runtime::{Value, ZStr};

    #[allow(clippy::unnecessary_wraps)] // builtin signature
    fn nop(_vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
        Ok(Value::Null)
    }

    fn vm() -> Vm {
        Vm::new(&VmOptions::default(), Box::new(std::io::sink()))
    }

    /// The magic methods `build_magic` found, by name.
    fn magic(c: &Class) -> Vec<&'static str> {
        let m = &c.magic;
        [
            ("__construct", m.construct.is_some()),
            ("__destruct", m.destruct.is_some()),
            ("__get", m.get.is_some()),
            ("__set", m.set.is_some()),
            ("__isset", m.isset.is_some()),
            ("__unset", m.unset.is_some()),
            ("__call", m.call.is_some()),
            ("__callStatic", m.call_static.is_some()),
            ("__toString", m.to_string.is_some()),
            ("__invoke", m.invoke.is_some()),
            ("__clone", m.clone.is_some()),
            ("__debugInfo", m.debug_info.is_some()),
            ("__serialize", m.serialize.is_some()),
            ("__unserialize", m.unserialize.is_some()),
            ("__sleep", m.sleep.is_some()),
            ("__wakeup", m.wakeup.is_some()),
        ]
        .into_iter()
        .filter_map(|(n, found)| found.then_some(n))
        .collect()
    }

    fn with_magic(b: ClassBuilder) -> ClassBuilder {
        b.method("__construct(int $x = 0)", flags::PUBLIC, nop)
            .method("__toString(): string", flags::PUBLIC, nop)
            .method(
                "__callStatic(string $name, array $arguments): mixed",
                flags::STATIC,
                nop,
            )
            .method("__unserialize(array $data): void", flags::PUBLIC, nop)
            .method("__notMagic(): void", flags::PUBLIC, nop)
            .method("other(): void", flags::PUBLIC, nop)
    }

    #[test]
    fn magic_methods_of_small_and_large_method_tables_agree() {
        let mut vm = vm();
        let small = with_magic(ClassBuilder::new("MagicSmall")).register(&mut vm);
        let mut large = with_magic(ClassBuilder::new("MagicLarge"));
        for i in 0..40 {
            let sig: &'static str = Box::leak(format!("m{i}(): void").into_boxed_str());
            large = large.method(sig, flags::PUBLIC, nop);
        }
        let large = large.register(&mut vm);
        assert!(small.methods.len() <= 32 && large.methods.len() > 32);
        let expected = ["__construct", "__callStatic", "__toString", "__unserialize"];
        assert_eq!(magic(&small), expected);
        assert_eq!(magic(&large), expected);
        let ctor = small.magic.construct.as_ref().expect("constructor");
        assert!(std::rc::Rc::ptr_eq(
            ctor,
            &small.methods[b"__construct".as_slice()]
        ));
    }

    #[test]
    fn inherited_tables_keep_declaration_order_and_overrides() {
        let mut vm = vm();
        ClassBuilder::new("BaseT")
            .constant("A", Value::Int(1))
            .constant("B", Value::Int(2))
            .method("first(): void", flags::PUBLIC, nop)
            .method("__toString(): string", flags::PUBLIC, nop)
            .method("second(): void", flags::PUBLIC, nop)
            .register(&mut vm);
        ClassBuilder::new("IfaceT")
            .flags(cflags::INTERFACE | cflags::ABSTRACT)
            .constant("A", Value::Int(3))
            .constant("C", Value::Int(4))
            .abstract_method("second(): void", flags::PUBLIC)
            .abstract_method("third(): void", flags::PUBLIC)
            .register(&mut vm);
        let child = ClassBuilder::new("ChildT")
            .extends(&vm, "BaseT")
            .implements(&vm, &["IfaceT"])
            .constant("B", Value::Int(20))
            .constant("D", Value::Int(5))
            .method("own(): void", flags::PUBLIC, nop)
            .method("first(): void", flags::PUBLIC, nop)
            .register(&mut vm);
        let consts: Vec<(Vec<u8>, i64)> = child
            .consts
            .iter()
            .map(|c| {
                let v = match &*c.value.borrow() {
                    Some(Value::Int(i)) => *i,
                    _ => -1,
                };
                (c.name.as_bytes().to_vec(), v)
            })
            .collect();
        // The parent's constants, then the interface's new ones; own ones
        // replace inherited ones in place.
        assert_eq!(
            consts,
            [
                (b"A".to_vec(), 1),
                (b"B".to_vec(), 20),
                (b"C".to_vec(), 4),
                (b"D".to_vec(), 5)
            ]
        );
        for (i, c) in child.consts.iter().enumerate() {
            assert_eq!(child.const_map[c.name.as_bytes()], i);
        }
        let order: Vec<&[u8]> = child.method_order.iter().map(ZStr::as_bytes).collect();
        let expected: [&[u8]; 5] = [b"own", b"first", b"__tostring", b"second", b"third"];
        assert_eq!(order, expected);
        assert_eq!(child.methods.len(), 5);
        assert_eq!(magic(&child), ["__toString"]);
        let base = vm.find_class(b"BaseT").expect("BaseT");
        assert!(std::rc::Rc::ptr_eq(
            &child.methods[b"second".as_slice()],
            &base.methods[b"second".as_slice()]
        ));
    }

    #[test]
    fn grandchild_inherits_interface_members_through_its_parent() {
        let mut vm = vm();
        ClassBuilder::new("IfaceG")
            .flags(cflags::INTERFACE | cflags::ABSTRACT)
            .constant("K", Value::Int(1))
            .abstract_method("iface(): void", flags::PUBLIC)
            .register(&mut vm);
        ClassBuilder::new("IfaceH")
            .flags(cflags::INTERFACE | cflags::ABSTRACT)
            .constant("L", Value::Int(2))
            .abstract_method("other(): void", flags::PUBLIC)
            .register(&mut vm);
        let parent = ClassBuilder::new("ParentG")
            .implements(&vm, &["IfaceG"])
            .constant("P", Value::Int(3))
            .method("own(): void", flags::PUBLIC, nop)
            .register(&mut vm);
        let child = ClassBuilder::new("ChildG")
            .extends(&vm, "ParentG")
            .implements(&vm, &["IfaceH"])
            .method("iface(): void", flags::PUBLIC, nop)
            .register(&mut vm);
        let names = |c: &crate::class::Class| -> Vec<Vec<u8>> {
            c.consts
                .iter()
                .map(|k| k.name.as_bytes().to_vec())
                .collect()
        };
        assert_eq!(names(&parent), [b"K".to_vec(), b"P".to_vec()]);
        assert_eq!(names(&child), [b"K".to_vec(), b"P".to_vec(), b"L".to_vec()]);
        let order: Vec<&[u8]> = child.method_order.iter().map(ZStr::as_bytes).collect();
        let expected: [&[u8]; 3] = [b"iface", b"own", b"other"];
        assert_eq!(order, expected);
        let ifaces: Vec<&[u8]> = child.interfaces.iter().map(|i| i.name.as_bytes()).collect();
        let expected: [&[u8]; 2] = [b"IfaceG", b"IfaceH"];
        assert_eq!(ifaces, expected);
    }
}
