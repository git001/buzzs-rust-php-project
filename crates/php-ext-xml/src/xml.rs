// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! `ext/xml`: the `XMLParser` class and the `xml_*()` functions over
//! `php_libxml::xml`.
//!
//! Handlers are stored per parser as PHP callables or — for method names
//! resolved against the object given to the deprecated `xml_set_object()`
//! — as `(object, method)` pairs that follow the object when it is
//! swapped, exactly like the C extension's `calling_scope == NULL` trick.

use std::cell::RefCell;
use std::rc::Rc;

use php_libxml::xml::{
    error_string, HandlerKind, OptionValue, StructEntry, StructSink, StructValue, XmlError,
    XmlHandlers, XmlOption, XmlParser, ERROR_CONSTANTS, OPTION_CONSTANTS, XML_SAX_IMPL,
};
use php_runtime::{ArrayKey, HashTable, Obj, PhpRef, Value, ZArray, ZStr};
use php_vm::ext::{acc, class_flags, Args, ClassBuilder, ClassHooks, ModuleBuilder, VmResult};
use php_vm::{object, Throw, Vm};

/// Handler kinds in `xml_parser` field order (the order `xml_set_object()`
/// re-resolves them in).
const KINDS: [HandlerKind; 10] = [
    HandlerKind::StartElement,
    HandlerKind::EndElement,
    HandlerKind::CharacterData,
    HandlerKind::ProcessingInstruction,
    HandlerKind::Default,
    HandlerKind::UnparsedEntityDecl,
    HandlerKind::NotationDecl,
    HandlerKind::ExternalEntityRef,
    HandlerKind::StartNamespaceDecl,
    HandlerKind::EndNamespaceDecl,
];

fn slot(kind: HandlerKind) -> usize {
    KINDS.iter().position(|k| *k == kind).unwrap_or(0)
}

/// A registered handler.
#[derive(Clone)]
enum Handler {
    /// A proper PHP callable.
    Callable(Value),
    /// A method name resolved on the `xml_set_object()` object.
    Method {
        /// The object.
        object: Obj,
        /// The method's declared name.
        name: ZStr,
    },
}

/// The native state of an `XMLParser` object.
struct ParserState {
    parser: XmlParser,
    /// `parser->object` (`xml_set_object()`).
    object: RefCell<Option<Obj>>,
    handlers: RefCell<[Option<Handler>; 10]>,
    /// `parser->data` (`$values` of `xml_parse_into_struct()`).
    data: RefCell<Option<PhpRef>>,
    /// `parser->info` (`$index`).
    info: RefCell<Option<PhpRef>>,
}

impl ParserState {
    fn new(parser: XmlParser) -> Self {
        Self {
            parser,
            object: RefCell::new(None),
            handlers: RefCell::new(Default::default()),
            data: RefCell::new(None),
            info: RefCell::new(None),
        }
    }

    fn handler(&self, kind: HandlerKind) -> Option<Handler> {
        self.handlers.borrow()[slot(kind)].clone()
    }

    fn set(&self, kind: HandlerKind, h: Option<Handler>) -> Result<(), XmlError> {
        self.parser.set_handler(kind, h.is_some())?;
        self.handlers.borrow_mut()[slot(kind)] = h;
        Ok(())
    }
}

type State = Rc<ParserState>;

/// The parser state of argument 0 (typed `XMLParser` by the signature).
fn state_of(vm: &mut Vm, a: &Args) -> VmResult<(Obj, State)> {
    if let Value::Object(o) = a.at(0) {
        if let Some(s) = object::internal::<State, _>(&o, |s| Rc::clone(s)) {
            return Ok((o, s));
        }
    }
    Err(vm.throw("Error", "Invalid or uninitialized XMLParser object"))
}

fn bytes(s: &[u8]) -> Value {
    Value::Str(ZStr::from(s))
}

fn opt_bytes(s: Option<&[u8]>) -> Value {
    s.map_or(Value::Bool(false), bytes)
}

// ---------------------------------------------------------------------
// Handlers bridge
// ---------------------------------------------------------------------

/// Delivers parser events to the PHP handlers of one parser.
struct VmHandlers<'a> {
    vm: &'a mut Vm,
    state: &'a ParserState,
    this: Value,
    pending: Option<Throw>,
}

impl VmHandlers<'_> {
    fn call(&mut self, kind: HandlerKind, args: Vec<Value>) -> Option<Value> {
        if self.pending.is_some() {
            return None;
        }
        let h = self.state.handler(kind)?;
        let mut full = Vec::with_capacity(args.len() + 1);
        full.push(self.this.clone());
        full.extend(args);
        let r = match &h {
            Handler::Callable(c) => self.vm.call_value(c, full),
            Handler::Method { object, name } => {
                self.vm
                    .call_method_by_name(object, &name.to_string_lossy(), full)
            }
        };
        match r {
            Ok(v) => Some(v),
            Err(t) => {
                self.pending = Some(t);
                None
            }
        }
    }
}

impl XmlHandlers for VmHandlers<'_> {
    /// `xml_handler_aborted()`: `EG(exception)` or `ZEND_HAS_OOM()` (the
    /// soft memory limit was crossed; the VM raises the fatal error once
    /// `xml_parse()` returns).
    fn aborted(&self) -> bool {
        self.pending.is_some() || php_memlimit::is_pending()
    }

    fn start_element(&mut self, name: &[u8], attributes: &[(Vec<u8>, Vec<u8>)]) {
        let mut attrs = ZArray::new();
        for (k, v) in attributes {
            attrs.make_mut().insert(ArrayKey::from_bytes(k), bytes(v));
            if php_memlimit::is_pending() {
                return;
            }
        }
        self.call(
            HandlerKind::StartElement,
            vec![bytes(name), Value::Array(attrs)],
        );
    }

    fn end_element(&mut self, name: &[u8]) {
        self.call(HandlerKind::EndElement, vec![bytes(name)]);
    }

    fn character_data(&mut self, data: &[u8]) {
        self.call(HandlerKind::CharacterData, vec![bytes(data)]);
    }

    fn processing_instruction(&mut self, target: Option<&[u8]>, data: Option<&[u8]>) {
        self.call(
            HandlerKind::ProcessingInstruction,
            vec![opt_bytes(target), opt_bytes(data)],
        );
    }

    fn default_handler(&mut self, data: &[u8]) {
        self.call(HandlerKind::Default, vec![bytes(data)]);
    }

    fn unparsed_entity_decl(
        &mut self,
        entity_name: Option<&[u8]>,
        base: Option<&[u8]>,
        system_id: Option<&[u8]>,
        public_id: Option<&[u8]>,
        notation_name: Option<&[u8]>,
    ) {
        self.call(
            HandlerKind::UnparsedEntityDecl,
            vec![
                opt_bytes(entity_name),
                opt_bytes(base),
                opt_bytes(system_id),
                opt_bytes(public_id),
                opt_bytes(notation_name),
            ],
        );
    }

    fn notation_decl(
        &mut self,
        notation_name: Option<&[u8]>,
        base: Option<&[u8]>,
        system_id: Option<&[u8]>,
        public_id: Option<&[u8]>,
    ) {
        self.call(
            HandlerKind::NotationDecl,
            vec![
                opt_bytes(notation_name),
                opt_bytes(base),
                opt_bytes(system_id),
                opt_bytes(public_id),
            ],
        );
    }

    fn external_entity_ref(
        &mut self,
        open_entity_names: Option<&[u8]>,
        base: Option<&[u8]>,
        system_id: Option<&[u8]>,
        public_id: Option<&[u8]>,
    ) -> i64 {
        let Some(v) = self.call(
            HandlerKind::ExternalEntityRef,
            vec![
                opt_bytes(open_entity_names),
                opt_bytes(base),
                opt_bytes(system_id),
                opt_bytes(public_id),
            ],
        ) else {
            return 0;
        };
        match self.vm.value_to_int(&v) {
            Ok(i) => i,
            Err(t) => {
                self.pending = Some(t);
                0
            }
        }
    }

    fn start_namespace_decl(&mut self, prefix: Option<&[u8]>, uri: Option<&[u8]>) {
        self.call(
            HandlerKind::StartNamespaceDecl,
            vec![opt_bytes(prefix), opt_bytes(uri)],
        );
    }

    fn end_namespace_decl(&mut self, prefix: Option<&[u8]>) {
        self.call(HandlerKind::EndNamespaceDecl, vec![opt_bytes(prefix)]);
    }

    fn warning(&mut self, message: &str) {
        if self.pending.is_some() {
            return;
        }
        if let Err(t) = self.vm.php_warning(message) {
            self.pending = Some(t);
        }
    }
}

// ---------------------------------------------------------------------
// xml_parse_into_struct() sink over the live PHP arrays
// ---------------------------------------------------------------------

/// Runs `f` on the array held by `r` (looking through a nested reference);
/// `None` when it is not an array.
fn with_array<R>(r: &PhpRef, f: impl FnOnce(&mut HashTable) -> R) -> Option<R> {
    let mut v = r.try_borrow_mut()?;
    match &mut *v {
        Value::Array(a) => Some(f(a.make_mut())),
        Value::Reference(inner) => {
            let inner = inner.clone();
            drop(v);
            with_array(&inner, f)
        }
        _ => None,
    }
}

/// Runs `f` on the array element `v` (looking through a reference).
fn with_elem<R>(v: &mut Value, f: impl FnOnce(&mut HashTable) -> R) -> Option<R> {
    match v {
        Value::Array(a) => Some(f(a.make_mut())),
        Value::Reference(r) => {
            let r = r.clone();
            with_array(&r, f)
        }
        _ => None,
    }
}

fn entry_value(entry: StructEntry) -> Value {
    let mut out = ZArray::new();
    for (k, v) in entry.fields {
        let v = match v {
            StructValue::Str(s) => bytes(&s),
            StructValue::Int(i) => Value::Int(i),
            StructValue::Attributes(attrs) => {
                let mut a = ZArray::new();
                for (k, v) in attrs {
                    a.make_mut().insert(ArrayKey::from_bytes(&k), bytes(&v));
                }
                Value::Array(a)
            }
        };
        out.make_mut().insert(k, v);
    }
    Value::Array(out)
}

/// [`StructSink`] over the `$values` / `$index` references.
struct RefSink {
    data: PhpRef,
    info: Option<PhpRef>,
}

impl StructSink for RefSink {
    fn push(&mut self, entry: StructEntry) -> Option<i64> {
        // `ZEND_HAS_OOM()`: the entries are counted as they are built.
        if php_memlimit::is_pending() {
            return None;
        }
        let v = entry_value(entry);
        with_array(&self.data, |a| a.push(v).ok()).flatten()
    }

    fn mark_complete(&mut self, index: i64) {
        with_array(&self.data, |a| {
            if let Some(e) = a.get_mut(&ArrayKey::Int(index)) {
                with_elem(e, |e| e.insert("type", Value::from("complete")));
            }
        });
    }

    fn append_value(&mut self, index: i64, text: &[u8]) -> Option<bool> {
        with_array(&self.data, |a| {
            let e = a.get_mut(&ArrayKey::Int(index))?;
            with_elem(e, |e| match e.get_mut(&ArrayKey::from("value")) {
                Some(Value::Str(s)) => {
                    let mut b = s.as_bytes().to_vec();
                    b.extend_from_slice(text);
                    *s = ZStr::from(b);
                    true
                }
                _ => false,
            })
        })
        .flatten()
    }

    fn set_value(&mut self, index: i64, text: &[u8]) {
        with_array(&self.data, |a| {
            if let Some(e) = a.get_mut(&ArrayKey::Int(index)) {
                with_elem(e, |e| e.insert("value", bytes(text)));
            }
        });
    }

    fn values_is_array(&mut self) -> bool {
        with_array(&self.data, |_| ()).is_some()
    }

    fn append_to_last_cdata(&mut self, text: &[u8]) -> bool {
        with_array(&self.data, |a| {
            let pos = a.last().map(|(k, _)| k.clone())?;
            let e = a.get_mut(&pos)?;
            with_elem(e, |e| {
                let is_cdata = matches!(
                    e.get(&ArrayKey::from("type")).map(Value::deref),
                    Some(Value::Str(t)) if t.as_bytes() == b"cdata"
                );
                if !is_cdata {
                    return false;
                }
                match e.get_mut(&ArrayKey::from("value")) {
                    Some(Value::Str(s)) => {
                        let mut b = s.as_bytes().to_vec();
                        b.extend_from_slice(text);
                        *s = ZStr::from(b);
                        true
                    }
                    _ => false,
                }
            })
        })
        .flatten()
        .unwrap_or(false)
    }

    fn add_index(&mut self, tag: &[u8], position: i64) -> bool {
        let Some(info) = &self.info else {
            return false;
        };
        if php_memlimit::is_pending() {
            return false;
        }
        with_array(info, |a| {
            let key = ArrayKey::from_bytes(tag);
            if let Some(e) = a.get_mut(&key) {
                with_elem(e, |e| e.push(Value::Int(position)).ok());
            } else {
                let mut list = ZArray::new();
                let _ = list.make_mut().push(Value::Int(position));
                a.insert(key, Value::Array(list));
            }
        })
        .is_some()
    }
}

// ---------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------

fn throw_xml_error(vm: &mut Vm, a: &Args, e: &XmlError) -> Throw {
    match e {
        XmlError::UnsupportedSourceEncoding => {
            a.value_error(vm, 1, "is not a supported source encoding")
        }
        XmlError::UnsupportedTargetEncoding => {
            a.value_error(vm, 3, "is not a supported target encoding")
        }
        other => vm.throw("Error", &other.to_string()),
    }
}

fn run_parse(
    vm: &mut Vm,
    obj: &Obj,
    st: &ParserState,
    f: impl FnOnce(&XmlParser, &mut VmHandlers<'_>, Option<&mut RefSink>) -> Result<i64, XmlError>,
) -> VmResult<Result<i64, XmlError>> {
    let mut sink = st.data.borrow().clone().map(|data| RefSink {
        data,
        info: st.info.borrow().clone(),
    });
    let mut h = VmHandlers {
        vm,
        state: st,
        this: Value::Object(obj.clone()),
        pending: None,
    };
    let r = f(&st.parser, &mut h, sink.as_mut());
    // libxml diagnostics are not displayed for ext/xml parsers; drop any.
    let _ = php_libxml::error::take_diagnostics();
    if let Some(t) = h.pending {
        return Err(t);
    }
    Ok(r)
}

fn xml_parse(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (obj, st) = state_of(vm, a)?;
    if st.parser.is_parsing() {
        return Err(vm.throw("Error", "Parser must not be called recursively"));
    }
    let data = a.str(1);
    let is_final = a.bool(2);
    let r = run_parse(vm, &obj, &st, |p, h, sink| {
        p.parse(
            data.as_bytes(),
            is_final,
            h,
            sink.map(|s| s as &mut dyn StructSink),
        )
    })?;
    match r {
        Ok(v) => Ok(Value::Int(v)),
        Err(e) => Err(throw_xml_error(vm, a, &e)),
    }
}

/// `zend_try_array_init()` on a by-reference argument.
fn init_array_ref(a: &Args, n: usize) -> Option<PhpRef> {
    let r = a.reference(n)?;
    r.set(Value::Array(ZArray::new()));
    Some(r)
}

fn xml_parse_into_struct(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (obj, st) = state_of(vm, a)?;
    if st.parser.is_parsing() {
        vm.php_warning("Parser must not be called recursively")?;
        return Ok(Value::Bool(false));
    }
    if a.has(3) {
        let info = init_array_ref(a, 3);
        *st.info.borrow_mut() = info;
    }
    let data_ref = init_array_ref(a, 2);
    if let Some(d) = data_ref {
        *st.data.borrow_mut() = Some(d);
    }
    let data = a.str(1);
    let r = run_parse(vm, &obj, &st, |p, h, sink| match sink {
        Some(s) => p.parse_into_struct(data.as_bytes(), h, s),
        None => p.parse(data.as_bytes(), true, h, None),
    })?;
    match r {
        Ok(v) => Ok(Value::Int(v)),
        Err(XmlError::Recursive) => {
            vm.php_warning("Parser must not be called recursively")?;
            Ok(Value::Bool(false))
        }
        Err(e) => Err(throw_xml_error(vm, a, &e)),
    }
}

// ---------------------------------------------------------------------
// Creation, options, queries
// ---------------------------------------------------------------------

fn create(vm: &mut Vm, a: &Args, ns: bool) -> VmResult<Value> {
    let encoding = a.opt(0).map(|v| match v {
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    });
    let parser = if ns {
        let sep = a.str(1);
        XmlParser::new_ns(encoding.as_deref(), sep.as_bytes())
    } else {
        XmlParser::new(encoding.as_deref())
    };
    let parser = parser.map_err(|e| throw_xml_error(vm, a, &e))?;
    let class = vm
        .find_class(b"XMLParser")
        .ok_or_else(|| vm.throw("Error", "Class \"XMLParser\" not found"))?;
    let obj = vm.new_object(&class)?;
    object::set_internal::<State>(&obj, Rc::new(ParserState::new(parser)));
    Ok(Value::Object(obj))
}

fn xml_parser_create(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    create(vm, a, false)
}

fn xml_parser_create_ns(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    create(vm, a, true)
}

fn xml_get_error_code(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    Ok(Value::Int(st.parser.error_code()))
}

fn xml_error_string_fn(_vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    Ok(Value::from(error_string(a.int(0))))
}

fn xml_get_current_line_number(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    Ok(Value::Int(st.parser.current_line_number()))
}

fn xml_get_current_column_number(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    Ok(Value::Int(st.parser.current_column_number()))
}

fn xml_get_current_byte_index(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    Ok(Value::Int(st.parser.current_byte_index()))
}

fn xml_parser_free(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    if st.parser.is_parsing() {
        vm.php_warning("Parser cannot be freed while it is parsing")?;
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(true))
}

fn xml_parser_set_option(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    let opt = a.int(1);
    let value = a.at(2);
    if !matches!(value, Value::Bool(_) | Value::Int(_) | Value::Str(_)) {
        vm.php_warning(&format!(
            "Argument #3 ($value) must be of type string|int|bool, {} given",
            value.type_name()
        ))?;
    }
    let Some(option) = XmlOption::from_i64(opt) else {
        return Err(a.value_error(vm, 2, "must be a XML_OPTION_* constant"));
    };
    match option {
        XmlOption::CaseFolding => {
            let b = vm.value_to_bool(&value)?;
            st.parser.set_case_folding(b);
        }
        XmlOption::SkipWhite => {
            let b = vm.value_to_bool(&value)?;
            st.parser.set_skip_white(b);
        }
        XmlOption::ParseHuge => {
            let b = vm.value_to_bool(&value)?;
            st.parser
                .set_parse_huge(b)
                .map_err(|e| throw_xml_error(vm, a, &e))?;
        }
        XmlOption::SkipTagstart => {
            let i = vm.value_to_int(&value)?;
            if let Err(e) = st.parser.set_skip_tagstart(i) {
                vm.php_warning(&e.to_string())?;
                return Ok(Value::Bool(false));
            }
        }
        XmlOption::TargetEncoding => {
            let s = vm.value_to_string(&value)?;
            st.parser
                .set_target_encoding(s.as_bytes())
                .map_err(|e| throw_xml_error(vm, a, &e))?;
        }
    }
    Ok(Value::Bool(true))
}

fn xml_parser_get_option(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    let Some(option) = XmlOption::from_i64(a.int(1)) else {
        return Err(a.value_error(vm, 2, "must be a XML_OPTION_* constant"));
    };
    Ok(match st.parser.get_option(option) {
        OptionValue::Bool(b) => Value::Bool(b),
        OptionValue::Int(i) => Value::Int(i),
        OptionValue::Str(s) => Value::from(s),
    })
}

// ---------------------------------------------------------------------
// Handler registration
// ---------------------------------------------------------------------

/// How a `callable|string|null` handler argument parses.
enum HandlerArg {
    /// `null` (`F!` with null).
    Null,
    /// A callable (`F`).
    Callable(Value),
    /// A non-callable string (`S`).
    Name(ZStr),
    /// Anything else.
    Invalid,
}

fn classify(vm: &mut Vm, v: Value) -> HandlerArg {
    match v {
        Value::Null => HandlerArg::Null,
        v if vm.is_callable(&v) => HandlerArg::Callable(v),
        Value::Str(s) => HandlerArg::Name(s),
        _ => HandlerArg::Invalid,
    }
}

/// The string a `S` parameter would accept (callable strings included).
fn as_string(arg: &HandlerArg) -> Option<ZStr> {
    match arg {
        HandlerArg::Name(s) | HandlerArg::Callable(Value::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// What `F!` would parse: `Ok(handler)` (with `None` for `null`), or
/// `Err(())` when the argument is neither callable nor `null`.
fn fcc(arg: &HandlerArg) -> Result<Option<Handler>, ()> {
    match arg {
        HandlerArg::Null => Ok(None),
        HandlerArg::Callable(v) => Ok(Some(Handler::Callable(v.clone()))),
        _ => Err(()),
    }
}

/// `php_xml_check_string_method_arg()`.
fn method_handler(
    vm: &mut Vm,
    a: &Args,
    st: &ParserState,
    arg_num: usize,
    name: &ZStr,
) -> VmResult<Option<Handler>> {
    if name.as_bytes().is_empty() {
        return Ok(None);
    }
    let Some(obj) = st.object.borrow().clone() else {
        return Err(a.value_error(
            vm,
            arg_num,
            "an object must be set via xml_set_object() to be able to lookup method",
        ));
    };
    let class = object::class_of(&obj).clone();
    match class.find_method(name.as_bytes()) {
        Some(m) => Ok(Some(Handler::Method {
            object: obj.clone(),
            name: m.name.clone(),
        })),
        None => Err(a.value_error(
            vm,
            arg_num,
            &format!(
                "method {}::{}() does not exist",
                class.name.to_string_lossy(),
                name.to_string_lossy()
            ),
        )),
    }
}

fn deprecate_strings(vm: &mut Vm) -> VmResult<()> {
    vm.php_deprecated("Passing non-callable strings is deprecated since 8.4")
}

fn set_handlers(
    vm: &mut Vm,
    a: &Args,
    st: &ParserState,
    list: &[(HandlerKind, Option<Handler>)],
) -> VmResult<Value> {
    if st.parser.is_parsing() {
        return Err(vm.throw("Error", "Cannot change handlers while parsing"));
    }
    for (kind, h) in list {
        st.set(*kind, h.clone())
            .map_err(|e| throw_xml_error(vm, a, &e))?;
    }
    Ok(Value::Bool(true))
}

fn set_single(vm: &mut Vm, a: &Args, kind: HandlerKind) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    let arg = classify(vm, a.at(1));
    let h = match &arg {
        HandlerArg::Null | HandlerArg::Callable(_) => fcc(&arg).ok().flatten(),
        HandlerArg::Name(name) => {
            deprecate_strings(vm)?;
            method_handler(vm, a, &st, 2, name)?
        }
        HandlerArg::Invalid => {
            return Err(a.type_error(vm, 2, "must be of type callable|string|null"));
        }
    };
    set_handlers(vm, a, &st, &[(kind, h)])
}

fn xml_set_element_handler(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    let start = classify(vm, a.at(1));
    let end = classify(vm, a.at(2));
    let (s, e) = match (fcc(&start).ok(), fcc(&end).ok()) {
        (Some(s), Some(e)) => (s, e),
        (Some(s), None) if as_string(&end).is_some() => {
            deprecate_strings(vm)?;
            let name = as_string(&end).unwrap_or_default();
            let e = method_handler(vm, a, &st, 3, &name)?;
            (s, e)
        }
        _ => match (as_string(&start), fcc(&end).ok(), as_string(&end)) {
            (Some(sn), Some(e), _) => {
                deprecate_strings(vm)?;
                let s = method_handler(vm, a, &st, 2, &sn)?;
                (s, e)
            }
            (Some(sn), None, Some(en)) => {
                deprecate_strings(vm)?;
                let s = method_handler(vm, a, &st, 2, &sn)?;
                let e = method_handler(vm, a, &st, 3, &en)?;
                (s, e)
            }
            _ => {
                let n = if matches!(a.at(1), Value::Null | Value::Str(_)) {
                    3
                } else {
                    2
                };
                return Err(a.type_error(vm, n, "must be of type callable|string|null"));
            }
        },
    };
    set_handlers(
        vm,
        a,
        &st,
        &[(HandlerKind::StartElement, s), (HandlerKind::EndElement, e)],
    )
}

macro_rules! single_setter {
    ($($fn:ident => $kind:ident),* $(,)?) => {
        $(fn $fn(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
            set_single(vm, a, HandlerKind::$kind)
        })*
    };
}

single_setter! {
    xml_set_character_data_handler => CharacterData,
    xml_set_processing_instruction_handler => ProcessingInstruction,
    xml_set_default_handler => Default,
    xml_set_unparsed_entity_decl_handler => UnparsedEntityDecl,
    xml_set_notation_decl_handler => NotationDecl,
    xml_set_external_entity_ref_handler => ExternalEntityRef,
    xml_set_start_namespace_decl_handler => StartNamespaceDecl,
    xml_set_end_namespace_decl_handler => EndNamespaceDecl,
}

fn xml_set_object(vm: &mut Vm, a: &mut Args) -> VmResult<Value> {
    let (_, st) = state_of(vm, a)?;
    let Value::Object(new_this) = a.at(1) else {
        return Ok(Value::Bool(true));
    };
    let old = st.object.borrow().clone();
    if let Some(old) = old {
        let new_class = object::class_of(&new_this).clone();
        for kind in KINDS {
            let Some(Handler::Method { object, name }) = st.handler(kind) else {
                continue;
            };
            if !Obj::ptr_eq(&object, &old) {
                continue;
            }
            let Some(m) = new_class.find_method(name.as_bytes()) else {
                st.handlers.borrow_mut()[slot(kind)] = None;
                return Err(a.value_error(
                    vm,
                    2,
                    &format!(
                        "cannot safely swap to object of class {} as method \"{}\" does not exist, which was set via {}",
                        new_class.name.to_string_lossy(),
                        name.to_string_lossy(),
                        kind.setter_name()
                    ),
                ));
            };
            st.handlers.borrow_mut()[slot(kind)] = Some(Handler::Method {
                object: new_this.clone(),
                name: m.name.clone(),
            });
        }
    }
    *st.object.borrow_mut() = Some(new_this);
    Ok(Value::Bool(true))
}

/// `zend_objects_not_comparable`.
fn not_comparable(_vm: &mut Vm, l: &Value, r: &Value) -> VmResult<Option<i32>> {
    if let (Value::Object(a), Value::Object(b)) = (l, r) {
        if Obj::ptr_eq(a, b) {
            return Ok(Some(0));
        }
    }
    Ok(Some(1))
}

/// `xml_parser_get_constructor`: `XMLParser` cannot be instantiated
/// directly.
fn xml_parser_construct(vm: &mut Vm, _a: &mut Args) -> VmResult<Value> {
    Err(vm.throw(
        "Error",
        "Cannot directly construct XMLParser, use xml_parser_create() or xml_parser_create_ns() instead",
    ))
}

/// Registers the `xml` module.
pub(crate) fn register(m: &mut ModuleBuilder<'_>) {
    for (name, v) in ERROR_CONSTANTS.iter().chain(OPTION_CONSTANTS.iter()) {
        m.constant(name, Value::Int(*v));
    }
    m.constant("XML_SAX_IMPL", Value::from(XML_SAX_IMPL));
    ClassBuilder::new("XMLParser")
        .flags(class_flags::FINAL | class_flags::NOT_SERIALIZABLE | class_flags::NO_CLONE)
        .hooks(ClassHooks {
            compare: Some(not_comparable),
            ..Default::default()
        })
        .method("__construct()", acc::PUBLIC, xml_parser_construct)
        .register(m.vm());
    let handler_sig = |name: &str| format!("{name}(XMLParser $parser, $handler): true");
    m.function_static(
        "xml_parser_create(?string $encoding = null): XMLParser",
        xml_parser_create,
    )
    .function_static(
        "xml_parser_create_ns(?string $encoding = null, string $separator = \":\"): XMLParser",
        xml_parser_create_ns,
    )
    .function_static(
        "xml_set_object(XMLParser $parser, object $object): true",
        xml_set_object,
    )
    .deprecated(
        "8.4",
        "provide a proper method callable to xml_set_*_handler() functions",
    )
    .function_static(
        "xml_set_element_handler(XMLParser $parser, $start_handler, $end_handler): true",
        xml_set_element_handler,
    );
    let singles: [(&str, php_vm::ext::NativeFn); 8] = [
        (
            "xml_set_character_data_handler",
            xml_set_character_data_handler,
        ),
        (
            "xml_set_processing_instruction_handler",
            xml_set_processing_instruction_handler,
        ),
        ("xml_set_default_handler", xml_set_default_handler),
        (
            "xml_set_unparsed_entity_decl_handler",
            xml_set_unparsed_entity_decl_handler,
        ),
        (
            "xml_set_notation_decl_handler",
            xml_set_notation_decl_handler,
        ),
        (
            "xml_set_external_entity_ref_handler",
            xml_set_external_entity_ref_handler,
        ),
        (
            "xml_set_start_namespace_decl_handler",
            xml_set_start_namespace_decl_handler,
        ),
        (
            "xml_set_end_namespace_decl_handler",
            xml_set_end_namespace_decl_handler,
        ),
    ];
    for (name, f) in singles {
        m.function(&handler_sig(name), f);
    }
    m.function_static(
        "xml_parse(XMLParser $parser, string $data, bool $is_final = false): int",
        xml_parse,
    )
    .function_static(
        "xml_parse_into_struct(XMLParser $parser, string $data, &$values, &$index = null): int|false",
        xml_parse_into_struct,
    )
    .function_static("xml_get_error_code(XMLParser $parser): int", xml_get_error_code)
    .function_static("xml_error_string(int $error_code): ?string", xml_error_string_fn)
    .function_static(
        "xml_get_current_line_number(XMLParser $parser): int",
        xml_get_current_line_number,
    )
    .function_static(
        "xml_get_current_column_number(XMLParser $parser): int",
        xml_get_current_column_number,
    )
    .function_static(
        "xml_get_current_byte_index(XMLParser $parser): int",
        xml_get_current_byte_index,
    )
    .function_static("xml_parser_free(XMLParser $parser): bool", xml_parser_free)
    .deprecated("8.5", "as it has no effect since PHP 8.0")
    .function_static(
        "xml_parser_set_option(XMLParser $parser, int $option, $value): bool",
        xml_parser_set_option,
    )
    .function_static(
        "xml_parser_get_option(XMLParser $parser, int $option): string|int|bool",
        xml_parser_get_option,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_distinct() {
        let mut seen: Vec<usize> = KINDS.iter().map(|k| slot(*k)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), KINDS.len());
    }

    #[test]
    fn entry_conversion_keeps_order() {
        let mut e = StructEntry::default();
        e.set("tag", StructValue::Str(b"A".to_vec()));
        e.set("type", StructValue::Str(b"open".to_vec()));
        e.set("level", StructValue::Int(1));
        let Value::Array(a) = entry_value(e) else {
            panic!("not an array")
        };
        let keys: Vec<String> = a
            .keys()
            .map(|k| match k {
                ArrayKey::Str(s) => s.to_string_lossy(),
                ArrayKey::Int(i) => i.to_string(),
            })
            .collect();
        assert_eq!(keys, ["tag", "type", "level"]);
    }

    #[test]
    fn ref_sink_builds_arrays() {
        let data = PhpRef::new(Value::Array(ZArray::new()));
        let info = PhpRef::new(Value::Array(ZArray::new()));
        let mut sink = RefSink {
            data: data.clone(),
            info: Some(info.clone()),
        };
        let mut e = StructEntry::default();
        e.set("tag", StructValue::Str(b"A".to_vec()));
        e.set("value", StructValue::Str(b"x".to_vec()));
        assert_eq!(sink.push(e), Some(0));
        assert_eq!(sink.append_value(0, b"y"), Some(true));
        sink.mark_complete(0);
        assert!(sink.add_index(b"A", 0));
        assert!(sink.add_index(b"A", 1));
        let Value::Array(d) = data.get() else {
            panic!()
        };
        let Some(Value::Array(first)) = d.get_int(0).cloned() else {
            panic!()
        };
        assert!(matches!(first.get_str(b"value"), Some(Value::Str(s)) if s.as_bytes() == b"xy"));
        assert!(
            matches!(first.get_str(b"type"), Some(Value::Str(s)) if s.as_bytes() == b"complete")
        );
        let Value::Array(i) = info.get() else {
            panic!()
        };
        assert!(matches!(i.get_str(b"A"), Some(Value::Array(l)) if l.len() == 2));
    }
}
