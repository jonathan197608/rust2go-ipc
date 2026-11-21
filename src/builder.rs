//! Build-time API mirroring `rust2go::Builder`. Instead of cgo + bindgen, this
//! parses the Rust schema and emits pure-Go sources plus Rust binding mirrors.

use std::{collections::HashSet, env, ffi::OsString, fs, path::PathBuf, process::Command};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Fields, FnArg, Item, ItemStruct, Pat, ReturnType, TraitItem, Type};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    Static,
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CopyLib {
    Disabled,
    DefaultPath,
    CustomPath(PathBuf),
}

pub struct Builder<GOSRC = (), GOC = CustomArgGoCompiler> {
    go_src: GOSRC,
    binding_name: Option<String>,
    regen_src: String,
    regen_dst: String,
    copy_lib: CopyLib,
    go_comp: GOC,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            go_src: (),
            binding_name: None,
            regen_src: String::new(),
            regen_dst: String::new(),
            copy_lib: CopyLib::Disabled,
            go_comp: CustomArgGoCompiler::new(),
        }
    }
}

impl<GOSRC, GOC> Builder<GOSRC, GOC> {
    pub fn with_go_src<S: Into<PathBuf>>(self, go_src: S) -> Builder<PathBuf, GOC> {
        Builder {
            go_src: go_src.into(),
            binding_name: self.binding_name,
            regen_src: self.regen_src,
            regen_dst: self.regen_dst,
            copy_lib: self.copy_lib,
            go_comp: self.go_comp,
        }
    }

    pub fn with_go_compiler<GOC2>(self, go_comp: GOC2) -> Builder<GOSRC, GOC2> {
        Builder {
            go_src: self.go_src,
            binding_name: self.binding_name,
            regen_src: self.regen_src,
            regen_dst: self.regen_dst,
            copy_lib: self.copy_lib,
            go_comp,
        }
    }

    pub fn go_compiler_mut(&mut self) -> &mut GOC {
        &mut self.go_comp
    }

    pub fn with_binding(mut self, name: impl Into<String>) -> Self {
        self.binding_name = Some(name.into());
        self
    }

    pub fn with_link(self, _link: LinkType) -> Self {
        self
    }

    pub fn with_copy_lib(mut self, copy_lib: CopyLib) -> Self {
        self.copy_lib = copy_lib;
        self
    }

    pub fn with_regen(mut self, src: &str, dst: &str) -> Self {
        self.regen_src = src.to_string();
        self.regen_dst = dst.to_string();
        self
    }

    pub fn with_regen_arg(mut self, arg: RegenArgs) -> Self {
        self.regen_src = arg.src;
        self.regen_dst = arg.dst;
        self
    }
}

#[derive(Debug, Default, Clone)]
pub struct RegenArgs {
    pub src: String,
    pub dst: String,
}

impl<GOSRC, GOC> Builder<GOSRC, GOC> {
    pub fn compiler_arg(&mut self, arg: impl Into<OsString>) -> &mut Self
    where
        GOC: GoCompilerArgs,
    {
        self.go_comp.push_arg(arg.into());
        self
    }
}

pub trait GoCompilerArgs {
    fn push_arg(&mut self, arg: OsString);
}

#[derive(Debug, Clone, Copy)]
pub struct DefaultGoCompiler;
pub trait GoCompiler {}

impl GoCompiler for DefaultGoCompiler {}
impl GoCompiler for CustomArgGoCompiler {}

#[derive(Debug, Clone)]
pub struct CustomArgGoCompiler {
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
}

impl Default for CustomArgGoCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl CustomArgGoCompiler {
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            envs: vec![(OsString::from("GO111MODULE"), OsString::from("on"))],
        }
    }

    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }

    pub fn env(&mut self, key: impl Into<OsString>, val: impl Into<OsString>) -> &mut Self {
        self.envs.push((key.into(), val.into()));
        self
    }
}

impl GoCompilerArgs for CustomArgGoCompiler {
    fn push_arg(&mut self, arg: OsString) {
        self.args.push(arg);
    }
}

impl Builder<PathBuf, CustomArgGoCompiler> {
    /// Generate pure-Go sources and Rust binding mirrors, then compile the
    /// Go IPC server.
    pub fn build(self) {
        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
        let manifest =
            PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
        let src_rs = if self.regen_src.is_empty() {
            manifest.join("src/golang.rs")
        } else {
            PathBuf::from(&self.regen_src)
        };
        let go_dir = self.go_src.clone();
        let regen_dst = if self.regen_dst.is_empty() {
            go_dir.join("gen.go")
        } else {
            PathBuf::from(&self.regen_dst)
        };
        // `go build .` always runs in `go_dir`; writing gen.go elsewhere would
        // silently compile a stale file. Fail loudly instead.
        assert!(
            regen_dst.parent() == Some(go_dir.as_path()),
            "rust2go-ipc: regen dst must be inside go_src ({} vs {})",
            regen_dst.display(),
            go_dir.display()
        );

        println!("cargo:rerun-if-changed={}", src_rs.display());
        if let Ok(entries) = fs::read_dir(&go_dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("go") {
                    println!("cargo:rerun-if-changed={}", p.display());
                }
            }
        }

        let schema = parse_schema(&fs::read_to_string(&src_rs).expect("cannot read schema source"));
        let binding_name = self.binding_name.as_deref().unwrap_or("_go_bindings.rs");
        let port = crate::ipc::free_port();

        let rust = generate_rust_bindings(&schema, port);
        fs::write(out_dir.join(binding_name), rust.to_string()).expect("cannot write bindings");

        let go_gen = generate_go_sources(&schema);
        fs::write(&regen_dst, &go_gen).expect("cannot write Go sources");

        let ipc_main = go_dir.join("ipc_main.go");
        let main_src = generate_go_main();
        if fs::read_to_string(&ipc_main)
            .map(|old| old != main_src)
            .unwrap_or(false)
        {
            println!(
                "cargo:warning=rust2go-ipc: overwriting {} with the generated IPC server",
                ipc_main.display()
            );
        }
        fs::write(&ipc_main, main_src).expect("cannot write Go main");

        // Keep generated Go sources gofmt-clean when the tool is available.
        let gofmt = find_gofmt();
        for path in [&regen_dst, &ipc_main] {
            let result = match &gofmt {
                Some(p) => Command::new(p).arg("-w").arg(path).status(),
                None => Command::new("gofmt").arg("-w").arg(path).status(),
            };
            match result {
                Ok(status) if status.success() => {}
                Ok(status) => println!(
                    "cargo:warning=rust2go-ipc: gofmt exited with {status} for {}",
                    path.display()
                ),
                Err(e) => println!(
                    "cargo:warning=rust2go-ipc: cannot run gofmt for {}: {e}",
                    path.display()
                ),
            }
        }

        let server_out = out_dir.join("go-ipc-server");
        let mut cmd = Command::new("go");
        // Real projects may depend on cgo-only packages. The Go server is a
        // separate process, so cgo here does not affect the Rust host.
        cmd.current_dir(&go_dir)
            .env("GO111MODULE", "on")
            .arg("build")
            .arg("-o")
            .arg(&server_out);
        for (k, v) in &self.go_comp.envs {
            cmd.env(k, v);
        }
        for a in &self.go_comp.args {
            cmd.arg(a);
        }
        cmd.arg(".");
        let status = cmd.status().expect("go build failed to start");
        assert!(status.success(), "go build failed");

        if let Some(profile_dir) = out_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            let stable = profile_dir.join("go-ipc-server");
            if let Err(e) = fs::copy(&server_out, &stable) {
                println!(
                    "cargo:warning=rust2go-ipc: cannot copy server to {}: {e}",
                    stable.display()
                );
            }
        }

        let server_lit = server_out.to_string_lossy().replace('\\', "/");
        println!("cargo:rustc-env=RUST2GO_IPC_SERVER={server_lit}");
        println!("cargo:rustc-env=RUST2GO_IPC_PORT={port}");
    }
}

/// Locate gofmt: prefer PATH, then `$GOROOT/bin/gofmt`, then `go env GOROOT`.
fn find_gofmt() -> Option<PathBuf> {
    if Command::new("gofmt")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("gofmt"));
    }
    if let Ok(root) = env::var("GOROOT") {
        let p = PathBuf::from(root).join("bin").join("gofmt");
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(out) = Command::new("go").args(["env", "GOROOT"]).output() {
        if out.status.success() {
            let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !root.is_empty() {
                let p = PathBuf::from(root).join("bin").join("gofmt");
                if p.exists() {
                    return Some(p);
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Schema model
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FieldDef {
    name: String,
    ty: Type,
}

#[derive(Clone)]
struct StructDef {
    name: String,
    fields: Vec<FieldDef>,
}

#[derive(Clone)]
struct ParamDef {
    name: String,
    ty: Type,
}

#[derive(Clone)]
struct MethodDef {
    name: String,
    trait_name: String,
    params: Vec<ParamDef>,
    resp: Option<Type>,
    is_async: bool,
    go_ptr: bool,
    index: u32,
}

#[derive(Default)]
struct Schema {
    structs: Vec<StructDef>,
    methods: Vec<MethodDef>,
    traits: Vec<String>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum FieldKind {
    Primitive(String),
    String,
    List(Box<FieldKind>),
    Custom(String),
}

fn parse_schema(src: &str) -> Schema {
    let file = syn::parse_file(src).expect("rust2go-ipc: cannot parse schema source");
    let mut schema = Schema::default();

    for item in &file.items {
        if let Item::Struct(s) = item {
            if has_derive(s, "R2G") {
                schema.structs.push(parse_struct(s));
            }
        }
    }
    let struct_names: HashSet<String> = schema.structs.iter().map(|s| s.name.clone()).collect();

    // Method IDs are global: multiple #[r2g] traits must not produce duplicate
    // Go `case` labels.
    let mut idx = 0u32;
    let mut stub_names = HashSet::new();
    for item in &file.items {
        let Item::Trait(t) = item else { continue };
        if !has_marker_attr(&t.attrs, "r2g") {
            continue;
        }
        let trait_name = bare_ident(&t.ident);
        validate_ident(&trait_name, "trait");
        if !schema.traits.contains(&trait_name) {
            schema.traits.push(trait_name.clone());
        }
        for m in &t.items {
            let TraitItem::Fn(f) = m else { continue };
            let name = bare_ident(&f.sig.ident);
            validate_ident(&name, "method");
            let mut params = Vec::new();
            for arg in &f.sig.inputs {
                let FnArg::Typed(p) = arg else {
                    panic!("rust2go-ipc: receiver is not supported in #[r2g] trait methods");
                };
                let pname = match &*p.pat {
                    Pat::Ident(i) => bare_ident(&i.ident),
                    _ => panic!("rust2go-ipc: only simple parameter patterns are supported"),
                };
                validate_ident(&pname, "parameter");
                let (ty, is_reference) = strip_reference(&p.ty);
                let _ = is_reference;
                params.push(ParamDef { name: pname, ty });
            }
            let resp = match &f.sig.output {
                ReturnType::Type(_, ty) => Some(strip_reference(ty).0),
                ReturnType::Default => None,
            };
            let go_ptr = !f
                .attrs
                .iter()
                .any(|a| has_last_segment(a.path(), "go_pass_struct"));
            let stub = format!("C{trait_name}_{name}");
            assert!(
                stub_names.insert(stub.clone()),
                "rust2go-ipc: duplicate stub symbol {stub} (duplicate #[r2g] method)"
            );
            schema.methods.push(MethodDef {
                name,
                trait_name: trait_name.clone(),
                params,
                resp,
                is_async: f.sig.asyncness.is_some(),
                go_ptr,
                index: idx,
            });
            idx += 1;
        }
    }

    for st in &schema.structs {
        validate_ident(&st.name, "struct");
        if matches!(st.name.as_str(), "StringRef" | "ListRef" | "enc" | "dec") {
            panic!(
                "rust2go-ipc: struct name {} conflicts with generated aliases",
                st.name
            );
        }
        for f in &st.fields {
            validate_ident(&f.name, "field");
            let kind = classify(&f.ty);
            check_supported(&kind, &format!("{}.{}", st.name, f.name), &struct_names);
        }
    }
    // A list of zero-width structs cannot round-trip: each element occupies no
    // wire bytes, so a non-empty list is indistinguishable from an empty one
    // and the count bound rejects every value. Fail at build time with a clear
    // message instead of a confusing runtime "list length out of bounds".
    let empty_structs: HashSet<String> = schema
        .structs
        .iter()
        .filter(|s| s.fields.is_empty())
        .map(|s| s.name.clone())
        .collect();
    if !empty_structs.is_empty() {
        let check = |kind: &FieldKind, ctx: &str| {
            if let FieldKind::List(elem) = kind {
                if let FieldKind::Custom(n) = &**elem {
                    assert!(
                        !empty_structs.contains(n),
                        "rust2go-ipc: list of empty struct {n} at {ctx} is not wire-encodable"
                    );
                }
            }
        };
        for st in &schema.structs {
            for f in &st.fields {
                check(&classify(&f.ty), &format!("{}.{}", st.name, f.name));
            }
        }
        for m in &schema.methods {
            for p in &m.params {
                check(&classify(&p.ty), &format!("{}.{}", m.trait_name, p.name));
            }
            if let Some(ty) = &m.resp {
                check(&classify(ty), &format!("{} return", m.trait_name));
            }
        }
    }

    for m in &schema.methods {
        for p in &m.params {
            let kind = classify(&p.ty);
            check_supported(
                &kind,
                &format!("{}.{}", m.trait_name, p.name),
                &struct_names,
            );
        }
        if let Some(ty) = &m.resp {
            let kind = classify(ty);
            check_supported(&kind, &format!("{} return", m.trait_name), &struct_names);
        }
    }
    check_go_name_collisions(&schema);
    schema
}

fn path_last_segment(path: &syn::Path) -> String {
    path.segments
        .last()
        .map(|s| bare_ident(&s.ident))
        .unwrap_or_default()
}

fn has_last_segment(path: &syn::Path, name: &str) -> bool {
    path_last_segment(path) == name
}

fn has_marker_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|a| has_last_segment(a.path(), name))
}

fn has_derive(s: &ItemStruct, name: &str) -> bool {
    s.attrs
        .iter()
        .filter(|a| a.path().is_ident("derive"))
        .any(|a| {
            if let syn::Meta::List(list) = &a.meta {
                list.tokens
                    .to_string()
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .any(|s| s.rsplit("::").next() == Some(name))
            } else {
                false
            }
        })
}

/// Strip a Rust raw-identifier prefix (`r#foo` -> `foo`). Validation already
/// operates on the bare form; generation must use the same bare form or it
/// would emit Go/Rust identifiers containing an illegal `#`.
fn bare_ident(ident: &proc_macro2::Ident) -> String {
    let s = ident.to_string();
    s.strip_prefix("r#").unwrap_or(&s).to_string()
}

fn parse_struct(s: &ItemStruct) -> StructDef {
    let name = bare_ident(&s.ident);
    let fields = match &s.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let fname = bare_ident(f.ident.as_ref().expect("named field"));
                FieldDef {
                    name: fname,
                    ty: strip_reference(&f.ty).0,
                }
            })
            .collect(),
        // Treating tuple/unit structs as zero-field would silently drop their
        // data. Reject them at build time instead of generating broken code.
        Fields::Unnamed(_) | Fields::Unit => panic!(
            "rust2go-ipc: struct `{name}` must have named fields; tuple and unit structs are not supported"
        ),
    };
    StructDef { name, fields }
}

fn strip_reference(ty: &Type) -> (Type, bool) {
    if let Type::Reference(r) = ty {
        ((*r.elem).clone(), true)
    } else {
        (ty.clone(), false)
    }
}

fn type_ident(ty: &Type) -> String {
    match &strip_reference(ty).0 {
        Type::Path(p) => path_last_segment(&p.path),
        _ => String::new(),
    }
}

fn generic_arg(ty: &Type) -> Option<Type> {
    let Type::Path(p) = &strip_reference(ty).0 else {
        return None;
    };
    let seg = p.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

fn classify(ty: &Type) -> FieldKind {
    match type_ident(ty).as_str() {
        "String" => FieldKind::String,
        "Vec" | "Option" => FieldKind::List(Box::new(
            generic_arg(ty)
                .map(|t| classify(&t))
                .unwrap_or(FieldKind::Primitive("u8".to_string())),
        )),
        "bool" | "i8" | "i16" | "i32" | "i64" | "i128" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "f32" | "f64" | "usize" | "isize" | "char" => FieldKind::Primitive(type_ident(ty)),
        other => FieldKind::Custom(other.to_string()),
    }
}

fn check_supported(kind: &FieldKind, ctx: &str, structs: &HashSet<String>) {
    match kind {
        FieldKind::Primitive(p) if p == "i128" || p == "u128" => {
            panic!("rust2go-ipc: {p} field at {ctx} is not wire-compatible")
        }
        FieldKind::List(elem) => {
            if matches!(**elem, FieldKind::List(_)) {
                panic!("rust2go-ipc: nested list field at {ctx} is not supported");
            }
            check_supported(elem, ctx, structs);
        }
        FieldKind::Custom(name) => {
            assert!(
                structs.contains(name),
                "rust2go-ipc: unknown struct type {name} at {ctx} (missing #[derive(R2G)]?)"
            );
        }
        _ => {}
    }
}

fn validate_ident(name: &str, what: &str) {
    const RUST: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
    ];
    const GO: &[&str] = &[
        "break",
        "default",
        "func",
        "interface",
        "select",
        "case",
        "defer",
        "go",
        "map",
        "struct",
        "chan",
        "else",
        "goto",
        "package",
        "switch",
        "const",
        "fallthrough",
        "if",
        "range",
        "type",
        "continue",
        "for",
        "import",
        "return",
        "var",
    ];
    let bare = name.strip_prefix("r#").unwrap_or(name);
    assert!(
        !RUST.contains(&bare) && !GO.contains(&bare) && !bare.is_empty(),
        "rust2go-ipc: {what} name `{name}` is a reserved identifier"
    );
}

/// Top-level Go identifiers emitted by the generated server (helpers, types,
/// constants) plus Go predeclared identifiers. A schema name that maps onto any
/// of these would emit a duplicate declaration and break `go build`; reject it
/// at build time with a clear message.
const GO_FIXED_SYMBOLS: &[&str] = &[
    // gen.go fixed types and helpers
    "StringRef",
    "ListRef",
    "enc",
    "dec",
    "newString",
    "decodePrims",
    "newPrimList",
    "newStringList",
    "primList",
    "stringList",
    "dispatch",
    // ipc_main.go fixed symbols
    "magic",
    "version",
    "maxAcceptErrs",
    "sem",
    "acceptBackoff",
    "main",
    "serve",
    "init",
    // Go predeclared identifiers that a same-named type would shadow
    "string",
    "error",
    "any",
    "byte",
    "rune",
    "bool",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "true",
    "false",
    "iota",
    "nil",
    "append",
    "cap",
    "close",
    "complex",
    "copy",
    "delete",
    "imag",
    "len",
    "make",
    "new",
    "panic",
    "print",
    "println",
    "real",
    "recover",
];

/// Verify that no two generated top-level Go symbols collide. User-supplied
/// struct/trait names expand into several helpers each (`newX`, `XRef`, ...),
/// so collisions can arise both with fixed helpers and between two schema
/// types. It is cheaper to reject at build time than to debug a `go build`
/// redeclaration error.
fn check_go_name_collisions(schema: &Schema) {
    use std::collections::HashMap;
    let mut seen: HashMap<String, String> = HashMap::new();
    for s in GO_FIXED_SYMBOLS {
        seen.insert(
            (*s).to_string(),
            "a generated helper / Go predeclared name".to_string(),
        );
    }
    let mut claim = |name: String, owner: &str| {
        if let Some(prev) = seen.get(&name) {
            panic!(
                "rust2go-ipc: generated Go symbol `{name}` from {owner} collides with {prev}; rename the schema type"
            );
        }
        seen.insert(name, owner.to_string());
    };
    for st in &schema.structs {
        let n = &st.name;
        let owner = format!("struct {n}");
        for sym in [
            n.clone(),
            format!("{n}Ref"),
            format!("new{n}"),
            format!("new{n}List"),
            format!("flatten{n}"),
            format!("restore{n}Ref"),
            format!("{n}ListRef"),
        ] {
            claim(sym, &owner);
        }
    }
    for t in &schema.traits {
        let owner = format!("trait {t}");
        claim(t.clone(), &owner);
        claim(format!("{t}Impl"), &owner);
    }
    // Mirror generate_go_sources' top-level helper derivation for non-struct types.
    let mut tops: Vec<String> = Vec::new();
    for m in &schema.methods {
        for p in &m.params {
            if !matches!(classify(&p.ty), FieldKind::Custom(_)) {
                let n = top_name(&p.ty);
                if !tops.contains(&n) {
                    tops.push(n);
                }
            }
        }
        if let Some(ty) = &m.resp {
            if !matches!(classify(ty), FieldKind::Custom(_)) {
                let n = top_name(ty);
                if !tops.contains(&n) {
                    tops.push(n);
                }
            }
        }
    }
    for n in tops {
        claim(format!("restoreTop{n}"), "a top-level helper");
        claim(format!("newTop{n}"), "a top-level helper");
    }
}

fn rust_owned_kind_path(kind: &FieldKind) -> TokenStream {
    match kind {
        FieldKind::String => quote! { ::std::string::String },
        FieldKind::List(elem) => {
            let inner = rust_owned_kind_path(elem);
            quote! { ::std::vec::Vec<#inner> }
        }
        FieldKind::Custom(n) => {
            let id = format_ident!("{}", n);
            quote! { super::#id }
        }
        FieldKind::Primitive(p) => {
            let id = format_ident!("{}", p);
            quote! { #id }
        }
    }
}

/// Owned type as seen from the generated `binding` module. Schema structs live
/// in the parent module, hence the `super::` prefix (the generated file is
/// included from a sibling `binding` module).
fn rust_owned_path(ty: &Type) -> TokenStream {
    match classify(ty) {
        FieldKind::List(_) if type_ident(ty) == "Option" => {
            let elem = generic_arg(ty).expect("Option<T>");
            let inner = rust_owned_path(&elem);
            quote! { ::std::option::Option<#inner> }
        }
        other => rust_owned_kind_path(&other),
    }
}

fn rust_ref_type(ty: &Type) -> TokenStream {
    match classify(ty) {
        FieldKind::String => quote! { ::rust2go::ipc::RawStringRef },
        FieldKind::List(_) => quote! { ::rust2go::ipc::RawListRef },
        FieldKind::Custom(n) => {
            let id = format_ident!("{}Ref", n);
            quote! { #id }
        }
        FieldKind::Primitive(p) => {
            let id = format_ident!("{}", p);
            quote! { #id }
        }
    }
}

// ---------------------------------------------------------------------------
// Rust binding generation
// ---------------------------------------------------------------------------

fn flatten_expr(ty: &Type, value: TokenStream) -> TokenStream {
    let r = format_ident!("__r2g_r");
    match classify(ty) {
        FieldKind::String => quote! { enc.string(#value); },
        FieldKind::Primitive(p) if p == "bool" => quote! { enc.prim(#value); },
        FieldKind::Primitive(_) => quote! { enc.prim(#value); },
        FieldKind::Custom(n) => {
            let f = format_ident!("__r2g_flatten_{}", n);
            quote! { #f(enc, &#value); }
        }
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(p) => {
                let t = format_ident!("{}", p);
                quote! {
                    {
                        let #r = #value;
                        enc.prim_list::<#t>(#r.ptr, #r.len);
                    }
                }
            }
            FieldKind::String => quote! {
                {
                    let #r = #value;
                    enc.list_header(#r.len);
                    if #r.len > 0 {
                        // rust2go packs child Refs into a `Vec<u8>` with 1-byte
                        // alignment via write_unaligned. Read each element
                        // unaligned instead of forming an over-aligned slice,
                        // which would be UB under strict/custom allocators.
                        let base = #r.ptr as *const ::rust2go::ipc::RawStringRef;
                        for i in 0..#r.len {
                            let item = ::std::ptr::read_unaligned(base.add(i));
                            enc.string(item);
                        }
                    }
                }
            },
            FieldKind::Custom(n) => {
                let f = format_ident!("__r2g_flatten_{}", n);
                let rt = format_ident!("{}Ref", n);
                quote! {
                    {
                        let #r = #value;
                        enc.list_header(#r.len);
                        if #r.len > 0 {
                            let base = #r.ptr as *const #rt;
                            for i in 0..#r.len {
                                let item = ::std::ptr::read_unaligned(base.add(i));
                                #f(enc, &item);
                            }
                        }
                    }
                }
            }
            FieldKind::List(_) => unreachable!("nested lists rejected by check_supported"),
        },
    }
}

fn min_wire_width(kind: &FieldKind, widths: &std::collections::HashMap<String, usize>) -> usize {
    match kind {
        FieldKind::String | FieldKind::List(_) => 8,
        FieldKind::Primitive(p) => match p.as_str() {
            "bool" | "i8" | "u8" => 1,
            "i16" | "u16" => 2,
            "i32" | "u32" | "f32" | "char" => 4,
            _ => 8,
        },
        FieldKind::Custom(n) => *widths.get(n).unwrap_or(&1),
    }
}

fn struct_wire_widths(schema: &Schema) -> std::collections::HashMap<String, usize> {
    let mut widths: std::collections::HashMap<String, usize> =
        schema.structs.iter().map(|s| (s.name.clone(), 1)).collect();
    for _ in 0..8 {
        let mut changed = false;
        for st in &schema.structs {
            let w: usize = st
                .fields
                .iter()
                .map(|f| min_wire_width(&classify(&f.ty), &widths))
                .sum();
            if widths.get(&st.name) != Some(&w) {
                widths.insert(st.name.clone(), w);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    widths
}

fn restore_expr(ty: &Type, widths: &std::collections::HashMap<String, usize>) -> TokenStream {
    match classify(ty) {
        FieldKind::String => quote! { dec.string() },
        FieldKind::Primitive(_) => quote! { dec.prim() },
        FieldKind::Custom(n) => {
            let f = format_ident!("__r2g_restore_{}", n);
            quote! { #f(dec) }
        }
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(p) => {
                let t = format_ident!("{}", p);
                quote! { dec.prim_list::<#t>() }
            }
            FieldKind::String => quote! {
                {
                    // A wire string is [u64 length][bytes], so the minimum
                    // encoded width is 8 (empty string) — not the 16-byte
                    // in-memory size of RawStringRef. Using the latter would
                    // reject valid lists of short strings.
                    let n = dec.list_len_min(8);
                    let mut items = Vec::with_capacity(n);
                    for _ in 0..n {
                        items.push(dec.string());
                    }
                    dec.ref_list(items)
                }
            },
            FieldKind::Custom(n) => {
                let f = format_ident!("__r2g_restore_{}", n);
                let min_wire = widths.get(&n).copied().unwrap_or(1);
                quote! {
                    {
                        let min_wire = #min_wire;
                        let n = dec.list_len_min(min_wire);
                        let mut items = Vec::with_capacity(n);
                        for _ in 0..n {
                            items.push(#f(dec));
                        }
                        dec.ref_list(items)
                    }
                }
            }
            FieldKind::List(_) => unreachable!("nested lists rejected by check_supported"),
        },
    }
}

fn generate_rust_bindings(schema: &Schema, port: u16) -> TokenStream {
    let widths = struct_wire_widths(schema);
    let mut out = TokenStream::new();
    out.extend(quote! {
        // Generated by rust2go-ipc. Do not edit.
        const _: () = assert!(
            ::std::mem::size_of::<usize>() == 8,
            "rust2go-ipc requires a 64-bit target (wire usize is 8 bytes)"
        );
        pub type StringRef = ::rust2go::ipc::RawStringRef;
        pub type ListRef = ::rust2go::ipc::RawListRef;
    });

    for st in &schema.structs {
        let name = format_ident!("{}Ref", st.name);
        let fields = st.fields.iter().map(|f| {
            let id = format_ident!("{}", f.name);
            let ty = rust_ref_type(&f.ty);
            quote! { pub #id: #ty }
        });
        out.extend(quote! {
            #[repr(C)]
            #[derive(Copy, Clone)]
            #[allow(non_snake_case)]
            pub struct #name { #(#fields),* }
        });
    }

    for st in &schema.structs {
        let name = format_ident!("__r2g_flatten_{}", st.name);
        let st_name = format_ident!("{}Ref", st.name);
        let fields: Vec<TokenStream> = st
            .fields
            .iter()
            .map(|f| {
                let id = format_ident!("{}", f.name);
                flatten_expr(&f.ty, quote! { v.#id })
            })
            .collect();
        out.extend(quote! {
            #[allow(non_snake_case, clippy::all, unused_variables)]
            unsafe fn #name(enc: &mut ::rust2go::ipc::Encoder, v: &#st_name) {
                #(#fields)*
            }
        });
    }

    for st in &schema.structs {
        let name = format_ident!("__r2g_restore_{}", st.name);
        let st_name = format_ident!("{}Ref", st.name);
        let fields: Vec<TokenStream> = st
            .fields
            .iter()
            .map(|f| {
                let id = format_ident!("{}", f.name);
                let expr = restore_expr(&f.ty, &widths);
                quote! { #id: #expr, }
            })
            .collect();
        out.extend(quote! {
            #[allow(non_snake_case, clippy::all, unused_variables)]
            unsafe fn #name(dec: &mut ::rust2go::ipc::Decoder<'_>) -> #st_name {
                #st_name { #(#fields)* }
            }
        });
    }

    for m in &schema.methods {
        let stub = format_ident!("C{}_{}", m.trait_name, m.name);
        // Include the trait in helper names: two #[r2g] traits may define
        // methods with the same name (method_id is global, symbols are not).
        let tramp = format_ident!("__r2g_trampoline_{}_{}", m.trait_name, m.name);
        let cleanup = format_ident!("__r2g_cleanup_{}_{}", m.trait_name, m.name);
        let method_id = m.index;
        let param_decls: Vec<TokenStream> = m
            .params
            .iter()
            .map(|p| {
                let id = format_ident!("{}", p.name);
                let ty = rust_ref_type(&p.ty);
                quote! { #id: #ty }
            })
            .collect();
        let flatten_stmts: Vec<TokenStream> = m
            .params
            .iter()
            .map(|p| {
                let id = format_ident!("{}", p.name);
                flatten_expr(&p.ty, quote! { #id })
            })
            .collect();

        match (&m.resp, m.is_async) {
            (Some(resp), is_async) => {
                let resp_ref = rust_ref_type(resp);
                let resp_owned = rust_owned_path(resp);
                let restore = restore_expr(resp, &widths);
                let slot_cb = quote! {
                    slot: *mut ::std::ffi::c_void,
                    cb: *mut ::std::ffi::c_void,
                };
                let cleanup_arg = if is_async {
                    quote! { Some(#cleanup) }
                } else {
                    quote! { None }
                };
                let call = if is_async {
                    quote! { ::rust2go::ipc::dispatch }
                } else {
                    quote! { ::rust2go::ipc::dispatch_sync }
                };
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    pub unsafe fn #stub(#(#param_decls,)* #slot_cb) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        {
                            let enc = &mut enc;
                            #(#flatten_stmts)*
                        }
                        let handler = ::rust2go::ipc::ResponseHandler::new(#tramp, #cleanup_arg);
                        #call(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                            slot as *const (),
                            cb as *const (),
                            handler,
                        );
                    }
                });

                let owned_types: Vec<TokenStream> =
                    m.params.iter().map(|p| rust_owned_path(&p.ty)).collect();
                let write_back = if is_async {
                    quote! {
                        ::rust2go::SlotWriter::<#resp_owned, ((#(#owned_types,)*), Vec<u8>)>
                            ::from_ptr(slot)
                            .write(::rust2go::FromRef::from_ref(::std::mem::transmute(&resp)));
                    }
                } else {
                    quote! {
                        *(slot as *mut Option<#resp_owned>) =
                            Some(::rust2go::FromRef::from_ref(::std::mem::transmute(&resp)));
                    }
                };
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    unsafe extern "C-unwind" fn #tramp(
                        payload: *const u8,
                        len: usize,
                        slot: *const (),
                        _cb: *const (),
                    ) {
                        let mut dec = ::rust2go::ipc::Decoder::new(
                            ::std::slice::from_raw_parts(payload, len)
                        );
                        let resp: #resp_ref = {
                            let dec = &mut dec;
                            #restore
                        };
                        #write_back
                    }
                });
                if is_async {
                    out.extend(quote! {
                        #[allow(non_snake_case, clippy::missing_safety_doc)]
                        unsafe fn #cleanup(slot: *const ()) {
                            drop(::rust2go::SlotWriter::<
                                #resp_owned,
                                ((#(#owned_types,)*), Vec<u8>),
                            >::from_ptr(slot));
                        }
                    });
                }
            }
            (None, _) => {
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    pub unsafe fn #stub(#(#param_decls,)*) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        {
                            let enc = &mut enc;
                            #(#flatten_stmts)*
                        }
                        ::rust2go::ipc::dispatch_oneway(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                        );
                    }
                });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Go generation
// ---------------------------------------------------------------------------

fn go_prim(p: &str) -> &'static str {
    match p {
        "bool" => "bool",
        "i8" => "int8",
        "i16" => "int16",
        "i32" => "int32",
        "i64" => "int64",
        "i128" => panic!("rust2go-ipc: go_prim called for i128, which check_supported must reject"),
        "u8" => "uint8",
        "u16" => "uint16",
        "u32" => "uint32",
        "u64" => "uint64",
        "u128" => panic!("rust2go-ipc: go_prim called for u128, which check_supported must reject"),
        "f32" => "float32",
        "f64" => "float64",
        "usize" => "uint64",
        "isize" => "int64",
        "char" => "uint32",
        _ => "uint64",
    }
}

fn go_owned_prim(p: &str) -> &'static str {
    match p {
        "usize" => "uint",
        "isize" => "int",
        "char" => "rune",
        _ => go_prim(p),
    }
}

fn go_type(ty: &Type, owned: bool) -> String {
    match classify(ty) {
        FieldKind::String => "string".to_string(),
        FieldKind::List(elem) => format!("[]{}", go_type_inner(&elem, owned)),
        FieldKind::Primitive(p) => {
            if owned {
                go_owned_prim(&p).to_string()
            } else {
                go_prim(&p).to_string()
            }
        }
        FieldKind::Custom(n) => n,
    }
}

fn go_type_inner(kind: &FieldKind, owned: bool) -> String {
    match kind {
        FieldKind::String => "string".to_string(),
        FieldKind::List(elem) => format!("[]{}", go_type_inner(elem, owned)),
        FieldKind::Primitive(p) => {
            if owned {
                go_owned_prim(p).to_string()
            } else {
                go_prim(p).to_string()
            }
        }
        FieldKind::Custom(n) => n.clone(),
    }
}

fn go_ref_type(ty: &Type) -> String {
    match classify(ty) {
        FieldKind::String => "StringRef".to_string(),
        FieldKind::List(_) => "ListRef".to_string(),
        FieldKind::Primitive(p) => go_prim(&p).to_string(),
        FieldKind::Custom(n) => format!("{n}Ref"),
    }
}

/// Stable Go helper name for a non-struct top-level type.
fn top_name(ty: &Type) -> String {
    match classify(ty) {
        FieldKind::String => "String".to_string(),
        FieldKind::Primitive(p) => p,
        FieldKind::List(elem) => format!("List_{}", top_name_kind(&elem)),
        FieldKind::Custom(n) => n,
    }
}

fn top_name_kind(kind: &FieldKind) -> String {
    match kind {
        FieldKind::String => "String".to_string(),
        FieldKind::Primitive(p) => p.clone(),
        FieldKind::List(elem) => format!("List_{}", top_name_kind(elem)),
        FieldKind::Custom(n) => n.clone(),
    }
}

fn go_restore_expr(
    ty: &Type,
    d: &str,
    _widths: &std::collections::HashMap<String, usize>,
) -> String {
    match classify(ty) {
        FieldKind::String => format!("{d}.str()"),
        FieldKind::Primitive(p) => match p.as_str() {
            "bool" => format!("{d}.boolean()"),
            "i8" => format!("{d}.i8()"),
            "i16" => format!("{d}.i16()"),
            "i32" => format!("{d}.i32()"),
            "i64" => format!("{d}.i64()"),
            "u8" => format!("{d}.u8()"),
            "u16" => format!("{d}.u16()"),
            "u32" | "char" => format!("{d}.u32()"),
            "u64" | "usize" => format!("{d}.u64()"),
            "isize" => format!("{d}.i64()"),
            "f32" => format!("{d}.f32()"),
            "f64" => format!("{d}.f64()"),
            _ => panic!("rust2go-ipc: unsupported go primitive {p}"),
        },
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(_) => format!("primList[{}]({d})", go_type_inner(&elem, true)),
            FieldKind::String => format!("stringList({d})"),
            FieldKind::Custom(n) => format!("{n}ListRef({d})"),
            FieldKind::List(_) => unreachable!(),
        },
        FieldKind::Custom(n) => format!("restore{n}Ref({d})"),
    }
}

fn go_owned_expr(ty: &Type, var: &str) -> String {
    match classify(ty) {
        FieldKind::Custom(n) => format!("new{}({})", n, var),
        FieldKind::String => format!("newString({var})"),
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(_) => {
                format!("newPrimList[{}]({var})", go_type_inner(&elem, true))
            }
            FieldKind::String => format!("newStringList({var})"),
            FieldKind::Custom(n) => format!("new{n}List({var})"),
            FieldKind::List(_) => unreachable!(),
        },
        FieldKind::Primitive(p) if p == "usize" || p == "isize" || p == "char" => {
            format!("{}({var})", go_owned_prim(&p))
        }
        FieldKind::Primitive(_) => var.to_string(),
    }
}

fn generate_go_sources(schema: &Schema) -> String {
    let widths = struct_wire_widths(schema);
    let mut s = String::new();
    s.push_str("// Code generated by rust2go-ipc. DO NOT EDIT.\npackage main\n\n");
    s.push_str("import (\n\t\"encoding/binary\"\n\t\"fmt\"\n\t\"math\"\n\t\"os\"\n\t\"strings\"\n\t\"unsafe\"\n)\n\n");
    s.push_str("type StringRef struct {\n\tptr unsafe.Pointer\n\tlen uint64\n}\n\n");
    s.push_str("type ListRef struct {\n\tptr unsafe.Pointer\n\tlen uint64\n}\n\n");

    // Fixed helpers. `decodePrims` always copies into an aligned, owned slice,
    // avoiding both overflow in n*sz and unsafe misaligned pointer casts.
    s.push_str(r#"
func newString(r StringRef) string {
	if r.len == 0 {
		return ""
	}
	return strings.Clone(unsafe.String((*byte)(r.ptr), int(r.len)))
}

func decodePrims[T any](b []byte, n int) []T {
	var zero T
	sz := int(unsafe.Sizeof(zero))
	if n <= 0 {
		return nil
	}
	if n > len(b)/sz {
		panic("rust2go-ipc: primitive list out of bounds")
	}
	out := make([]T, n)
	switch any(zero).(type) {
	case bool:
		for i := 0; i < n; i++ {
			v := b[i]
			if v > 1 {
				panic("rust2go-ipc: invalid bool byte in list")
			}
			out[i] = any(v != 0).(T)
		}
	case int8:
		for i := 0; i < n; i++ { out[i] = any(int8(b[i])).(T) }
	case uint8:
		for i := 0; i < n; i++ { out[i] = any(b[i]).(T) }
	case int16:
		for i := 0; i < n; i++ { out[i] = any(int16(binary.LittleEndian.Uint16(b[i*2:]))).(T) }
	case uint16:
		for i := 0; i < n; i++ { out[i] = any(binary.LittleEndian.Uint16(b[i*2:])).(T) }
	case int32:
		for i := 0; i < n; i++ { out[i] = any(int32(binary.LittleEndian.Uint32(b[i*4:]))).(T) }
	case uint32:
		for i := 0; i < n; i++ { out[i] = any(binary.LittleEndian.Uint32(b[i*4:])).(T) }
	case int64:
		for i := 0; i < n; i++ { out[i] = any(int64(binary.LittleEndian.Uint64(b[i*8:]))).(T) }
	case uint64:
		for i := 0; i < n; i++ { out[i] = any(binary.LittleEndian.Uint64(b[i*8:])).(T) }
	case uint:
		for i := 0; i < n; i++ { out[i] = any(uint(binary.LittleEndian.Uint64(b[i*8:]))).(T) }
	case int:
		for i := 0; i < n; i++ { out[i] = any(int(binary.LittleEndian.Uint64(b[i*8:]))).(T) }
	case float32:
		for i := 0; i < n; i++ { out[i] = any(math.Float32frombits(binary.LittleEndian.Uint32(b[i*4:]))).(T) }
	case float64:
		for i := 0; i < n; i++ { out[i] = any(math.Float64frombits(binary.LittleEndian.Uint64(b[i*8:]))).(T) }
	default:
		panic("rust2go-ipc: unsupported primitive list element")
	}
	return out
}

func newPrimList[T any](r ListRef) []T {
	n := int(r.len)
	if n <= 0 {
		return nil
	}
	// primList already decoded the little-endian wire bytes into a native,
	// aligned []T. Decode again would be an identity on little-endian hosts
	// but would byte-swap values on big-endian targets. Just copy the decoded
	// elements.
	src := unsafe.Slice((*T)(r.ptr), n)
	return append([]T(nil), src...)
}

func newStringList(r ListRef) []string {
	n := int(r.len)
	if n <= 0 {
		return nil
	}
	src := unsafe.Slice((*StringRef)(r.ptr), n)
	out := make([]string, n)
	for i := range out {
		out[i] = newString(src[i])
	}
	return out
}

type enc struct{ b []byte }

func (e *enc) put(v any) {
	switch x := v.(type) {
	case bool:
		if x { e.b = append(e.b, 1) } else { e.b = append(e.b, 0) }
	case int8:
		e.b = append(e.b, byte(x))
	case uint8:
		e.b = append(e.b, x)
	case int16:
		var b [2]byte
		binary.LittleEndian.PutUint16(b[:], uint16(x))
		e.b = append(e.b, b[:]...)
	case uint16:
		var b [2]byte
		binary.LittleEndian.PutUint16(b[:], x)
		e.b = append(e.b, b[:]...)
	case int32:
		var b [4]byte
		binary.LittleEndian.PutUint32(b[:], uint32(x))
		e.b = append(e.b, b[:]...)
	case uint32:
		var b [4]byte
		binary.LittleEndian.PutUint32(b[:], x)
		e.b = append(e.b, b[:]...)
	case int64:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], uint64(x))
		e.b = append(e.b, b[:]...)
	case uint64:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], x)
		e.b = append(e.b, b[:]...)
	case uint:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], uint64(x))
		e.b = append(e.b, b[:]...)
	case int:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], uint64(x))
		e.b = append(e.b, b[:]...)
	case float32:
		var b [4]byte
		binary.LittleEndian.PutUint32(b[:], math.Float32bits(x))
		e.b = append(e.b, b[:]...)
	case float64:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], math.Float64bits(x))
		e.b = append(e.b, b[:]...)
	case string:
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], uint64(len(x)))
		e.b = append(e.b, b[:]...)
		e.b = append(e.b, x...)
	default:
		panic("rust2go-ipc: unsupported wire type")
	}
}

type dec struct {
	b   []byte
	p   int
	err bool
}

func (d *dec) take(n int) []byte {
	if n < 0 || n > len(d.b)-d.p {
		d.err = true
		panic("rust2go-ipc: decode out of bounds")
	}
	v := d.b[d.p : d.p+n]
	d.p += n
	return v
}
func (d *dec) u64() uint64 { return binary.LittleEndian.Uint64(d.take(8)) }
func (d *dec) i64() int64  { return int64(d.u64()) }
func (d *dec) u32() uint32 { return binary.LittleEndian.Uint32(d.take(4)) }
func (d *dec) i32() int32  { return int32(d.u32()) }
func (d *dec) u16() uint16 { return binary.LittleEndian.Uint16(d.take(2)) }
func (d *dec) i16() int16  { return int16(d.u16()) }
func (d *dec) u8() uint8   { return d.take(1)[0] }
func (d *dec) i8() int8    { return int8(d.u8()) }
func (d *dec) boolean() bool {
	b := d.take(1)[0]
	if b > 1 {
		d.err = true
		panic("rust2go-ipc: invalid bool byte")
	}
	return b == 1
}
func (d *dec) f32() float32 { return math.Float32frombits(d.u32()) }
func (d *dec) f64() float64 { return math.Float64frombits(d.u64()) }
func (d *dec) str() StringRef {
	n := int(d.u64())
	if n < 0 || n > len(d.b)-d.p {
		d.err = true
		panic("rust2go-ipc: string len out of bounds")
	}
	if n == 0 {
		return StringRef{}
	}
	p := unsafe.Pointer(&d.b[d.p])
	d.p += n
	return StringRef{ptr: p, len: uint64(n)}
}
func primList[T any](d *dec) ListRef {
	n := int(d.u64())
	if n < 0 {
		d.err = true
		panic("rust2go-ipc: negative list len")
	}
	if n == 0 {
		return ListRef{}
	}
	var zero T
	sz := int(unsafe.Sizeof(zero))
	// Check the byte count against the remaining buffer before multiplying:
	// n * sz can overflow a signed int on a malformed frame. decodePrims also
	// guards this, but validating here keeps the error at the frame boundary.
	if n > (len(d.b)-d.p)/sz {
		d.err = true
		panic("rust2go-ipc: primitive list out of bounds")
	}
	b := d.take(n * sz)
	arr := decodePrims[T](b, n)
	return ListRef{ptr: unsafe.Pointer(&arr[0]), len: uint64(n)}
}
func stringList(d *dec) ListRef {
	n := int(d.u64())
	// A wire string is [u64 length][bytes]: minimum width is 8, not the
	// 16-byte in-memory size of StringRef. Using the latter rejects valid
	// lists of short strings.
	const minStringWire = 8
	if n < 0 || n > (len(d.b)-d.p)/minStringWire {
		d.err = true
		panic("rust2go-ipc: string list len out of bounds")
	}
	if n == 0 {
		return ListRef{}
	}
	arr := make([]StringRef, n)
	for i := range arr {
		arr[i] = d.str()
	}
	return ListRef{ptr: unsafe.Pointer(&arr[0]), len: uint64(n)}
}
"#);

    // Owned and Ref struct declarations.
    for st in &schema.structs {
        s.push_str(&format!("type {} struct {{\n", st.name));
        for f in &st.fields {
            s.push_str(&format!("\t{} {}\n", f.name, go_type(&f.ty, true)));
        }
        s.push_str("}\n\n");
        s.push_str(&format!("type {}Ref struct {{\n", st.name));
        for f in &st.fields {
            s.push_str(&format!("\t{} {}\n", f.name, go_ref_type(&f.ty)));
        }
        s.push_str("}\n\n");
    }

    // new* / *ListRef / flatten / restore per struct.
    for st in &schema.structs {
        s.push_str(&format!(
            "func new{}(r {}Ref) {} {{\n\tvar v {}\n",
            st.name, st.name, st.name, st.name
        ));
        for f in &st.fields {
            s.push_str(&format!(
                "\tv.{} = {}\n",
                f.name,
                go_owned_expr(&f.ty, &format!("r.{}", f.name))
            ));
        }
        s.push_str("\treturn v\n}\n\n");

        s.push_str(&format!(
            "func new{}List(r ListRef) []{} {{\n",
            st.name, st.name
        ));
        s.push_str("\tn := int(r.len)\n\tif n <= 0 { return nil }\n");
        s.push_str(&format!(
            "\tsrc := unsafe.Slice((*{}Ref)(r.ptr), n)\n\tout := make([]{}, n)\n\tfor i := range out {{ out[i] = new{}(src[i]) }}\n\treturn out\n}}\n\n",
            st.name, st.name, st.name
        ));

        s.push_str(&format!(
            "func flatten{}(v *{}, e *enc) {{\n",
            st.name, st.name
        ));
        for f in &st.fields {
            s.push_str(&format!(
                "\t{}\n",
                go_flatten_stmt(&f.ty, &format!("v.{}", f.name))
            ));
        }
        s.push_str("}\n\n");

        s.push_str(&format!(
            "func restore{}Ref(d *dec) {}Ref {{\n\tvar r {}Ref\n",
            st.name, st.name, st.name
        ));
        for f in &st.fields {
            s.push_str(&format!(
                "\tr.{} = {}\n",
                f.name,
                go_restore_expr(&f.ty, "d", &widths)
            ));
        }
        s.push_str("\tif d.err { panic(\"rust2go-ipc: invalid payload\") }\n\treturn r\n}\n\n");

        let size = widths.get(&st.name).copied().unwrap_or(1).to_string();
        s.push_str(&format!(
            "func {}ListRef(d *dec) ListRef {{\n\tcnt := int(d.u64())\n\tsize := {}\n\tif size == 0 {{ size = 1 }}\n\tif cnt < 0 || cnt > (len(d.b)-d.p)/size {{\n\t\td.err = true\n\t\tpanic(\"rust2go-ipc: struct list len out of bounds\")\n\t}}\n\tif cnt == 0 {{ return ListRef{{}} }}\n\tarr := make([]{}Ref, cnt)\n\tfor i := range arr {{ arr[i] = restore{}Ref(d) }}\n\treturn ListRef{{ptr: unsafe.Pointer(&arr[0]), len: uint64(cnt)}}\n}}\n\n",
            st.name, size, st.name, st.name
        ));
    }

    // Top-level helpers for non-struct method parameters / returns.
    let mut tops: Vec<(String, Type)> = Vec::new();
    for m in &schema.methods {
        for p in &m.params {
            if !matches!(classify(&p.ty), FieldKind::Custom(_)) {
                let n = top_name(&p.ty);
                if !tops.iter().any(|(existing, _)| existing == &n) {
                    tops.push((n, p.ty.clone()));
                }
            }
        }
        if let Some(ty) = &m.resp {
            if !matches!(classify(ty), FieldKind::Custom(_)) {
                let n = top_name(ty);
                if !tops.iter().any(|(existing, _)| existing == &n) {
                    tops.push((n, ty.clone()));
                }
            }
        }
    }
    for (n, ty) in &tops {
        let ref_ty = go_ref_type(ty);
        let owned_ty = go_type(ty, true);
        s.push_str(&format!(
            "func restoreTop{n}(d *dec) {ref_ty} {{ return {} }}\n",
            go_restore_expr(ty, "d", &widths)
        ));
        s.push_str(&format!(
            "func newTop{n}(r {ref_ty}) {owned_ty} {{ return {} }}\n\n",
            go_owned_expr(ty, "r")
        ));
    }

    // One Go interface per #[r2g] trait, mirroring upstream naming.
    for trait_name in &schema.traits {
        s.push_str(&format!("var {trait_name}Impl {trait_name}\n"));
        s.push_str(&format!("type {trait_name} interface {{\n"));
        for m in schema
            .methods
            .iter()
            .filter(|m| &m.trait_name == trait_name)
        {
            let params: Vec<String> = m
                .params
                .iter()
                .map(|p| {
                    let star = if m.go_ptr { "*" } else { "" };
                    format!("{} {}{}", p.name, star, go_type(&p.ty, true))
                })
                .collect();
            let ret = m
                .resp
                .as_ref()
                .map(|t| format!(" {}", go_type(t, true)))
                .unwrap_or_default();
            s.push_str(&format!("\t{}({}){}\n", m.name, params.join(", "), ret));
        }
        s.push_str("}\n\n");
    }

    // dispatch with panic containment.
    s.push_str("func dispatch(methodID uint32, payload []byte) (out []byte, ok bool) {\n");
    s.push_str("\tdefer func() {\n\t\tif r := recover(); r != nil {\n");
    s.push_str("\t\t\tfmt.Fprintf(os.Stderr, \"rust2go-ipc: Go handler panic: %v\\n\", r)\n");
    s.push_str("\t\t\tout, ok = nil, false\n\t\t}\n\t}()\n");
    s.push_str("\td := &dec{b: payload}\n\tswitch methodID {\n");
    for m in &schema.methods {
        let mut decls = String::new();
        let mut args = String::new();
        for (i, p) in m.params.iter().enumerate() {
            let var = format!("_a{i}");
            let ref_var = format!("_r{i}");
            let kind = classify(&p.ty);
            let restore = if let FieldKind::Custom(n) = &kind {
                format!("restore{n}Ref(d)")
            } else {
                format!("restoreTop{}(d)", top_name(&p.ty))
            };
            let owned = if let FieldKind::Custom(n) = &kind {
                format!("new{n}({ref_var})")
            } else {
                format!("newTop{}({ref_var})", top_name(&p.ty))
            };
            decls.push_str(&format!(
                "\t\t{ref_var} := {restore}\n\t\t{var} := {owned}\n"
            ));
            let star = if m.go_ptr { "&" } else { "" };
            if i > 0 {
                args.push_str(", ");
            }
            args.push_str(&format!("{star}{var}"));
        }
        let receiver = format!("{}Impl.{}", m.trait_name, m.name);
        s.push_str(&format!("\tcase {}:\n", m.index));
        s.push_str(&decls);
        match &m.resp {
            None => {
                s.push_str(&format!(
                    "\t\t{}({})\n\t\treturn nil, true\n",
                    receiver, args
                ));
            }
            Some(resp) => {
                s.push_str(&format!("\t\t_resp := {}({})\n", receiver, args));
                if matches!(classify(resp), FieldKind::Custom(_)) {
                    let rn = type_ident(resp);
                    s.push_str(&format!(
                        "\t\te := &enc{{}}\n\t\tflatten{}(&_resp, e)\n\t\treturn e.b, true\n",
                        rn
                    ));
                } else {
                    let flatten = go_flatten_stmt(resp, "_resp");
                    s.push_str(&format!(
                        "\t\te := &enc{{}}\n\t\t{flatten}\n\t\treturn e.b, true\n"
                    ));
                }
            }
        }
    }
    s.push_str("\t}\n\treturn nil, false\n}\n");
    s
}

fn go_flatten_stmt(ty: &Type, value: &str) -> String {
    match classify(ty) {
        FieldKind::String => format!("e.put({value})"),
        FieldKind::Primitive(_) => format!("e.put({value})"),
        FieldKind::Custom(_) => format!("flatten{}(&{value}, e)", type_ident(ty)),
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(_) | FieldKind::String | FieldKind::Custom(_) => format!(
                "e.put(uint64(len({value})))\n\tfor _, _item := range {value} {{\n\t\t{}\n\t}}",
                go_put_item(&elem, "_item")
            ),
            FieldKind::List(_) => unreachable!(),
        },
    }
}

fn go_put_item(kind: &FieldKind, item: &str) -> String {
    match kind {
        FieldKind::Primitive(_) | FieldKind::String => format!("e.put({item})"),
        FieldKind::Custom(n) => format!("flatten{n}(&{item}, e)"),
        FieldKind::List(_) => unreachable!(),
    }
}

fn generate_go_main() -> String {
    r#"// Code generated by rust2go-ipc. DO NOT EDIT.
package main

import (
	"encoding/binary"
	"errors"
	"io"
	"net"
	"os"
	"runtime"
	"strconv"
	"sync"
	"time"
)

const magic = "R2GI"
const version = uint16(1)

const maxAcceptErrs = 10

// A single long-lived connection can carry many in-flight async requests.
// Dispatching each in its own goroutine prevents a slow Go handler from
// serialising every later request on that connection. The Rust reactor matches
// responses by req_id, so responses may be written back out of order. sem
// bounds concurrency so a burst cannot spawn an unbounded number of goroutines
// or overwhelm downstream services. Set RUST2GO_IPC_MAX_CONCURRENCY to tune.
var sem chan struct{}

func init() {
	n := runtime.NumCPU() * 4
	if n < 16 {
		n = 16
	}
	if v := os.Getenv("RUST2GO_IPC_MAX_CONCURRENCY"); v != "" {
		if parsed, err := strconv.Atoi(v); err == nil && parsed > 0 {
			n = parsed
		}
	}
	sem = make(chan struct{}, n)
}

func acceptBackoff(attempt int) time.Duration {
	d := 5 * time.Millisecond
	for i := 1; i < attempt && d < time.Second; i++ {
		d *= 2
	}
	if d > time.Second {
		d = time.Second
	}
	return d
}

func main() {
	transport := os.Getenv("RUST2GO_IPC_TRANSPORT")
	endpoint := os.Getenv("RUST2GO_IPC_ENDPOINT")
	if endpoint == "" {
		port := os.Getenv("RUST2GO_IPC_PORT")
		if port == "" {
			panic("RUST2GO_IPC_ENDPOINT / RUST2GO_IPC_PORT not set")
		}
		if transport == "" {
			transport = "tcp"
		}
		endpoint = "127.0.0.1:" + port
	}
	if transport == "" {
		transport = "tcp"
	}

	var ln net.Listener
	var err error
	switch transport {
	case "unix":
		if len(endpoint) >= 100 {
			panic("rust2go-ipc: unix endpoint path too long")
		}
		_ = os.Remove(endpoint)
		ln, err = net.Listen("unix", endpoint)
	case "tcp":
		ln, err = net.Listen("tcp", endpoint)
	default:
		panic("rust2go-ipc: unknown transport " + transport)
	}
	if err != nil {
		panic(err)
	}
	acceptErrs := 0
	for {
		conn, err := ln.Accept()
		if err != nil {
			if errors.Is(err, net.ErrClosed) {
				panic("rust2go-ipc: listener closed: " + err.Error())
			}
			acceptErrs++
			if acceptErrs >= maxAcceptErrs {
				panic("rust2go-ipc: accept failed repeatedly: " + err.Error())
			}
			time.Sleep(acceptBackoff(acceptErrs))
			continue
		}
		acceptErrs = 0
		go serve(conn)
	}
}

func serve(conn net.Conn) {
	var writeMu sync.Mutex
	var wg sync.WaitGroup
	done := make(chan struct{})
	var once sync.Once
	stop := func() {
		once.Do(func() {
			close(done)
			_ = conn.Close()
		})
	}
	defer func() {
		stop()
		// Give in-flight handlers a bounded chance to finish and release their
		// semaphore slot. A hung business handler must not block this exit:
		// the Rust side has its own pending TTL and caller timeout.
		drained := make(chan struct{})
		go func() { wg.Wait(); close(drained) }()
		select {
		case <-drained:
		case <-time.After(2 * time.Second):
		}
	}()

	hdr := make([]byte, 22)
	for {
		// Idle handling: an idle read deadline is a keep-alive tick, not a
		// reason to close. Closing an idle connection makes the Rust reactor
		// see EOF and (before the liveness probe) kill + respawn this still
		// healthy child, discarding in-process state such as a one-way Init.
		// n == 0 guards against a partial header (a desynced stream), which
		// must always be closed.
		_ = conn.SetReadDeadline(time.Now().Add(5 * time.Minute))
		n, err := io.ReadFull(conn, hdr)
		if err != nil {
			if n == 0 && errors.Is(err, os.ErrDeadlineExceeded) {
				continue
			}
			return
		}
		if string(hdr[0:4]) != magic {
			return
		}
		if binary.LittleEndian.Uint16(hdr[4:6]) != version {
			return
		}
		reqID := binary.LittleEndian.Uint64(hdr[6:14])
		methodID := binary.LittleEndian.Uint32(hdr[14:18])
		length32 := binary.LittleEndian.Uint32(hdr[18:22])
		if length32 > 64*1024*1024 {
			return
		}
		// Own the payload before handing it to a goroutine: the read buffer is
		// reused on the next loop iteration.
		payload := make([]byte, int(length32))
		if _, err := io.ReadFull(conn, payload); err != nil {
			return
		}

		// Back-pressure: block the read loop once the concurrency cap is
		// reached, so a burst cannot spawn unbounded goroutines. Acquiring the
		// slot is cancellable two ways: if the connection is torn down (e.g. a
		// handler hit a write error) `done` fires; if every slot is wedged by a
		// hung handler the timeout fires. Without the timeout the Rust caller
		// times out and closes its side, but this serve goroutine would block
		// forever on the channel (a read deadline does not cover channel ops),
		// leaking a goroutine. Both paths are bounded. The wait exceeds the
		// Rust sync timeout (30 s) so normal queuing is not cut short.
		select {
		case sem <- struct{}{}:
		case <-done:
			return
		case <-time.After(60 * time.Second):
			return
		}
		wg.Add(1)
		go func(id uint64, method uint32, body []byte) {
			defer func() { wg.Done(); <-sem }()
			resp, ok := dispatch(method, body)
			if id == 0 {
				return
			}
			status := byte(0)
			if !ok || len(resp) > 64*1024*1024 {
				status = 1
				resp = nil
			}
			out := make([]byte, 19+len(resp))
			copy(out[0:4], magic)
			binary.LittleEndian.PutUint16(out[4:6], version)
			binary.LittleEndian.PutUint64(out[6:14], id)
			out[14] = status
			binary.LittleEndian.PutUint32(out[15:19], uint32(len(resp)))
			copy(out[19:], resp)
			func() {
				writeMu.Lock()
				defer writeMu.Unlock()
				if _, err := conn.Write(out); err != nil {
					// The peer is gone; tear down the read loop so the Rust
					// side reconnects on EOF instead of waiting for idle time.
					stop()
				}
			}()
		}(reqID, methodID, payload)
	}
}
"#
    .to_string()
}

#[cfg(test)]
#[path = "builder_tests.rs"]
mod tests;
