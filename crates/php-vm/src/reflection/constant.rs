// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ReflectionClassConstant`, `ReflectionEnumUnitCase`,
//! `ReflectionEnumBackedCase` and `ReflectionConstant`.

use super::{
    class_value, rexc, set_name_props, str_arg, this, this_refl, with_scope, ConstRef, Refl, PRIV,
    PUB, RO,
};
use crate::builtins::classbuilder::ClassBuilder;
use crate::class::{cflags, Class, ClassConst};
use crate::func::{flags, Args};
use crate::object;
use crate::vm::{Vm, VmResult};
use php_runtime::{Obj, Value, ZStr};
use std::rc::Rc;

/// The declaring class of a constant.
fn decl_of(vm: &Vm, cls: &Rc<Class>, c: &ClassConst) -> Rc<Class> {
    vm.find_class(c.class.as_bytes())
        .unwrap_or_else(|| cls.clone())
}

/// Evaluates a class constant (`zend_update_class_constant`).
pub(crate) fn const_value_of(vm: &mut Vm, cls: &Rc<Class>, c: &Rc<ClassConst>) -> VmResult<Value> {
    let decl = decl_of(vm, cls, c);
    let name = c.name.clone();
    if c.is_case {
        return vm.enum_case(&decl, &name);
    }
    with_scope(vm, Some(decl.clone()), |vm| vm.class_constant(&decl, &name))
}

/// The backing value of an enum case object.
pub(crate) fn backing_value(v: &Value) -> Value {
    if let Value::Object(o) = v {
        let cls = object::class_of(o).clone();
        if let Some(pi) = cls.prop_map.get(&ZStr::from("value")) {
            return object::data(o)
                .slots
                .borrow()
                .get(pi.slot as usize)
                .cloned()
                .flatten()
                .unwrap_or_default();
        }
    }
    Value::Null
}

/// `reflection_class_constant_factory` / `reflection_enum_case_factory`.
pub(crate) fn class_constant_value(
    vm: &mut Vm,
    cls: &Rc<Class>,
    c: &Rc<ClassConst>,
    as_case: bool,
) -> VmResult<Value> {
    let decl = decl_of(vm, cls, c);
    let cn = if as_case {
        if decl.backing.is_some() {
            "ReflectionEnumBackedCase"
        } else {
            "ReflectionEnumUnitCase"
        }
    } else {
        "ReflectionClassConstant"
    };
    super::new_refl(
        vm,
        cn,
        Refl::Const {
            class: decl.clone(),
            c: c.clone(),
        },
        Some(&c.name),
        Some(&decl.name),
    )
}

fn this_const(vm: &mut Vm, a: &Args) -> VmResult<(Rc<Class>, Rc<ClassConst>)> {
    match this_refl(vm, a)? {
        Refl::Const { class, c } => Ok((class, c)),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn rcc_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let c = match a.at(0) {
        Value::Object(x) => object::class_of(&x).clone(),
        v => {
            let s = vm.to_zstr(&v)?;
            match vm.lookup_class(&s)? {
                Some(c) => c,
                None => {
                    return Err(rexc(
                        vm,
                        &format!("Class \"{}\" does not exist", s.to_string_lossy()),
                    ))
                }
            }
        }
    };
    let n = str_arg(a, 1);
    let Some(cc) = c.find_const(n.as_bytes()).cloned() else {
        return Err(rexc(
            vm,
            &format!(
                "Constant {}::{} does not exist",
                c.name,
                n.to_string_lossy()
            ),
        ));
    };
    let decl = decl_of(vm, &c, &cc);
    set_name_props(&o, Some(&n), Some(&decl.name));
    object::set_internal(&o, Refl::Const { class: decl, c: cc });
    Ok(Value::Null)
}

fn rcc_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, cc) = this_const(vm, a)?;
    let s = super::export::class_const_string(vm, &c, &cc, "")?;
    Ok(Value::Str(ZStr::from(s)))
}

fn rcc_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(this_const(vm, a)?.1.name.clone()))
}
fn rcc_get_value(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, cc) = this_const(vm, a)?;
    const_value_of(vm, &c, &cc)
}
fn rcc_get_declaring_class(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, _) = this_const(vm, a)?;
    class_value(vm, &c)
}
fn rcc_get_modifiers(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Int(i64::from(this_const(vm, a)?.1.flags & 0x27)))
}

macro_rules! rcc_flag {
    ($name:ident, $mask:expr) => {
        fn $name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            Ok(Value::Bool(this_const(vm, a)?.1.flags & $mask != 0))
        }
    };
}

rcc_flag!(rcc_is_public, flags::PUBLIC);
rcc_flag!(rcc_is_private, flags::PRIVATE);
rcc_flag!(rcc_is_protected, flags::PROTECTED);
rcc_flag!(rcc_is_final, flags::FINAL);

fn rcc_is_deprecated(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cc) = this_const(vm, a)?;
    Ok(Value::Bool(
        cc.flags & flags::DEPRECATED != 0
            || cc
                .attributes
                .iter()
                .any(|at| at.name.eq_ignore_ascii_case(b"Deprecated")),
    ))
}
fn rcc_is_enum_case(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(this_const(vm, a)?.1.is_case))
}
/// Attributes of enum cases (not kept by the compiler): literal arguments
/// re-read from the source.
fn case_attributes(vm: &Vm, c: &Class, cc: &ClassConst) -> Vec<crate::class::AttributeInfo> {
    let Some(file) = c.filename.clone() else {
        return Vec::new();
    };
    let Some((groups, ns)) = [c.line_start, c.line_end]
        .iter()
        .find_map(|&l| super::source::case_attributes(vm, file.as_bytes(), l, cc.name.as_bytes()))
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for g in &groups {
        for at in &g.attributes {
            let n = &at.name;
            let full = if n.kind == php_parser::ast::NameKind::FullyQualified || ns.is_empty() {
                n.name.clone()
            } else {
                let mut v = ns.clone();
                v.push(b'\\');
                v.extend_from_slice(&n.name);
                v
            };
            let mut args = Vec::new();
            let mut ok = true;
            for arg in at
                .args
                .as_ref()
                .map(|l| l.args.as_slice())
                .unwrap_or_default()
            {
                let (name, e) = match &arg.kind {
                    php_parser::ast::ArgKind::Positional(e) => (None, e),
                    php_parser::ast::ArgKind::Named { name, value } => {
                        (Some(ZStr::from(name.name.as_slice())), value)
                    }
                    _ => {
                        ok = false;
                        break;
                    }
                };
                match super::source::literal_value(e) {
                    Some(v) => args.push((name, crate::class::ConstExpr::Value(v))),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                out.push(crate::class::AttributeInfo {
                    name: ZStr::from(full),
                    args,
                    line: at.span.line,
                    strict: false,
                });
            }
        }
    }
    out
}

fn rcc_get_attributes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, cc) = this_const(vm, a)?;
    let attrs = if cc.is_case && cc.attributes.is_empty() {
        case_attributes(vm, &c, &cc)
    } else {
        cc.attributes.clone()
    };
    super::attribute::attributes_array_in(
        vm,
        &attrs,
        super::target::CLASS_CONSTANT,
        Some(&c),
        c.filename.clone(),
        a,
    )
}
fn rcc_get_doc_comment(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, cc) = this_const(vm, a)?;
    Ok(const_doc_comment(&cc).map_or(Value::Bool(false), Value::Str))
}

/// The doc comment of a constant or enum case, as compiled (none when
/// opcache dropped it under `opcache.save_comments=0`).
pub(crate) fn const_doc_comment(cc: &ClassConst) -> Option<ZStr> {
    cc.doc_comment.clone()
}
fn rcc_get_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, cc) = this_const(vm, a)?;
    match cc.ty.clone() {
        Some(t) => super::types::type_value(vm, &t, Some(&c), true),
        None => Ok(Value::Null),
    }
}
fn rcc_has_type(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(this_const(vm, a)?.1.ty.is_some()))
}

fn case_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    rcc_construct(vm, a)?;
    let (c, cc) = this_const(vm, a)?;
    if !cc.is_case {
        return Err(rexc(
            vm,
            &format!("Constant {}::{} is not a case", c.name, cc.name),
        ));
    }
    Ok(Value::Null)
}

fn backed_case_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    case_construct(vm, a)?;
    let (c, cc) = this_const(vm, a)?;
    if c.backing.is_none() {
        return Err(rexc(
            vm,
            &format!("Enum case {}::{} is not a backed case", c.name, cc.name),
        ));
    }
    Ok(Value::Null)
}

fn rcase_get_enum(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, _) = this_const(vm, a)?;
    class_value(vm, &c)
}

fn rcase_get_backing_value(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (c, cc) = this_const(vm, a)?;
    let v = vm.enum_case(&c, &cc.name)?;
    Ok(backing_value(&v))
}

fn clone_error(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(rexc(vm, "Cannot clone object using __clone()"))
}

// ---------------------------------------------------------------------
// ReflectionConstant
// ---------------------------------------------------------------------

/// Finds a global constant (namespace part case-insensitive).
fn find_constant(vm: &Vm, name: &[u8]) -> Option<(ZStr, Value)> {
    let src = name.strip_prefix(b"\\").unwrap_or(name);
    if let Some((k, v)) = vm.constants.get_key_value(src) {
        return Some((k.clone(), v.clone()));
    }
    let ns_end = src.iter().rposition(|&c| c == b'\\');
    let (ns, short) = match ns_end {
        Some(p) => (&src[..p], &src[p..]),
        None => (&src[..0], src),
    };
    let mut lc = ns.to_ascii_lowercase();
    lc.extend_from_slice(short);
    if let Some((k, v)) = vm.constants.get_key_value(lc.as_slice()) {
        return Some((k.clone(), v.clone()));
    }
    if ns_end.is_none() {
        let up = src.to_ascii_uppercase();
        if matches!(up.as_slice(), b"TRUE" | b"FALSE" | b"NULL") {
            let v = match up.as_slice() {
                b"TRUE" => Value::Bool(true),
                b"FALSE" => Value::Bool(false),
                _ => Value::Null,
            };
            return Some((ZStr::from(src.to_ascii_lowercase()), v));
        }
    }
    vm.constants
        .iter()
        .find(|(k, _)| {
            let kb = k.as_bytes();
            kb.len() == src.len() && {
                let kp = kb.iter().rposition(|&c| c == b'\\');
                kp == ns_end
                    && kb[..kp.unwrap_or(0)].eq_ignore_ascii_case(&src[..ns_end.unwrap_or(0)])
                    && kb[kp.unwrap_or(0)..] == src[ns_end.unwrap_or(0)..]
            }
        })
        .map(|(k, v)| (k.clone(), v.clone()))
}

fn this_constant(vm: &mut Vm, a: &Args) -> VmResult<ConstRef> {
    match this_refl(vm, a)? {
        Refl::Constant(c) => Ok(c),
        _ => Err(vm.make_throw(
            "Error",
            "Internal error: Failed to retrieve the reflection object",
        )),
    }
}

fn is_user_constant(vm: &Vm, name: &ZStr) -> bool {
    vm.user_constants.iter().any(|n| n == name)
}

fn rk_construct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let o = this(a);
    let n = str_arg(a, 0);
    let Some((name, value)) = find_constant(vm, n.as_bytes()) else {
        return Err(rexc(
            vm,
            &format!("Constant \"{}\" does not exist", n.to_string_lossy()),
        ));
    };
    set_name_props(&o, Some(&n), None);
    object::set_internal(&o, Refl::Constant(ConstRef { name, value }));
    Ok(Value::Null)
}

fn rk_get_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(this_constant(vm, a)?.name))
}
fn rk_in_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Bool(super::in_ns(&this_constant(vm, a)?.name)))
}
fn rk_get_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(super::ns_name(&this_constant(vm, a)?.name)))
}
fn rk_get_short_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::Str(super::short_name(&this_constant(vm, a)?.name)))
}
fn rk_get_value(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(this_constant(vm, a)?.value)
}
fn rk_is_deprecated(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    Ok(Value::Bool(super::misc::constant_deprecated(vm, &c.name)))
}
fn rk_get_file_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    Ok(super::misc::constant_file(vm, &c.name).map_or(Value::Bool(false), Value::Str))
}
fn rk_get_extension(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    if is_user_constant(vm, &c.name) {
        return Ok(Value::Null);
    }
    let m = super::misc::constant_module(vm, &c.name);
    super::misc::extension_value(vm, &m)
}
fn rk_get_extension_name(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    if is_user_constant(vm, &c.name) {
        return Ok(Value::Bool(false));
    }
    Ok(super::zs(&super::misc::constant_module(vm, &c.name)))
}
fn rk_get_attributes(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    let mut attrs = vm
        .const_attributes
        .get(&c.name)
        .map(|x| (**x).clone())
        .unwrap_or_default();
    if attrs.is_empty() && super::misc::constant_deprecated(vm, &c.name) {
        // Deprecated internal constants carry `#[\Deprecated]`.
        attrs.push(crate::class::AttributeInfo {
            name: ZStr::from("Deprecated"),
            args: Vec::new(),
            line: 0,
            strict: false,
        });
    }
    let file = vm.const_files.get(&c.name).cloned();
    super::attribute::attributes_array_in(vm, &attrs, super::target::CONSTANT, None, file, a)
}
fn rk_to_string(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let c = this_constant(vm, a)?;
    let mut fl: Vec<&str> = Vec::new();
    if !is_user_constant(vm, &c.name) {
        fl.push("persistent");
    }
    if super::misc::constant_deprecated(vm, &c.name) {
        fl.push("deprecated");
    }
    let s = super::export::const_string(vm, &c.name, &c.value, &fl, "")?;
    Ok(Value::Str(ZStr::from(s)))
}

/// `_reflection_write_property`: `$name`/`$class` cannot be written.
pub(crate) fn write_guard(vm: &mut Vm, o: &Obj, name: &ZStr, _v: &Value) -> VmResult<bool> {
    let cls = object::class_of(o).clone();
    if (name.as_bytes() == b"name" || name.as_bytes() == b"class")
        && cls.prop_map.contains_key(name)
    {
        let cn = super::dn(&cls.name);
        return Err(rexc(
            vm,
            &format!(
                "Cannot set read-only property {cn}::${}",
                name.to_string_lossy()
            ),
        ));
    }
    Ok(false)
}

/// Object handlers shared by the reflection classes.
pub(crate) fn hooks() -> crate::ext::ClassHooks {
    crate::ext::ClassHooks {
        write_property: Some(write_guard),
        ..Default::default()
    }
}

/// Registers `ReflectionClassConstant`.
pub(crate) fn register_class_constant(vm: &mut Vm) {
    ClassBuilder::new("ReflectionClassConstant")
        .implements(vm, &["Reflector"])
        .hooks(super::constant::hooks())
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .prop("name", RO, Some("string"), None)
        .prop("class", RO, Some("string"), None)
        .constant("IS_PUBLIC", Value::Int(1))
        .constant("IS_PROTECTED", Value::Int(2))
        .constant("IS_PRIVATE", Value::Int(4))
        .constant("IS_FINAL", Value::Int(32))
        .method("__clone(): void", PRIV, clone_error)
        .method(
            "__construct(object|string $class, string $constant)",
            PUB,
            rcc_construct,
        )
        .method("__toString(): string", PUB, rcc_to_string)
        .method("getName(): string", PUB, rcc_get_name)
        .method("getValue(): mixed", PUB, rcc_get_value)
        .method("isPublic(): bool", PUB, rcc_is_public)
        .method("isPrivate(): bool", PUB, rcc_is_private)
        .method("isProtected(): bool", PUB, rcc_is_protected)
        .method("isFinal(): bool", PUB, rcc_is_final)
        .method("getModifiers(): int", PUB, rcc_get_modifiers)
        .method(
            "getDeclaringClass(): ReflectionClass",
            PUB,
            rcc_get_declaring_class,
        )
        .method("getDocComment(): string|false", PUB, rcc_get_doc_comment)
        .method(
            "getAttributes(?string $name = null, int $flags = 0): array",
            PUB,
            rcc_get_attributes,
        )
        .method("isEnumCase(): bool", PUB, rcc_is_enum_case)
        .method("isDeprecated(): bool", PUB, rcc_is_deprecated)
        .method("hasType(): bool", PUB, rcc_has_type)
        .method("getType(): ?ReflectionType", PUB, rcc_get_type)
        .register(vm);
}

/// Registers the enum case classes.
pub(crate) fn register_cases(vm: &mut Vm) {
    ClassBuilder::new("ReflectionEnumUnitCase")
        .extends(vm, "ReflectionClassConstant")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method(
            "__construct(object|string $class, string $constant)",
            PUB,
            case_construct,
        )
        .method("getEnum(): ReflectionEnum", PUB, rcase_get_enum)
        .method("getValue(): UnitEnum", PUB, rcc_get_value)
        .register(vm);
    ClassBuilder::new("ReflectionEnumBackedCase")
        .extends(vm, "ReflectionEnumUnitCase")
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .method(
            "__construct(object|string $class, string $constant)",
            PUB,
            backed_case_construct,
        )
        .method(
            "getBackingValue(): int|string",
            PUB,
            rcase_get_backing_value,
        )
        .register(vm);
}

/// Registers `ReflectionConstant`.
pub(crate) fn register_constant(vm: &mut Vm) {
    ClassBuilder::new("ReflectionConstant")
        .implements(vm, &["Reflector"])
        .hooks(super::constant::hooks())
        .flags(cflags::NOT_SERIALIZABLE | cflags::NO_CLONE)
        .prop("name", RO, Some("string"), None)
        .method("__construct(string $name)", PUB, rk_construct)
        .method("getName(): string", PUB, rk_get_name)
        .method("inNamespace(): bool", PUB, rk_in_ns)
        .method("getNamespaceName(): string", PUB, rk_get_ns)
        .method("getShortName(): string", PUB, rk_get_short_name)
        .method("getValue(): mixed", PUB, rk_get_value)
        .method("isDeprecated(): bool", PUB, rk_is_deprecated)
        .method("getFileName(): string|false", PUB, rk_get_file_name)
        .method(
            "getExtension(): ?ReflectionExtension",
            PUB,
            rk_get_extension,
        )
        .method(
            "getExtensionName(): string|false",
            PUB,
            rk_get_extension_name,
        )
        .method("__toString(): string", PUB, rk_to_string)
        .method(
            "getAttributes(?string $name = null, int $flags = 0): array",
            PUB,
            rk_get_attributes,
        )
        .register(vm);
}
