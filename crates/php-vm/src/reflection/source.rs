// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Access to the source ASTs of declarations.
//!
//! The VM compiles constant expressions (property defaults, class constant
//! values) to op arrays. For multibyte files, retain the declaration AST so
//! reflection uses the original encoding/substitution semantics even after
//! runtime settings change. Otherwise re-parse with the VM's current settings
//! and cache per VM, rather than sharing incompatible encodings across VMs.

use crate::vm::Vm;
use php_parser::ast::{self, ClassDecl, ClassMemberKind, Expr, Stmt, StmtKind};
use php_runtime::ZStr;
use std::rc::Rc;

fn program(vm: &Vm, file: &[u8]) -> Option<Rc<ast::Program>> {
    if let Some(p) = vm.parsed_sources.borrow().get(file).cloned() {
        return Some(p);
    }
    let path = String::from_utf8_lossy(file).into_owned();
    let p = std::fs::read(&path)
        .ok()
        .and_then(|src| vm.parse_source(&src, false, true).ok().map(Rc::new));
    if let Some(p) = &p {
        vm.parsed_sources
            .borrow_mut()
            .insert(ZStr::from(file), p.clone());
    }
    p
}

fn find_in<'a>(
    stmts: &'a [Stmt],
    line: u32,
    ns: &mut Vec<u8>,
    out: &mut Option<(&'a ClassDecl, Vec<u8>)>,
) {
    for s in stmts {
        if out.is_some() {
            return;
        }
        match &s.kind {
            StmtKind::Class(c) => {
                if c.span.line == line || c.end_line == line {
                    *out = Some((c, ns.clone()));
                    return;
                }
            }
            StmtKind::Namespace { name, body } => {
                let n = name.as_ref().map(|n| n.name.clone()).unwrap_or_default();
                match body {
                    Some(b) => {
                        let mut inner = n;
                        find_in(b, line, &mut inner, out);
                    }
                    None => *ns = n,
                }
            }
            StmtKind::Block(b) => find_in(b, line, ns, out),
            StmtKind::If {
                branches,
                else_branch,
            } => {
                for br in branches {
                    find_in(std::slice::from_ref(br.body.as_ref()), line, ns, out);
                }
                if let Some(e) = else_branch {
                    find_in(std::slice::from_ref(e.as_ref()), line, ns, out);
                }
            }
            StmtKind::Function(f) => find_in(&f.body, line, ns, out),
            _ => {}
        }
    }
}

/// Finds a class declaration (by file and start line) and the namespace
/// it is declared in.
pub(crate) fn class_ast(vm: &Vm, file: &[u8], line: u32) -> Option<(ClassDecl, Vec<u8>)> {
    let p = program(vm, file)?;
    let mut ns = Vec::new();
    let mut out = None;
    find_in(&p.statements, line, &mut ns, &mut out);
    out.map(|(c, n)| (c.clone(), n))
}

/// The default value expression of property `name` declared in the class at
/// `file:line`.
pub(crate) fn prop_default(
    vm: &Vm,
    file: &[u8],
    line: u32,
    name: &[u8],
) -> Option<(Expr, Vec<u8>)> {
    let (c, ns) = class_ast(vm, file, line)?;
    for m in &c.members {
        if let ClassMemberKind::Property(g) = &m.kind {
            for p in &g.props {
                if p.name.name == name {
                    return p.default.clone().map(|e| (e, ns.clone()));
                }
            }
        }
    }
    None
}

/// The value expression of class constant `name` declared in the class at
/// `file:line`.
pub(crate) fn const_value(vm: &Vm, file: &[u8], line: u32, name: &[u8]) -> Option<(Expr, Vec<u8>)> {
    let (c, ns) = class_ast(vm, file, line)?;
    for m in &c.members {
        match &m.kind {
            ClassMemberKind::Const(g) => {
                for k in &g.consts {
                    if k.name.name == name {
                        return Some((k.value.clone(), ns.clone()));
                    }
                }
            }
            ClassMemberKind::EnumCase(ec) if ec.name.name == name => {
                return ec.value.clone().map(|e| (e, ns.clone()));
            }
            _ => {}
        }
    }
    None
}

/// Collects the parameter lists of every function-like declaration.
struct ParamCollector {
    line: u32,
    end: u32,
    found: Option<Vec<ast::Param>>,
}

impl ParamCollector {
    fn check(&mut self, start: u32, end: u32, params: &[ast::Param]) {
        if self.found.is_none()
            && start == self.line
            && (end == self.end || self.end == 0 || end == 0)
        {
            self.found = Some(params.to_vec());
        }
    }
}

impl php_parser::visit::Visitor for ParamCollector {
    fn visit_stmt(&mut self, s: &Stmt) {
        if let StmtKind::Function(f) = &s.kind {
            self.check(f.span.line, f.end_line, &f.params);
        }
        php_parser::visit::walk_stmt(self, s);
    }
    fn visit_expr(&mut self, e: &Expr) {
        match &e.kind {
            ast::ExprKind::Closure(c) => self.check(c.span.line, c.end_line, &c.params),
            ast::ExprKind::ArrowFn(c) => self.check(c.span.line, 0, &c.params),
            _ => {}
        }
        php_parser::visit::walk_expr(self, e);
    }
    fn visit_class(&mut self, c: &ClassDecl) {
        for m in &c.members {
            match &m.kind {
                ClassMemberKind::Method(md) => {
                    self.check(md.span.line, md.end_line, &md.params);
                    for p in &md.params {
                        for h in p.hooks.iter().flatten() {
                            if let Some(ps) = &h.params {
                                self.check(h.span.line, h.end_line, ps);
                            }
                        }
                    }
                }
                ClassMemberKind::Property(g) => {
                    for p in &g.props {
                        for h in p.hooks.iter().flatten() {
                            if let Some(ps) = &h.params {
                                self.check(h.span.line, h.end_line, ps);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        php_parser::visit::walk_class(self, c);
    }
}

/// The doc comment of parameter `idx` of the function declared at
/// `file:line-end`.
pub(crate) fn param_doc_comment(
    vm: &Vm,
    file: &[u8],
    line: u32,
    end: u32,
    idx: usize,
) -> Option<Vec<u8>> {
    let p = program(vm, file)?;
    let mut v = ParamCollector {
        line,
        end,
        found: None,
    };
    php_parser::visit::walk_stmts(&mut v, &p.statements);
    v.found?.get(idx)?.doc_comment.clone()
}

/// Finds attribute arguments in the original encoding-aware declaration AST.
struct AttributeArgs<'a> {
    line: u32,
    name: &'a [u8],
    found: Option<Vec<ast::Arg>>,
}

impl AttributeArgs<'_> {
    fn check(&mut self, groups: &ast::Attributes) {
        for group in groups {
            for attribute in &group.attributes {
                let short = attribute.name.name.rsplit(|&byte| byte == b'\\').next();
                let want = self.name.rsplit(|&byte| byte == b'\\').next();
                if self.found.is_none()
                    && attribute.span.line == self.line
                    && short
                        .zip(want)
                        .is_some_and(|(short, want)| short.eq_ignore_ascii_case(want))
                {
                    self.found = Some(
                        attribute
                            .args
                            .as_ref()
                            .map(|args| args.args.clone())
                            .unwrap_or_default(),
                    );
                }
            }
        }
    }

    fn hooks(&mut self, hooks: Option<&Vec<ast::PropertyHook>>) {
        for hook in hooks.into_iter().flatten() {
            self.check(&hook.attributes);
        }
    }
}

impl php_parser::visit::Visitor for AttributeArgs<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Function(function) => self.check(&function.attributes),
            StmtKind::Const { attributes, .. } => self.check(attributes),
            _ => {}
        }
        php_parser::visit::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ast::ExprKind::Closure(function) => self.check(&function.attributes),
            ast::ExprKind::ArrowFn(function) => self.check(&function.attributes),
            _ => {}
        }
        php_parser::visit::walk_expr(self, expr);
    }

    fn visit_param(&mut self, param: &ast::Param) {
        self.check(&param.attributes);
        self.hooks(param.hooks.as_ref());
        php_parser::visit::walk_param(self, param);
    }

    fn visit_class(&mut self, class: &ClassDecl) {
        self.check(&class.attributes);
        for member in &class.members {
            match &member.kind {
                ClassMemberKind::Method(method) => self.check(&method.attributes),
                ClassMemberKind::Const(group) => self.check(&group.attributes),
                ClassMemberKind::Property(group) => {
                    self.check(&group.attributes);
                    for property in &group.props {
                        self.hooks(property.hooks.as_ref());
                    }
                }
                ClassMemberKind::EnumCase(case) => self.check(&case.attributes),
                _ => {}
            }
        }
        php_parser::visit::walk_class(self, class);
    }
}

/// The argument expressions of the attribute `name` written at `file:line`
/// (the `n`-th attribute of that name in the group).
pub(crate) fn attribute_args(
    vm: &Vm,
    file: &[u8],
    line: u32,
    name: &[u8],
) -> Option<Vec<ast::Arg>> {
    let program = program(vm, file)?;
    let mut visitor = AttributeArgs {
        line,
        name,
        found: None,
    };
    php_parser::visit::walk_stmts(&mut visitor, &program.statements);
    visitor.found
}

/// The attributes of enum case `name` (name, arguments) declared in the class
/// at `file:line`, with the namespace of the declaration.
pub(crate) fn case_attributes(
    vm: &Vm,
    file: &[u8],
    line: u32,
    name: &[u8],
) -> Option<(ast::Attributes, Vec<u8>)> {
    let (c, ns) = class_ast(vm, file, line)?;
    c.members.iter().find_map(|m| match &m.kind {
        ClassMemberKind::EnumCase(ec) if ec.name.name == name => {
            Some((ec.attributes.clone(), ns.clone()))
        }
        _ => None,
    })
}

/// Evaluates a literal constant expression (no constants or calls).
pub(crate) fn literal_value(e: &Expr) -> Option<php_runtime::Value> {
    use php_runtime::{Value, ZArray, ZStr};
    Some(match &e.kind {
        ast::ExprKind::Int(i) => Value::Int(*i),
        ast::ExprKind::Float(f) => Value::Float(*f),
        ast::ExprKind::String(s) => Value::Str(ZStr::from(s.as_slice())),
        ast::ExprKind::Const(n) => match n.name.to_ascii_lowercase().as_slice() {
            b"true" => Value::Bool(true),
            b"false" => Value::Bool(false),
            b"null" => Value::Null,
            _ => return None,
        },
        ast::ExprKind::Array { items, .. } => {
            let mut t = php_runtime::HashTable::new();
            for it in items {
                let it = it.as_ref()?;
                let v = literal_value(&it.value)?;
                match &it.key {
                    Some(k) => {
                        let kv = literal_value(k)?;
                        let key = match kv {
                            Value::Int(i) => php_runtime::ArrayKey::Int(i),
                            Value::Str(s) => php_runtime::ArrayKey::from_zstr(&s),
                            _ => return None,
                        };
                        t.insert(key, v);
                    }
                    None => {
                        let _ = t.push(v);
                    }
                }
            }
            Value::Array(ZArray::from_table(t))
        }
        ast::ExprKind::Unary {
            op: ast::UnaryOp::Minus,
            expr,
        } => match literal_value(expr)? {
            Value::Int(i) => Value::Int(-i),
            Value::Float(f) => Value::Float(-f),
            _ => return None,
        },
        _ => return None,
    })
}

/// Collects `static $x = expr;` initializers within a line range.
struct StaticInits {
    start: u32,
    end: u32,
    found: Vec<(Vec<u8>, Expr)>,
}

impl php_parser::visit::Visitor for StaticInits {
    fn visit_stmt(&mut self, s: &Stmt) {
        if let StmtKind::Static(vars) = &s.kind {
            if s.span.line >= self.start && s.span.line <= self.end {
                for v in vars {
                    if let Some(e) = &v.default {
                        if !self.found.iter().any(|(n, _)| *n == v.name.name) {
                            self.found.push((v.name.name.clone(), e.clone()));
                        }
                    }
                }
            }
        }
        php_parser::visit::walk_stmt(self, s);
    }
}

/// The initializer expressions of the `static` variables of the function
/// spanning `file:start-end`.
pub(crate) fn static_initializers(
    vm: &Vm,
    file: &[u8],
    start: u32,
    end: u32,
) -> Vec<(Vec<u8>, Expr)> {
    let Some(p) = program(vm, file) else {
        return Vec::new();
    };
    let mut v = StaticInits {
        start,
        end,
        found: Vec::new(),
    };
    php_parser::visit::walk_stmts(&mut v, &p.statements);
    v.found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_arguments_use_encoding_aware_declaration_ast() {
        let opts = php_parser::ParseOptions {
            multibyte: true,
            ..Default::default()
        };
        let program = php_parser::parse(
            b"<?php declare(encoding='SJIS');\n#[A(1, '\x82\xA0')] class C {}",
            &opts,
        )
        .unwrap();
        let vm = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        vm.parsed_sources
            .borrow_mut()
            .insert(ZStr::from("not-on-disk.php"), Rc::new(program));
        let args = attribute_args(&vm, b"not-on-disk.php", 2, b"A").unwrap();
        assert_eq!(args.len(), 2);
        let ast::ArgKind::Positional(value) = &args[1].kind else {
            panic!("expected positional argument");
        };
        assert_eq!(value.kind, ast::ExprKind::String("あ".as_bytes().to_vec()));
        let other = Vm::new(&crate::VmOptions::default(), Box::new(std::io::sink()));
        assert!(attribute_args(&other, b"not-on-disk.php", 2, b"A").is_none());
    }

    #[test]
    fn literal_values() {
        let e = super::super::export::parse_const_expr("[1, 'a' => -2.5, true]").expect("parses");
        let v = literal_value(&e).expect("literal");
        let php_runtime::Value::Array(a) = v else {
            panic!("array")
        };
        assert_eq!(a.len(), 3);
        let e = super::super::export::parse_const_expr("FOO").expect("parses");
        assert!(literal_value(&e).is_none());
    }
}
