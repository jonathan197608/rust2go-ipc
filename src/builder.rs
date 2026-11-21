//! Build-time API mirroring `rust2go::Builder`. Instead of cgo + bindgen, this
//! parses the Rust schema and emits pure-Go sources plus Rust binding mirrors.

use std::{env, ffi::OsString, fs, path::PathBuf, process::Command};

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Fields, Item, ItemStruct, Type};

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
    /// Go IPC server with CGO disabled.
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

        println!("cargo:rerun-if-changed={}", src_rs.display());
        println!("cargo:rerun-if-changed={}", go_dir.display());

        let schema = parse_schema(&fs::read_to_string(&src_rs).expect("cannot read schema source"));
        let binding_name = self.binding_name.as_deref().unwrap_or("_go_bindings.rs");
        let port = crate::ipc::free_port();

        let rust = generate_rust_bindings(&schema, port);
        fs::write(out_dir.join(binding_name), rust.to_string()).expect("cannot write bindings");

        let go_gen = generate_go_sources(&schema);
        fs::write(&regen_dst, &go_gen).expect("cannot write Go sources");

        let ipc_main = go_dir.join("ipc_main.go");
        fs::write(&ipc_main, generate_go_main()).expect("cannot write Go main");

        // Keep generated Go sources gofmt-clean when the tool is available.
        // Some CI images put `go` on PATH but not `gofmt`; fall back to GOROOT/bin.
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
        // Do not force CGO_ENABLED=0: real projects may depend on cgo-only
        // packages (webp, etc.). The Go server is a separate process, so cgo
        // there does not put the Go runtime into the JVM process. Callers can
        // still force it off via CustomArgGoCompiler::env.
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

        // Also drop a copy next to the crate artifacts (target/<triple>/<profile>)
        // so container builds can COPY a stable path instead of the hashed
        // OUT_DIR used at compile time.
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
    // 1) Plain `gofmt` on PATH.
    if Command::new("gofmt")
        .arg("--help")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Some(PathBuf::from("gofmt"));
    }
    // 2) GOROOT env var.
    if let Ok(root) = env::var("GOROOT") {
        let p = PathBuf::from(root).join("bin").join("gofmt");
        if p.exists() {
            return Some(p);
        }
    }
    // 3) Ask `go env GOROOT`.
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

#[derive(Debug, Clone)]
struct MethodDef {
    name: String,
    req: String,
    resp: Option<String>,
    is_async: bool,
    index: u32,
}

#[derive(Default)]
struct Schema {
    structs: Vec<StructDef>,
    methods: Vec<MethodDef>,
}

fn parse_schema(src: &str) -> Schema {
    let file: syn::File = syn::parse_str(src).expect("cannot parse schema source");
    let mut schema = Schema::default();
    for item in &file.items {
        if let Item::Struct(s) = item {
            if has_derive(s, "R2G") {
                schema.structs.push(parse_struct(s));
            }
        }
    }
    for item in &file.items {
        if let Item::Trait(t) = item {
            if t.attrs.iter().any(|a| a.path().is_ident("r2g")) {
                let mut idx = 0u32;
                for m in &t.items {
                    if let syn::TraitItem::Fn(f) = m {
                        let name = f.sig.ident.to_string();
                        let is_async = f.sig.asyncness.is_some();
                        let req = match f.sig.inputs.first() {
                            Some(syn::FnArg::Typed(p)) => type_ident(&p.ty),
                            _ => "EmptyRequest".to_string(),
                        };
                        let resp = match &f.sig.output {
                            syn::ReturnType::Type(_, ty) => Some(type_ident(ty)),
                            _ => None,
                        };
                        schema.methods.push(MethodDef {
                            name,
                            req,
                            resp,
                            is_async,
                            index: idx,
                        });
                        idx += 1;
                    }
                }
            }
        }
    }
    schema
}

fn has_derive(s: &ItemStruct, name: &str) -> bool {
    s.attrs.iter().any(|a| {
        a.path().is_ident("derive")
            && a.meta
                .require_list()
                .map(|l| l.tokens.to_string().split(',').any(|p| p.trim() == name))
                .unwrap_or(false)
    })
}

fn parse_struct(s: &ItemStruct) -> StructDef {
    let fields = match &s.fields {
        Fields::Named(n) => n
            .named
            .iter()
            .map(|f| FieldDef {
                name: f.ident.as_ref().expect("named field").to_string(),
                ty: f.ty.clone(),
            })
            .collect(),
        _ => Vec::new(),
    };
    StructDef {
        name: s.ident.to_string(),
        fields,
    }
}

fn type_ident(ty: &Type) -> String {
    match ty {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum FieldKind {
    Primitive(String),
    String,
    List(Box<FieldKind>),
    Custom(String),
}

/// Extract the element type from `Vec<T>` / `Option<T>`.
fn generic_arg(ty: &Type) -> Option<&Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t),
        _ => None,
    })
}

fn classify(ty: &Type) -> FieldKind {
    let name = type_ident(ty);
    match name.as_str() {
        "String" => FieldKind::String,
        "Vec" | "Option" => {
            let elem = generic_arg(ty)
                .map(classify)
                .unwrap_or(FieldKind::Primitive("u8".to_string()));
            FieldKind::List(Box::new(elem))
        }
        "bool" | "i8" | "i16" | "i32" | "i64" | "i128" | "u8" | "u16" | "u32" | "u64" | "u128"
        | "f32" | "f64" | "usize" | "isize" => FieldKind::Primitive(name),
        _ => FieldKind::Custom(name),
    }
}

fn ref_ty(ty: &Type) -> TokenStream {
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

fn generate_rust_bindings(schema: &Schema, port: u16) -> TokenStream {
    fn check_supported(kind: &FieldKind) {
        match kind {
            FieldKind::Primitive(p) if p == "i128" || p == "u128" => {
                panic!("rust2go-ipc: i128/u128 fields are not wire-compatible")
            }
            FieldKind::List(elem) => check_supported(elem),
            _ => {}
        }
    }
    for st in &schema.structs {
        for f in &st.fields {
            check_supported(&classify(&f.ty));
        }
    }

    let mut out = TokenStream::new();

    let structs: Vec<TokenStream> = schema
        .structs
        .iter()
        .map(|s| {
            let name = format_ident!("{}Ref", s.name);
            let fields = s.fields.iter().map(|f| {
                let id = format_ident!("{}", f.name);
                let ty = ref_ty(&f.ty);
                quote! { pub #id: #ty }
            });
            quote! {
                #[repr(C)]
                #[derive(Copy, Clone)]
                #[allow(non_snake_case)]
                pub struct #name { #(#fields),* }
            }
        })
        .collect();

    let flatten_fns: Vec<TokenStream> = schema.structs.iter().map(gen_rust_flatten).collect();
    let restore_fns: Vec<TokenStream> = schema.structs.iter().map(gen_rust_restore).collect();

    let mut stubs = Vec::new();
    let mut trampolines = Vec::new();
    for m in &schema.methods {
        let stub_name = format_ident!("CGolangCall_{}", m.name);
        let req_ref = format_ident!("{}Ref", m.req);
        let method_id = m.index;
        let tramp_name = format_ident!("__r2g_trampoline_{}", m.name);

        let stub = match (&m.resp, m.is_async) {
            (Some(_resp), false) => {
                let flatten = format_ident!("__r2g_flatten_{}", m.req);
                quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    pub unsafe fn #stub_name(
                        req: #req_ref,
                        slot: *mut ::std::ffi::c_void,
                        cb: *mut ::std::ffi::c_void,
                    ) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        #flatten(&mut enc, &req);
                        ::rust2go::ipc::dispatch_sync(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                            slot as *const (),
                            cb as *const (),
                            #tramp_name,
                        );
                    }
                }
            }
            (Some(_resp), true) => {
                let flatten = format_ident!("__r2g_flatten_{}", m.req);
                quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    pub unsafe fn #stub_name(
                        req: #req_ref,
                        slot: *mut ::std::ffi::c_void,
                        cb: *mut ::std::ffi::c_void,
                    ) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        #flatten(&mut enc, &req);
                        ::rust2go::ipc::dispatch(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                            slot as *const (),
                            cb as *const (),
                            #tramp_name,
                        );
                    }
                }
            }
            (None, _) => {
                let flatten = format_ident!("__r2g_flatten_{}", m.req);
                quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc)]
                    pub unsafe fn #stub_name(
                        req: #req_ref,
                    ) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        #flatten(&mut enc, &req);
                        ::rust2go::ipc::dispatch_oneway(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                        );
                    }
                }
            }
        };
        stubs.push(stub);

        if let Some(resp) = &m.resp {
            let resp_ref = format_ident!("{}Ref", resp);
            let restore = format_ident!("__r2g_restore_{}", resp);
            let tramp = quote! {
                #[allow(non_snake_case, clippy::missing_safety_doc)]
                unsafe extern "C" fn #tramp_name(
                    payload: *const u8,
                    len: usize,
                    slot: *const (),
                    cb: *const (),
                ) {
                    let mut dec = ::rust2go::ipc::Decoder::new(
                        ::std::slice::from_raw_parts(payload, len)
                    );
                    let resp: #resp_ref = #restore(&mut dec);
                    type Cb = unsafe extern "C" fn(*const #resp_ref, *const ());
                    let f: Cb = ::std::mem::transmute(cb);
                    f(&resp, slot);
                }
            };
            trampolines.push(tramp);
        }
    }

    let header = quote! {
        // Generated by rust2go-ipc. Do not edit.
    };

    out.extend(header);
    out.extend(quote! { #(#structs)* });
    out.extend(quote! { #(#flatten_fns)* });
    out.extend(quote! { #(#restore_fns)* });
    out.extend(quote! { #(#stubs)* });
    out.extend(quote! { #(#trampolines)* });
    out
}

fn gen_rust_flatten(s: &StructDef) -> TokenStream {
    let name = format_ident!("__r2g_flatten_{}", s.name);
    let ref_name = format_ident!("{}Ref", s.name);
    let body: Vec<TokenStream> = s
        .fields
        .iter()
        .map(|f| {
            let id = format_ident!("{}", f.name);
            match classify(&f.ty) {
                FieldKind::String => quote! { enc.string(v.#id); },
                FieldKind::List(elem) => match *elem {
                    FieldKind::Primitive(p) => {
                        let ty = format_ident!("{}", p);
                        quote! { enc.prim_list::<#ty>(v.#id.ptr, v.#id.len); }
                    }
                    FieldKind::String => quote! {
                        enc.list_header(v.#id.len);
                        if v.#id.len > 0 {
                            let items = ::std::slice::from_raw_parts(
                                v.#id.ptr as *const ::rust2go::ipc::RawStringRef,
                                v.#id.len,
                            );
                            for item in items { enc.string(*item); }
                        }
                    },
                    FieldKind::Custom(n) => {
                        let flatten = format_ident!("__r2g_flatten_{}", n);
                        let rt = format_ident!("{}Ref", n);
                        quote! {
                            enc.list_header(v.#id.len);
                            if v.#id.len > 0 {
                                let items = ::std::slice::from_raw_parts(
                                    v.#id.ptr as *const #rt,
                                    v.#id.len,
                                );
                                for item in items { #flatten(enc, item); }
                            }
                        }
                    }
                    FieldKind::List(_) => quote! {},
                },
                FieldKind::Primitive(_) => quote! { enc.prim(v.#id); },
                FieldKind::Custom(n) => {
                    let flatten = format_ident!("__r2g_flatten_{}", n);
                    quote! { #flatten(enc, &v.#id); }
                }
            }
        })
        .collect();
    quote! {
        #[allow(non_snake_case, clippy::all, unused_variables)]
        unsafe fn #name(enc: &mut ::rust2go::ipc::Encoder, v: &#ref_name) {
            #(#body)*
        }
    }
}

fn gen_rust_restore(s: &StructDef) -> TokenStream {
    let name = format_ident!("__r2g_restore_{}", s.name);
    let ref_name = format_ident!("{}Ref", s.name);
    let body: Vec<TokenStream> = s
        .fields
        .iter()
        .map(|f| {
            let id = format_ident!("{}", f.name);
            match classify(&f.ty) {
                FieldKind::String => quote! { #id: dec.string(), },
                FieldKind::List(elem) => match *elem {
                    FieldKind::Primitive(p) => {
                        let ty = format_ident!("{}", p);
                        quote! { #id: dec.prim_list::<#ty>(), }
                    }
                    FieldKind::String => quote! {
                        #id: {
                            let n = dec.list_len();
                            let mut items = Vec::with_capacity(n);
                            for _ in 0..n { items.push(dec.string()); }
                            dec.ref_list(items)
                        },
                    },
                    FieldKind::Custom(n) => {
                        let restore = format_ident!("__r2g_restore_{}", n);
                        quote! {
                            #id: {
                                let n = dec.list_len();
                                let mut items = Vec::with_capacity(n);
                                for _ in 0..n { items.push(#restore(dec)); }
                                dec.ref_list(items)
                            },
                        }
                    }
                    FieldKind::List(_) => quote! { #id: ::rust2go::ipc::RawListRef { ptr: ::std::ptr::null(), len: 0 }, },
                },
                FieldKind::Primitive(_) => quote! { #id: dec.prim(), },
                FieldKind::Custom(n) => {
                    let restore = format_ident!("__r2g_restore_{}", n);
                    quote! { #id: #restore(dec), }
                }
            }
        })
        .collect();
    quote! {
        #[allow(non_snake_case, clippy::all, unused_variables)]
        unsafe fn #name(dec: &mut ::rust2go::ipc::Decoder<'_>) -> #ref_name {
            #ref_name { #(#body)* }
        }
    }
}

fn go_prim(p: &str) -> String {
    let ty = match p {
        "bool" => "bool",
        "i8" => "int8",
        "i16" => "int16",
        "i32" => "int32",
        "i64" => "int64",
        "i128" => "int64",
        "u8" => "uint8",
        "u16" => "uint16",
        "u32" => "uint32",
        "u64" => "uint64",
        "u128" => "uint64",
        "f32" => "float32",
        "f64" => "float64",
        "usize" | "isize" => "uint",
        _ => "uint64",
    };
    ty.to_string()
}

fn go_type(ty: &Type) -> String {
    match classify(ty) {
        FieldKind::String => "string".to_string(),
        FieldKind::List(elem) => match *elem {
            FieldKind::Primitive(p) => format!("[]{}", go_prim(&p)),
            FieldKind::String => "[]string".to_string(),
            FieldKind::Custom(n) => format!("[]{}", n),
            FieldKind::List(_) => "[]byte".to_string(),
        },
        FieldKind::Primitive(p) => go_prim(&p),
        FieldKind::Custom(n) => n,
    }
}

fn generate_go_sources(schema: &Schema) -> String {
    let mut s = String::new();
    s.push_str("// Code generated by rust2go-ipc. DO NOT EDIT.\npackage main\n\n");
    s.push_str(
        "import (\n\t\"encoding/binary\"\n\t\"fmt\"\n\t\"math\"\n\t\"os\"\n\t\"unsafe\"\n)\n\n",
    );
    s.push_str("type StringRef struct {\n\tptr unsafe.Pointer\n\tlen uint64\n}\n\n");
    s.push_str("type ListRef struct {\n\tptr unsafe.Pointer\n\tlen uint64\n}\n\n");

    // Owned business structs (same shape as rust2go-cli output).
    for st in &schema.structs {
        s.push_str(&format!("type {} struct {{\n", st.name));
        for f in &st.fields {
            s.push_str(&format!("\t{} {}\n", f.name, go_type(&f.ty)));
        }
        s.push_str("}\n\n");
    }

    // Ref structs mirroring Rust #[repr(C)] layout.
    for st in &schema.structs {
        s.push_str(&format!("type {}Ref struct {{\n", st.name));
        for f in &st.fields {
            let ty = match classify(&f.ty) {
                FieldKind::String => "StringRef".to_string(),
                FieldKind::List(_) => "ListRef".to_string(),
                FieldKind::Primitive(ref p) => go_prim(p),
                FieldKind::Custom(ref n) => format!("{}Ref", n),
            };
            s.push_str(&format!("\t{} {}\n", f.name, ty));
        }
        s.push_str("}\n\n");
    }

    // Generic Ref -> owned helpers.
    s.push_str("func newString(r StringRef) string {\n\tif r.len == 0 { return \"\" }\n\treturn unsafe.String((*byte)(r.ptr), int(r.len))\n}\n\n");
    s.push_str("func newPrimList[T any](r ListRef) []T {\n\tif r.len == 0 { return nil }\n\treturn append([]T(nil), unsafe.Slice((*T)(r.ptr), int(r.len))...)\n}\n\n");
    s.push_str("func newStringList(r ListRef) []string {\n\tif r.len == 0 { return nil }\n\tsrc := unsafe.Slice((*StringRef)(r.ptr), int(r.len))\n\tout := make([]string, int(r.len))\n\tfor i := range out { out[i] = newString(src[i]) }\n\treturn out\n}\n\n");

    // new<Type>: Ref -> owned, plus new<Type>List for complex elements.
    for st in &schema.structs {
        s.push_str(&format!(
            "func new{}(r {}Ref) {} {{\n\treturn {}{{\n",
            st.name, st.name, st.name, st.name
        ));
        for f in &st.fields {
            let expr = match classify(&f.ty) {
                FieldKind::String => format!("newString(r.{})", f.name),
                FieldKind::List(ref elem) => match **elem {
                    FieldKind::Primitive(ref p) => {
                        format!("newPrimList[{}](r.{})", go_prim(p), f.name)
                    }
                    FieldKind::String => format!("newStringList(r.{})", f.name),
                    FieldKind::Custom(ref n) => format!("new{}List(r.{})", n, f.name),
                    FieldKind::List(_) => "nil".to_string(),
                },
                FieldKind::Primitive(_) => format!("r.{}", f.name),
                FieldKind::Custom(ref n) => format!("new{}(r.{})", n, f.name),
            };
            s.push_str(&format!("\t\t{}: {},\n", f.name, expr));
        }
        s.push_str("\t}\n}\n\n");

        s.push_str(&format!(
            "func new{}List(r ListRef) []{} {{\n",
            st.name, st.name
        ));
        s.push_str("\tif r.len == 0 { return nil }\n");
        s.push_str(&format!(
            "\tsrc := unsafe.Slice((*{}Ref)(r.ptr), int(r.len))\n",
            st.name
        ));
        s.push_str(&format!("\tout := make([]{}, int(r.len))\n", st.name));
        s.push_str(&format!(
            "\tfor i := range out {{ out[i] = new{}(src[i]) }}\n",
            st.name
        ));
        s.push_str("\treturn out\n}\n\n");
    }

    // Flat codec on Go side. Primitive widths must match Rust memory layout.
    s.push_str("type enc struct{ b []byte }\n");
    s.push_str("func (e *enc) put(v any) {\n\tswitch x := v.(type) {\n");
    s.push_str("\tcase bool:\n\t\tif x { e.b = append(e.b, 1) } else { e.b = append(e.b, 0) }\n");
    s.push_str("\tcase int8:\n\t\te.b = append(e.b, byte(x))\n");
    s.push_str("\tcase uint8:\n\t\te.b = append(e.b, x)\n");
    s.push_str("\tcase int16:\n\t\tvar t [2]byte; binary.LittleEndian.PutUint16(t[:], uint16(x)); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase uint16:\n\t\tvar t [2]byte; binary.LittleEndian.PutUint16(t[:], x); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase int32:\n\t\tvar t [4]byte; binary.LittleEndian.PutUint32(t[:], uint32(x)); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase uint32:\n\t\tvar t [4]byte; binary.LittleEndian.PutUint32(t[:], x); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase int64:\n\t\tvar t [8]byte; binary.LittleEndian.PutUint64(t[:], uint64(x)); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase uint64:\n\t\tvar t [8]byte; binary.LittleEndian.PutUint64(t[:], x); e.b = append(e.b, t[:]...)\n");
    s.push_str("\tcase int:\n\t\te.put(int64(x))\n");
    s.push_str("\tcase uint:\n\t\te.put(uint64(x))\n");
    s.push_str("\tcase float32:\n\t\te.put(math.Float32bits(x))\n");
    s.push_str("\tcase float64:\n\t\te.put(math.Float64bits(x))\n");
    s.push_str("\tcase string:\n\t\te.put(uint64(len(x))); e.b = append(e.b, x...)\n");
    s.push_str("\t}\n}\n\n");

    s.push_str("type dec struct{ b []byte; p int }\n");
    s.push_str("func (d *dec) take(n int) []byte { v := d.b[d.p:d.p+n]; d.p += n; return v }\n");
    s.push_str("func (d *dec) u64() uint64 { return binary.LittleEndian.Uint64(d.take(8)) }\n");
    s.push_str("func (d *dec) i64() int64 { return int64(d.u64()) }\n");
    s.push_str("func (d *dec) u32() uint32 { return binary.LittleEndian.Uint32(d.take(4)) }\n");
    s.push_str("func (d *dec) i32() int32 { return int32(d.u32()) }\n");
    s.push_str("func (d *dec) u16() uint16 { return binary.LittleEndian.Uint16(d.take(2)) }\n");
    s.push_str("func (d *dec) i16() int16 { return int16(d.u16()) }\n");
    s.push_str("func (d *dec) u8() uint8 { return d.take(1)[0] }\n");
    s.push_str("func (d *dec) i8() int8 { return int8(d.u8()) }\n");
    s.push_str("func (d *dec) boolean() bool { return d.take(1)[0] != 0 }\n");
    s.push_str("func (d *dec) f32() float32 { return math.Float32frombits(d.u32()) }\n");
    s.push_str("func (d *dec) f64() float64 { return math.Float64frombits(d.u64()) }\n");
    s.push_str("func (d *dec) str() StringRef {\n\tn := int(d.u64()); if n == 0 { return StringRef{} }\n\tp := unsafe.Pointer(&d.b[d.p]); d.p += n\n\treturn StringRef{ptr: p, len: uint64(n)}\n}\n");
    s.push_str("func primList[T any](d *dec) ListRef {\n\tn := int(d.u64()); if n == 0 { return ListRef{} }\n\tvar zero T\n\tp := unsafe.Pointer(&d.b[d.p]); d.p += n * int(unsafe.Sizeof(zero))\n\treturn ListRef{ptr: p, len: uint64(n)}\n}\n");
    s.push_str("func stringList(d *dec) ListRef {\n\tn := int(d.u64()); if n == 0 { return ListRef{} }\n\tarr := make([]StringRef, n)\n\tfor i := range arr { arr[i] = d.str() }\n\treturn ListRef{ptr: unsafe.Pointer(&arr[0]), len: uint64(n)}\n}\n\n");

    // flatten_<T> (owned -> bytes) and restore_<T>Ref (bytes -> Ref).
    for st in &schema.structs {
        s.push_str(&format!(
            "func flatten{}(v *{}, e *enc) {{\n",
            st.name, st.name
        ));
        for f in &st.fields {
            match classify(&f.ty) {
                FieldKind::String => s.push_str(&format!("\te.put(v.{})\n", f.name)),
                FieldKind::List(ref elem) => match **elem {
                    FieldKind::Primitive(_) => {
                        s.push_str(&format!("\te.put(uint64(len(v.{})))\n", f.name));
                        s.push_str(&format!(
                            "\tfor _, item := range v.{} {{ e.put(item) }}\n",
                            f.name
                        ));
                    }
                    FieldKind::String => {
                        s.push_str(&format!("\te.put(uint64(len(v.{})))\n", f.name));
                        s.push_str(&format!(
                            "\tfor _, item := range v.{} {{ e.put(item) }}\n",
                            f.name
                        ));
                    }
                    FieldKind::Custom(ref n) => {
                        s.push_str(&format!("\te.put(uint64(len(v.{})))\n", f.name));
                        s.push_str(&format!(
                            "\tfor i := range v.{} {{ flatten{}(&v.{}[i], e) }}\n",
                            f.name, n, f.name
                        ));
                    }
                    FieldKind::List(_) => s.push_str("\te.put(uint64(0))\n"),
                },
                FieldKind::Primitive(_) => s.push_str(&format!("\te.put(v.{})\n", f.name)),
                FieldKind::Custom(ref n) => {
                    s.push_str(&format!("\tflatten{}(&v.{}, e)\n", n, f.name));
                }
            }
        }
        s.push_str("}\n\n");

        s.push_str(&format!(
            "func restore{}Ref(d *dec) {}Ref {{\n\treturn {}Ref{{\n",
            st.name, st.name, st.name
        ));
        for f in &st.fields {
            match classify(&f.ty) {
                FieldKind::String => {
                    s.push_str(&format!("\t\t{}: d.str(),\n", f.name));
                }
                FieldKind::List(ref elem) => match **elem {
                    FieldKind::Primitive(ref p) => {
                        s.push_str(&format!("\t\t{}: primList[{}](d),\n", f.name, go_prim(p)));
                    }
                    FieldKind::String => {
                        s.push_str(&format!("\t\t{}: stringList(d),\n", f.name));
                    }
                    FieldKind::Custom(ref n) => {
                        s.push_str(&format!("\t\t{}: func() ListRef {{\n", f.name));
                        s.push_str("\t\t\tcnt := int(d.u64())\n");
                        s.push_str("\t\t\tif cnt == 0 { return ListRef{} }\n");
                        s.push_str(&format!("\t\t\tarr := make([]{}Ref, cnt)\n", n));
                        s.push_str(&format!(
                            "\t\t\tfor i := range arr {{ arr[i] = restore{}Ref(d) }}\n",
                            n
                        ));
                        s.push_str("\t\t\treturn ListRef{ptr: unsafe.Pointer(&arr[0]), len: uint64(cnt)}\n");
                        s.push_str("\t\t}(),\n");
                    }
                    FieldKind::List(_) => {
                        s.push_str(&format!("\t\t{}: ListRef{{}},\n", f.name));
                    }
                },
                FieldKind::Primitive(ref p) => {
                    let rd = match p.as_str() {
                        "bool" => "d.boolean()",
                        "i8" => "d.i8()",
                        "u8" => "d.u8()",
                        "i16" => "d.i16()",
                        "u16" => "d.u16()",
                        "i32" | "i128" | "isize" => "d.i32()",
                        "u32" | "u128" | "usize" => "d.u32()",
                        "i64" => "d.i64()",
                        "f32" => "d.f32()",
                        "f64" => "d.f64()",
                        _ => "d.u64()",
                    };
                    s.push_str(&format!("\t\t{}: {},\n", f.name, rd));
                }
                FieldKind::Custom(ref n) => {
                    s.push_str(&format!("\t\t{}: restore{}Ref(d),\n", f.name, n));
                }
            }
        }
        s.push_str("\t}\n}\n\n");
    }

    // Interface + package-level impl variable. Business code (impl.go) assigns
    // `GolangCallImpl = &Golang{}` in its init(); upstream rust2go emits this
    // declaration in gen.go, so we must too or `go build` fails with
    // "undefined: GolangCallImpl".
    s.push_str("var GolangCallImpl GolangCall\n\n");
    s.push_str("type GolangCall interface {\n");
    for m in &schema.methods {
        let req = go_ptr(&m.req);
        let ret = m
            .resp
            .as_ref()
            .map(|r| format!(" {}", r))
            .unwrap_or_default();
        s.push_str(&format!("\t{}(req {}){}\n", m.name, req, ret));
    }
    s.push_str("}\n\n");

    s.push_str("func dispatch(methodID uint32, payload []byte) (out []byte, ok bool) {\n");
    s.push_str("\tdefer func() {\n");
    s.push_str("\t\tif r := recover(); r != nil {\n");
    s.push_str("\t\t\tfmt.Fprintf(os.Stderr, \"rust2go-ipc: Go handler panic: %v\\n\", r)\n");
    s.push_str("\t\t\tout, ok = nil, false\n");
    s.push_str("\t\t}\n");
    s.push_str("\t}()\n");
    s.push_str("\td := &dec{b: payload}\n");
    s.push_str("\tswitch methodID {\n");
    for m in &schema.methods {
        if m.resp.is_none() {
            let new_fn = format!("new{}", m.req);
            s.push_str(&format!(
                "\tcase {}:\n\t\tr := {}(restore{}Ref(d))\n\t\tGolangCallImpl.{}(&r)\n\t\treturn nil, true\n",
                m.index, new_fn, m.req, m.name
            ));
        } else {
            let resp = m.resp.as_deref().expect("resp");
            let new_fn = format!("new{}", m.req);
            s.push_str(&format!(
                "\tcase {}:\n\t\tr := {}(restore{}Ref(d))\n\t\tresp := GolangCallImpl.{}(&r)\n\t\te := &enc{{}}\n\t\tflatten{}(&resp, e)\n\t\treturn e.b, true\n",
                m.index, new_fn, m.req, m.name, resp
            ));
        }
    }
    s.push_str("\t}\n\treturn nil, false\n}\n");

    s
}

fn go_ptr(n: &str) -> String {
    format!("*{}", n)
}

fn generate_go_main() -> String {
    r#"// Code generated by rust2go-ipc. DO NOT EDIT.
package main

import (
	"encoding/binary"
	"io"
	"net"
	"os"
)

const magic = "R2GI"
const version = uint16(1)

func main() {
	transport := os.Getenv("RUST2GO_IPC_TRANSPORT")
	endpoint := os.Getenv("RUST2GO_IPC_ENDPOINT")
	if endpoint == "" {
		// Backward compat: older Rust clients only set RUST2GO_IPC_PORT.
		port := os.Getenv("RUST2GO_IPC_PORT")
		if port == "" {
			panic("RUST2GO_IPC_ENDPOINT / RUST2GO_IPC_PORT not set")
		}
		transport = "tcp"
		endpoint = "127.0.0.1:" + port
	}
	if transport == "" {
		transport = "tcp"
	}

	var ln net.Listener
	var err error
	switch transport {
	case "unix":
		// A killed server leaves its socket file behind. Unlink before listen
		// so a respawned process can rebind the same path.
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
	for {
		conn, err := ln.Accept()
		if err != nil {
			continue
		}
		go serve(conn)
	}
}

func serve(conn net.Conn) {
	defer conn.Close()
	hdr := make([]byte, 22)
	for {
		if _, err := io.ReadFull(conn, hdr); err != nil {
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
		length := int(binary.LittleEndian.Uint32(hdr[18:22]))
		if length > 64*1024*1024 {
			return
		}
		payload := make([]byte, length)
		if _, err := io.ReadFull(conn, payload); err != nil {
			return
		}

		if reqID == 0 {
			_, _ = dispatch(methodID, payload)
			continue // oneway: no response frame
		}
		resp, ok := dispatch(methodID, payload)
		status := byte(0)
		if !ok {
			status = 1
		}
		out := make([]byte, 19+len(resp))
		copy(out[0:4], magic)
		binary.LittleEndian.PutUint16(out[4:6], version)
		binary.LittleEndian.PutUint64(out[6:14], reqID)
		out[14] = status
		binary.LittleEndian.PutUint32(out[15:19], uint32(len(resp)))
		copy(out[19:], resp)
		if _, err := conn.Write(out); err != nil {
			return
		}
	}
}

"#
    .to_string()
}
