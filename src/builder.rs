//! Build-time API mirroring `rust2go::Builder`. Instead of cgo + bindgen, this
//! parses the Rust schema and emits pure-Go sources plus Rust binding mirrors.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

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

/// Abstraction over the `go build` invocation. Mirrors rust2go v0.4's trait so
/// existing `build.rs` code keeps compiling: `Builder::build()` is available
/// for every `GoCompiler`, not only `CustomArgGoCompiler`.
///
/// Default implementations are empty so a marker compiler (like
/// [`DefaultGoCompiler`]) needs no extra code.
pub trait GoCompiler {
    /// Extra command-line arguments passed to `go build`.
    fn args(&self) -> Vec<OsString> {
        Vec::new()
    }

    /// Extra environment variables applied to the `go build` command.
    fn envs(&self) -> Vec<(OsString, OsString)> {
        Vec::new()
    }
}

impl GoCompiler for DefaultGoCompiler {
    fn envs(&self) -> Vec<(OsString, OsString)> {
        // Match `CustomArgGoCompiler::new()` so switching between the two
        // compiler types does not change module resolution behaviour.
        vec![(OsString::from("GO111MODULE"), OsString::from("on"))]
    }
}

impl GoCompiler for CustomArgGoCompiler {
    fn args(&self) -> Vec<OsString> {
        self.args.clone()
    }

    fn envs(&self) -> Vec<(OsString, OsString)> {
        self.envs.clone()
    }
}

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

/// Write a generated file atomically: truncate-in-place (fs::write) can leave
/// a half-written .go file if the build is killed mid-write (OOM/signal/disk
/// full). Since generated outputs are excluded from rerun-if-changed, a stale
/// truncated file would be fed straight to `go build` with a confusing syntax
/// error. A same-directory temp file + rename is atomic on all targets, so the
/// destination is always either the old complete file or the new complete one.
fn write_generated(dst: &std::path::Path, bytes: &[u8]) {
    let ext = dst.extension().and_then(|e| e.to_str()).unwrap_or("");
    // Include the process id so two concurrent `cargo build` processes sharing
    // the same go_src cannot clobber each other's temp/backup files. Without
    // it, process A could rename its temp into dst while process B is still
    // writing the same path, or A's backup cleanup could delete B's backup.
    let pid = std::process::id();
    let tmp = dst.with_extension(format!("{ext}.{pid}.tmp"));
    fs::write(&tmp, bytes)
        .unwrap_or_else(|e| panic!("rust2go-ipc: cannot write temp {}: {e}", tmp.display()));
    // Unix rename() atomically replaces dst. Windows rename may refuse to
    // overwrite a dst that is momentarily locked by an editor/AV, so first move
    // the old complete file aside; if installing the new file then fails we
    // restore the backup instead of leaving dst missing (which would make the
    // subsequent `go build` fail on a file that used to be perfectly valid).
    #[cfg(windows)]
    let bak = dst.with_extension(format!("{ext}.{pid}.bak"));
    #[cfg(windows)]
    if dst.exists() {
        let _ = fs::remove_file(&bak); // drop a stale backup from a prior failed run
        fs::rename(dst, &bak)
            .unwrap_or_else(|e| panic!("rust2go-ipc: cannot back up {}: {e}", dst.display()));
    }
    let installed = fs::rename(&tmp, dst);
    #[cfg(windows)]
    if installed.is_err() && bak.exists() {
        let _ = fs::rename(&bak, dst); // best-effort rollback to the previous file
    }
    installed.unwrap_or_else(|e| panic!("rust2go-ipc: cannot install {}: {e}", dst.display()));
    #[cfg(windows)]
    let _ = fs::remove_file(&bak); // new file is in place; drop the backup
}

/// Format a generated Go file atomically. `gofmt -w` truncates the file
/// in place, which would defeat [`write_generated`]'s atomic-install guarantee
/// (a crash mid-format leaves a half-written .go that breaks `go build`, and
/// on Windows an editor/AV holding the file makes `-w` fail). Instead feed the
/// bytes on stdin, capture stdout, then atomically install the formatted result.
/// On any failure the previously installed complete file is left untouched.
fn gofmt_in_place(path: &std::path::Path, gofmt: &PathBuf) {
    let src = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "cargo:warning=rust2go-ipc: cannot read {} for gofmt: {e}",
                path.display()
            );
            return;
        }
    };
    let mut child = match Command::new(gofmt)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            println!(
                "cargo:warning=rust2go-ipc: cannot run gofmt for {}: {e}",
                path.display()
            );
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Err(e) = stdin.write_all(&src) {
            println!(
                "cargo:warning=rust2go-ipc: gofmt stdin write failed for {}: {e}",
                path.display()
            );
        }
    }
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            println!(
                "cargo:warning=rust2go-ipc: gofmt wait failed for {}: {e}",
                path.display()
            );
            return;
        }
    };
    if !out.status.success() {
        println!(
            "cargo:warning=rust2go-ipc: gofmt exited with {} for {}",
            out.status,
            path.display()
        );
        return;
    }
    write_generated(path, &out.stdout);
}

/// Recursively collect every `.go` source under `dir`. `go build .` compiles
/// imported sub-packages too, so watching only the top level would let edits
/// to `go_dir/internal/*.go` leave a stale server binary in the Cargo cache.
/// Symlinked directories are not followed (avoids cycles); depth is capped so
/// a pathological tree cannot stall a build.
fn collect_go_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_go_sources(&p, out, depth + 1);
        } else if p.extension().and_then(|e| e.to_str()) == Some("go") {
            out.push(p);
        }
    }
}

impl<GOC: GoCompiler> Builder<PathBuf, GOC> {
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
        // silently compile a stale file. Fail loudly instead. Compare the
        // canonicalized parents so an equivalent path spelled with `.`/`..`
        // (e.g. `./golang/gen.go`) is not rejected, while a genuinely different
        // directory still fails. Canonicalize the parent, not the file: the
        // generated file may not exist yet on a fresh build.
        let regen_parent = regen_dst.parent().unwrap_or(std::path::Path::new(""));
        let canon_go_dir = fs::canonicalize(&go_dir).unwrap_or_else(|e| {
            panic!(
                "rust2go-ipc: cannot resolve go_src {}: {e}",
                go_dir.display()
            )
        });
        let canon_regen_parent = fs::canonicalize(regen_parent).unwrap_or_else(|e| {
            panic!(
                "rust2go-ipc: cannot resolve regen dst parent {}: {e}",
                regen_parent.display()
            )
        });
        assert_eq!(
            canon_regen_parent,
            canon_go_dir,
            "rust2go-ipc: regen dst must be inside go_src ({} vs {})",
            regen_dst.display(),
            go_dir.display()
        );

        println!("cargo:rerun-if-changed={}", src_rs.display());
        // External linking is selected by the build image via RUST2GO_GOFLAGS
        // (e.g. "-linkmode external"). Re-run the build script when it changes
        // so a CI cache does not keep serving a stale, non-PIE server binary.
        println!("cargo:rerun-if-env-changed=RUST2GO_GOFLAGS");
        // go version is discovered via PATH; a toolchain switch must re-run us.
        println!("cargo:rerun-if-env-changed=PATH");
        // Go module/workspace files change the server build even though they
        // are not `.go` sources: a new dependency version or a rewritten sum
        // must re-run this script, otherwise Cargo keeps serving the stale
        // go-ipc-server binary built from the previous dependency graph.
        println!("cargo:rerun-if-changed={}", go_dir.join("go.mod").display());
        println!("cargo:rerun-if-changed={}", go_dir.join("go.sum").display());
        println!(
            "cargo:rerun-if-changed={}",
            go_dir.join("go.work").display()
        );
        // Fail fast: the generated server's sem-wait timer reuse relies on Go
        // >= 1.23 synchronous timer channels. On 1.20–1.22 a stale timer value
        // can survive Stop+drain and prematurely close connections. A hidden
        // runtime bug is worse than a clear build-time error, so gate here.
        require_go_1_23(&self.go_comp, &go_dir);
        let ipc_main_path = go_dir.join("ipc_main.go");
        // Canonicalize the two generated outputs once so equivalent spellings
        // (`./golang/gen.go` via with_regen vs `golang/gen.go` from read_dir)
        // compare equal. Otherwise regen_dst would be watched as an input while
        // also being rewritten as an output -> Cargo rebuilds forever.
        let canon_regen_dst = fs::canonicalize(&regen_dst).unwrap_or_else(|_| regen_dst.clone());
        let canon_ipc_main =
            fs::canonicalize(&ipc_main_path).unwrap_or_else(|_| ipc_main_path.clone());
        let mut go_sources = Vec::new();
        collect_go_sources(&go_dir, &mut go_sources, 0);
        for p in go_sources {
            // Do not watch our own generated outputs: Cargo would see the
            // mtime bump on every run and rebuild forever. Hand-written Go
            // sources (impl.go, internal/*.go, ...) remain watched, including
            // imported sub-packages that feed `go build .`.
            let canon_p = fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            if canon_p != canon_regen_dst && canon_p != canon_ipc_main {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }

        let schema_src = fs::read_to_string(&src_rs).unwrap_or_else(|e| {
            panic!(
                "rust2go-ipc: cannot read schema source {}: {e}",
                src_rs.display()
            )
        });
        let schema = parse_schema(&schema_src);
        let binding_name = self.binding_name.as_deref().unwrap_or("_go_bindings.rs");
        let port = crate::ipc::free_port();

        let rust = generate_rust_bindings(&schema, port);
        write_generated(&out_dir.join(binding_name), rust.to_string().as_bytes());

        let go_gen = generate_go_sources(&schema);
        write_generated(&regen_dst, go_gen.as_bytes());

        // ipc_main.go is a generated artifact, rewritten on every build. This
        // is expected (the project deliberately does not hand-edit it), so do
        // not emit a cargo warning — it would fire on every build because the
        // on-disk copy is gofmt'd while `main_src` is not, making a byte equality
        // check perpetually false and producing persistent build noise.
        let ipc_main = go_dir.join("ipc_main.go");
        let main_src = generate_go_main();
        write_generated(&ipc_main, main_src.as_bytes());

        // Keep generated Go sources gofmt-clean when the tool is available,
        // installing the formatted bytes atomically (see gofmt_in_place).
        if let Some(gofmt) = find_gofmt() {
            for path in [&regen_dst, &ipc_main] {
                gofmt_in_place(path, &gofmt);
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
        for (k, v) in self.go_comp.envs() {
            cmd.env(k, v);
        }
        let extra_ldflags = env::var("RUST2GO_GOFLAGS").unwrap_or_default();
        let build_args = with_extra_ldflags(self.go_comp.args(), &extra_ldflags);
        for a in &build_args {
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

        // On Windows the path's backslashes must become '/' because env!()
        // inlines it as a Rust string literal where '\' starts an escape, and
        // Command::new accepts '/' on Windows. On Unix '\' is a legal filename
        // byte, so converting it would point at a different (nonexistent) path.
        #[cfg(windows)]
        let server_lit = server_out.to_string_lossy().replace('\\', "/");
        #[cfg(not(windows))]
        let server_lit = server_out.to_string_lossy().into_owned();
        println!("cargo:rustc-env=RUST2GO_IPC_SERVER={server_lit}");
        println!("cargo:rustc-env=RUST2GO_IPC_PORT={port}");
    }
}

/// Merge extra linker flags (from `RUST2GO_GOFLAGS`) into the go build args.
///
/// Some linker flags — notably `-linkmode external` — cannot be passed as a
/// standalone `go build` flag and are rejected by Go's own `GOFLAGS` env var
/// ("unknown flag -linkmode"); they are only valid inside `-ldflags`. Build
/// images that need external linking therefore set `RUST2GO_GOFLAGS` (read by
/// this builder) instead of `GOFLAGS`. The value is appended to the existing
/// `-ldflags` value, or a fresh `-ldflags` is emitted when none is present.
fn with_extra_ldflags(mut args: Vec<OsString>, extra: &str) -> Vec<OsString> {
    let extra = extra.trim();
    if extra.is_empty() {
        return args;
    }
    // Locate `-ldflags` (either `-ldflags VALUE` or `-ldflags=VALUE`) and
    // append extra to its value so all linker flags reach cmd/link together.
    for i in 0..args.len() {
        let cur = args[i].to_string_lossy();
        if cur == "-ldflags" {
            if i + 1 < args.len() {
                // Append, do not prepend: cmd/link lets the last occurrence of
                // a string flag win, so an env-injected -linkmode must be able
                // to override a conflicting value already in build.rs.
                let mut merged = OsString::from(&args[i + 1]);
                merged.push(" ");
                merged.push(extra);
                args[i + 1] = merged;
            } else {
                // `-ldflags` with no value: treat the next push as its value.
                args.push(OsString::from(extra));
            }
            return args;
        }
        if let Some(val) = cur.strip_prefix("-ldflags=") {
            let merged = format!("{val} {extra}");
            args[i] = OsString::from(format!("-ldflags={merged}"));
            return args;
        }
    }
    // No -ldflags present: append one. Go accepts `-ldflags` anywhere before
    // the package argument, and build.rs always puts `.` last via cmd.arg(".").
    args.push(OsString::from("-ldflags"));
    args.push(OsString::from(extra));
    args
}

/// Locate gofmt: prefer PATH, then `$GOROOT/bin/gofmt`, then `go env GOROOT`.
/// Assert the Go toolchain in PATH is >= 1.23. The generated IPC server reuses
/// a single `time.Timer` via Stop+drain+Reset; only Go 1.23+ has the synchronous
/// timer channel that makes draining exact. Older toolchains can leave a stale
/// tick in the channel and prematurely close connections under load — a build
/// error here is far easier to diagnose than intermittent runtime drops.
fn require_go_1_23(go_comp: &impl GoCompiler, go_dir: &std::path::Path) {
    let mut cmd = Command::new("go");
    // Run in the same module dir as `go build`: since Go 1.21 the toolchain is
    // chosen from go.mod + GOTOOLCHAIN. Probing from the crate root could see a
    // different toolchain than the actual build and either refuse wrongly or
    // miss a pre-1.23 toolchain the build will really use.
    cmd.current_dir(go_dir);
    cmd.args(["env", "GOVERSION"]);
    for (k, v) in go_comp.envs() {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap_or_else(|e| {
        panic!("rust2go-ipc: cannot run `go env GOVERSION` (Go >= 1.23 required): {e}")
    });
    assert!(
        out.status.success(),
        "rust2go-ipc: `go env GOVERSION` failed; Go >= 1.23 is required"
    );
    let raw = String::from_utf8_lossy(&out.stdout);
    let v = raw.trim();
    // Typical forms: "go1.26.4", "go1.23", "devel go1.24-abc123".
    let digits: Vec<u32> = v
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse().ok())
        .take(2)
        .collect();
    let ok = match digits.as_slice() {
        [major, _] if *major > 1 => true,
        [1, minor] => *minor >= 23,
        _ => false,
    };
    assert!(
        ok,
        "rust2go-ipc requires Go >= 1.23 (generated server uses synchronous timer channels), got `{}`",
        v
    );
}

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
    /// Modules from the schema root down to this struct (empty = top level).
    /// Generated bindings live in a `binding` module, so a nested struct's
    /// owned path must be `super::<mod>::<...>::<Name>`, not just `super::Name`.
    module_path: Vec<String>,
}

#[derive(Clone)]
struct ParamDef {
    name: String,
    ty: Type,
    /// Whether the trait declared this parameter by reference (`&T`). The
    /// `ty` field is the stripped owned type used by the wire/flatten path,
    /// but the `#[r2g_result]` wrapper forwards the trait's original argument,
    /// so the Result try-stub must keep the reference in its signature and use
    /// `ToRef::calc_ref(id)` (not `&id`) for such parameters.
    is_reference: bool,
    /// `&mut T` vs `&T`. The async slot attachment is type-erased, so its
    /// `SlotWriter<A>` must match the upstream `ResponseFuture` attachment
    /// exactly — including mutability — or its Drop reinterprets the borrowed
    /// pointer with the wrong type.
    is_mutable: bool,
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

/// Flatten top-level items and items nested inside inline `mod { ... }` blocks.
/// The proc macro does not require schema types to be top-level, so the builder
/// must see nested R2G structs/traits too; otherwise they are silently dropped
/// and references to them report a misleading "missing derive" error.
fn collect_items<'a>(
    items: &'a [Item],
    mod_path: &mut Vec<String>,
    depth: u32,
    out: &mut Vec<(&'a Item, Vec<String>)>,
) {
    // `syn::parse_file` is itself recursive, but an explicit cap gives a clear
    // diagnostic instead of a stack abort on pathologically nested input.
    assert!(
        depth <= 64,
        "rust2go-ipc: schema module nesting exceeds 64 levels"
    );
    for item in items {
        // The builder reads a single source file and does not expand macros.
        // `include!("types.rs")` could hide R2G structs from us while the proc
        // macro (which does get expanded by rustc) still emits references to
        // them, producing a confusing "missing derive"/undefined-symbol error.
        // Fail at build time with an actionable message, same as external mods.
        if let Item::Macro(m) = item {
            if m.mac.path.is_ident("include") {
                panic!(
                    "rust2go-ipc: include!() is not supported in schema sources; \
                     inline the R2G types into the schema file instead"
                );
            }
        }
        out.push((item, mod_path.clone()));
        if let Item::Mod(m) = item {
            reject_schema_cfg(&m.attrs, &format!("module `{}`", bare_ident(&m.ident)));
            if let Some((_, inner)) = &m.content {
                mod_path.push(m.ident.to_string());
                collect_items(inner, mod_path, depth + 1, out);
                mod_path.pop();
            } else {
                // Only the single source path is read; a `mod foo;` whose body
                // lives in foo.rs cannot be parsed here. Silently skipping it
                // would drop any R2G types inside and surface a misleading
                // "missing derive" error at use sites. Fail at build time.
                panic!(
                    "rust2go-ipc: external module `{}` is not supported in schema sources; \
                     inline it (`mod {} {{ ... }}`) so R2G types are visible",
                    bare_ident(&m.ident),
                    bare_ident(&m.ident)
                );
            }
        }
    }
}

/// Reject `#[cfg]`/`#[cfg_attr]` on schema items. The builder parses source
/// without evaluating cfg, so a cfg-gated struct/trait/method would be emitted
/// unconditionally into Go, disagreeing with the Rust side (which sees only
/// activated items after cfg stripping). Fail fast with an actionable message.
fn reject_schema_cfg(attrs: &[syn::Attribute], what: &str) {
    assert!(
        !attrs
            .iter()
            .any(|a| { a.path().is_ident("cfg") || a.path().is_ident("cfg_attr") }),
        "rust2go-ipc: #[cfg] on {what} is not supported in schema sources; \
         keep R2G types/methods unconditional"
    );
}

fn parse_schema(src: &str) -> Schema {
    let file = syn::parse_file(src).expect("rust2go-ipc: cannot parse schema source");
    let mut schema = Schema::default();
    let mut all_items = Vec::new();
    let mut mod_path = Vec::new();
    collect_items(&file.items, &mut mod_path, 0, &mut all_items);

    for (item, path) in &all_items {
        if let Item::Struct(s) = item {
            if has_derive(s, "R2G") {
                reject_schema_cfg(&s.attrs, &format!("struct `{}`", bare_ident(&s.ident)));
                // Generics carry no wire representation; reject clearly rather
                // than letting `T` fall through to a bogus "missing derive".
                assert!(
                    s.generics.params.is_empty(),
                    "rust2go-ipc: generic struct `{}` is not wire-encodable; use a concrete type",
                    bare_ident(&s.ident)
                );
                schema.structs.push(parse_struct(s, path.clone()));
            }
        }
    }
    let struct_names: HashSet<String> = schema.structs.iter().map(|s| s.name.clone()).collect();

    // Method IDs are global: multiple #[r2g] traits must not produce duplicate
    // Go `case` labels.
    let mut idx = 0u32;
    let mut stub_names = HashSet::new();
    for (item, _path) in &all_items {
        let Item::Trait(t) = item else { continue };
        if !has_marker_attr(&t.attrs, "r2g") {
            continue;
        }
        reject_schema_cfg(&t.attrs, &format!("trait `{}`", bare_ident(&t.ident)));
        let trait_name = bare_ident(&t.ident);
        validate_ident(&trait_name, "trait");
        if !schema.traits.contains(&trait_name) {
            schema.traits.push(trait_name.clone());
        }
        for m in &t.items {
            let TraitItem::Fn(f) = m else { continue };
            let name = bare_ident(&f.sig.ident);
            reject_schema_cfg(&f.attrs, &format!("method `{name}`"));
            validate_ident(&name, "method");
            let resp = match &f.sig.output {
                ReturnType::Type(_, ty) => Some(strip_reference(ty).0),
                ReturnType::Default => None,
            };
            // Upstream rejects `async fn` without a response ("async function
            // must have a return value"). Without this guard the builder
            // silently emits a oneway stub that can never be called from a
            // successfully compiled crate.
            assert!(
                !(f.sig.asyncness.is_some() && resp.is_none()),
                "rust2go-ipc: async method `{name}` must have a return value (upstream #[r2g] rejects it)"
            );
            // Upstream `type_to_segment` accepts single-segment paths only,
            // and its List converter recurses into element types. Check the
            // RAW types (before `strip_reference`) so `&types::Req` and
            // `Vec<types::Req>` are rejected here instead of panicking inside
            // the upstream macro at the consumer's compile time.
            if let ReturnType::Type(_, ty) = &f.sig.output {
                assert!(
                    !matches!(&**ty, Type::Reference(_)),
                    "rust2go-ipc: reference return type in method `{name}` is rejected by upstream #[r2g]; only path types and `impl Future` returns are supported"
                );
                reject_qualified_paths(ty, &format!("method `{name}` return"));
            }
            for arg in &f.sig.inputs {
                if let FnArg::Typed(p) = arg {
                    reject_qualified_paths(&p.ty, &format!("parameter in method `{name}`"));
                }
            }
            let mut params = Vec::new();
            let mut param_names = HashSet::new();
            for arg in &f.sig.inputs {
                let FnArg::Typed(p) = arg else {
                    panic!("rust2go-ipc: receiver is not supported in #[r2g] trait methods");
                };
                // cfg is not evaluated at schema-parse time. A cfg-gated
                // parameter would be emitted into Go/Rust bindings
                // unconditionally while rustc strips it before macro
                // expansion, producing an argument-count mismatch that is hard
                // to diagnose. Fail fast like struct/trait/method/field.
                reject_schema_cfg(&p.attrs, &format!("parameter in method `{name}`"));
                let pname = match &*p.pat {
                    Pat::Ident(i) => bare_ident(&i.ident),
                    _ => panic!("rust2go-ipc: only simple parameter patterns are supported"),
                };
                validate_ident(&pname, "parameter");
                assert!(
                    param_names.insert(pname.clone()),
                    "rust2go-ipc: duplicate parameter name `{pname}` in method `{name}`"
                );
                let reserved = ALWAYS_RESERVED_PARAM_NAMES.contains(&pname.as_str())
                    || (resp.is_some() && RESPONSE_RESERVED_PARAM_NAMES.contains(&pname.as_str()));
                assert!(
                    !reserved && !pname.starts_with(GENERATED_IDENT_PREFIX),
                    "rust2go-ipc: parameter name `{pname}` conflicts with a generated identifier; rename it"
                );
                let is_mutable = matches!(&*p.ty, Type::Reference(r) if r.mutability.is_some());
                // Upstream `ParamType::to_tokens` emits `&T` for any reference
                // param and drops `mut`, so its generated Impl signature is
                // `fn(&T)` while the trait declares `fn(&mut T)` — E0053 in
                // every consumer. No configuration makes this usable.
                assert!(
                    !is_mutable,
                    "rust2go-ipc: `&mut` parameter `{pname}` is unsupported by upstream #[r2g]; use an owned value or `&T`"
                );
                let (ty, is_reference) = strip_reference(&p.ty);
                params.push(ParamDef {
                    name: pname,
                    ty,
                    is_reference,
                    is_mutable,
                });
            }
            // Shared-memory calls need rust2go_mem_ffi and a ring handler in
            // the Go server, neither of which exists in the IPC transport.
            // Reject any form of the attribute (including the qualified /
            // parenthesised ones upstream ignores) instead of silently
            // emitting an ordinary IPC stub for a handler that expects a ring
            // task.
            assert!(
                !has_marker_attr(&f.attrs, "mem") && !has_marker_attr(&f.attrs, "shm"),
                "rust2go-ipc: #[mem]/#[shm] shared-memory calls are not supported over IPC"
            );
            let go_ptr = !has_bare_marker_attr(&f.attrs, "go_pass_struct");
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
    // Use the converged wire width, not just `fields.is_empty()`: a composite
    // such as `Wrapper { inner: Empty }` also collapses to zero wire bytes and
    // is equally unencodable as a list element. struct_wire_widths runs a
    // fixed-point iteration so nested zero-width structs are caught.
    let widths_for_empty = struct_wire_widths(&schema);
    let empty_structs: HashSet<String> = widths_for_empty
        .iter()
        .filter(|(_, w)| **w == 0)
        .map(|(name, _)| name.clone())
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

/// Upstream rust2go-common calls `type_to_segment` on every path type and
/// recurses into List element types when building converters. Both require a
/// single, unqualified path segment. Reject qualified paths recursively so a
/// nested `types::Req` fails at schema parse time instead of panicking inside
/// the upstream macro when the consumer compiles its generated code.
fn reject_qualified_paths(ty: &Type, ctx: &str) {
    match ty {
        Type::Path(p) => {
            assert!(
                p.path.leading_colon.is_none() && p.path.segments.len() == 1,
                "rust2go-ipc: multi-segment type in {ctx} is rejected by upstream #[r2g]; use a single-segment alias (`use m::T;` then `T`)"
            );
            for seg in &p.path.segments {
                match &seg.arguments {
                    syn::PathArguments::AngleBracketed(args) => {
                        for arg in &args.args {
                            if let syn::GenericArgument::Type(t) = arg {
                                reject_qualified_paths(t, ctx);
                            }
                        }
                    }
                    syn::PathArguments::Parenthesized(args) => {
                        for t in &args.inputs {
                            reject_qualified_paths(t, ctx);
                        }
                        if let ReturnType::Type(_, t) = &args.output {
                            reject_qualified_paths(t, ctx);
                        }
                    }
                    syn::PathArguments::None => {}
                }
            }
        }
        Type::Reference(r) => reject_qualified_paths(&r.elem, ctx),
        _ => {}
    }
}

fn has_last_segment(path: &syn::Path, name: &str) -> bool {
    path_last_segment(path) == name
}

fn has_marker_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    attrs.iter().any(|a| has_last_segment(a.path(), name))
}

/// Match upstream rust2go-common's marker-attribute rule exactly: a bare,
/// argument-free single-segment path (`#[go_pass_struct]`, `#[mem]`, ...).
/// `has_last_segment` would also accept `#[go_pass_struct()]` and
/// `#[rust2go::go_pass_struct]`, which upstream ignores; the mismatch silently
/// changes the Go interface signature and breaks handlers written against
/// upstream semantics.
fn has_bare_marker_attr(attrs: &[syn::Attribute], name: &str) -> bool {
    let id = format_ident!("{}", name);
    attrs
        .iter()
        .any(|a| matches!(&a.meta, syn::Meta::Path(p) if p.get_ident() == Some(&id)))
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
                    // proc-macro2 prints `r2g::R2G` as `r2g :: R2G`, so the
                    // last segment carries a leading space. Trim it before
                    // comparing or qualified derive paths get silently missed.
                    .any(|s| s.rsplit("::").next().map(str::trim) == Some(name))
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

fn parse_struct(s: &ItemStruct, module_path: Vec<String>) -> StructDef {
    let name = bare_ident(&s.ident);
    let fields = match &s.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let fname = bare_ident(f.ident.as_ref().expect("named field"));
                // cfg is not evaluated when parsing source. A cfg-gated field
                // would be emitted into Go unconditionally while rustc strips
                // it before derive expansion, producing a Ref layout mismatch
                // and silent field misreads. Fail fast like struct/trait/method.
                reject_schema_cfg(
                    &f.attrs,
                    &format!("field `{name}.{fname}`"),
                );
                // Struct fields go through the same upstream ParamType
                // conversion as method params; a qualified path (including
                // inside a List element) makes upstream panic at derive time.
                reject_qualified_paths(&f.ty, &format!("field `{name}.{fname}`"));
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
    StructDef {
        name,
        fields,
        module_path,
    }
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

/// `enc` is always shadowed by the encoder local before flatten runs.
/// `slot`/`cb` only collide in methods that have a response (they are appended
/// callback parameters there); oneway methods do not emit them.
/// (`dec`/`handler` live in separate scopes or are declared after the flatten
/// statements consume the parameters, so they are deliberately not reserved.)
/// Reject at build time instead of emitting uncompilable Rust.
const ALWAYS_RESERVED_PARAM_NAMES: &[&str] = &["enc"];
const RESPONSE_RESERVED_PARAM_NAMES: &[&str] = &["slot", "cb"];
/// The generator emits `__r2g_*` bindings and helper calls inside the same
/// stub scope as user parameters. A parameter using that prefix (e.g.
/// `__r2g_flatten_Foo`) would shadow a helper function and break compilation.
const GENERATED_IDENT_PREFIX: &str = "__r2g_";

fn validate_ident(name: &str, what: &str) {
    const RUST: &[&str] = &[
        "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref",
        "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
        "use", "where", "while", "async", "await", "dyn", "abstract", "become", "box", "do",
        "final", "macro", "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
        // `gen` became a reserved keyword in edition 2024 (gen blocks). The
        // generated bindings are compiled in the consumer's edition, so a
        // schema name of `gen`/`r#gen` would emit a bare `gen` ident that fails
        // to compile there. Reject up front with an actionable message.
        "gen",
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
    "_require64bitGoTarget",
    "maxRestoreDepth",
    "maxAcceptErrs",
    "sem",
    "semWaitTimeout",
    "acceptBackoff",
    "main",
    "serve",
    "init",
    // Go predeclared identifiers that a same-named type would shadow.
    // Note: `comparable`, `min`, `max`, `clear` (added in Go 1.18/1.21) are
    // deliberately NOT rejected — Go permits shadowing predeclared names and
    // the generated code never calls them, so a user type named `max` is legal
    // and must keep working. Only names the generator itself relies on belong
    // in this list.
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

fn rust_owned_kind_path(kind: &FieldKind, schema: &Schema) -> TokenStream {
    match kind {
        FieldKind::String => quote! { ::std::string::String },
        FieldKind::List(elem) => {
            let inner = rust_owned_kind_path(elem, schema);
            quote! { ::std::vec::Vec<#inner> }
        }
        FieldKind::Custom(n) => {
            let id = format_ident!("{}", n);
            // The generated file is included from a `binding` module. A struct
            // declared at the schema root lives in `super`; one declared inside
            // `mod types { ... }` lives at `super::types`. Emit the full path so
            // nested-module schemas actually compile (collect_items now finds
            // them, so this path must follow).
            let mods = schema
                .structs
                .iter()
                .find(|s| s.name == *n)
                .map(|s| s.module_path.as_slice())
                .unwrap_or(&[]);
            let segs: Vec<syn::Ident> = mods.iter().map(|m| format_ident!("{}", m)).collect();
            quote! { super::#(#segs::)*#id }
        }
        FieldKind::Primitive(p) => {
            let id = format_ident!("{}", p);
            quote! { #id }
        }
    }
}

/// Owned type as seen from the generated `binding` module. Schema structs live
/// in the parent module (or a module nested within it), hence the `super::`
/// prefix (the generated file is included from a sibling `binding` module).
fn rust_owned_path(ty: &Type, schema: &Schema) -> TokenStream {
    match classify(ty) {
        FieldKind::List(_) if type_ident(ty) == "Option" => {
            let elem = generic_arg(ty).expect("Option<T>");
            let inner = rust_owned_path(&elem, schema);
            quote! { ::std::option::Option<#inner> }
        }
        other => rust_owned_kind_path(&other, schema),
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
    // Fixed-point iteration converges in at most N rounds (each round either
    // converges or widens at least one struct). Use the struct count instead of
    // a hard-coded 8 so deeply nested schemas cannot be underestimated.
    for _ in 0..schema.structs.len() {
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
            #[allow(non_snake_case, clippy::all, unused_variables, unsafe_op_in_unsafe_fn)]
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
            #[allow(non_snake_case, clippy::all, unused_variables, unsafe_op_in_unsafe_fn)]
            unsafe fn #name(dec: &mut ::rust2go::ipc::Decoder<'_>) -> #st_name {
                // Bound recursion for self-referential schemas / hostile frames.
                dec.enter();
                let restored = #st_name { #(#fields)* };
                dec.leave();
                restored
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
        let trait_name_str = m.trait_name.clone();
        let method_name_str = m.name.clone();
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
        // Result wrappers forward the trait's original argument types, which
        // may be references (`&T`). The try-stub signature must mirror that so
        // the call site type-checks; only the wire/flatten path uses the
        // stripped owned type. Mirror upstream's `ToRef::calc_ref` conversion:
        // owned params use `calc_ref(&id)`, reference params use `calc_ref(id)`
        // (upstream r2g.rs ref_marks). Whether Go treats the field as a pointer
        // (`go_ptr`) is a separate concern handled in the Go template's `star`
        // emission and must NOT drive this Rust-side borrow decision.
        let owned_param_decls: Vec<TokenStream> = m
            .params
            .iter()
            .map(|p| {
                let id = format_ident!("{}", p.name);
                let ty = rust_owned_path(&p.ty, schema);
                if p.is_reference {
                    quote! { #id: &#ty }
                } else {
                    quote! { #id: #ty }
                }
            })
            .collect();
        let to_ref_stmts: Vec<TokenStream> = m
            .params
            .iter()
            .map(|p| {
                let id = format_ident!("{}", p.name);
                let buf = format_ident!("__r2g_buf_{}", p.name);
                let tmp = format_ident!("__r2g_ref_{}", p.name);
                let ref_ty = rust_ref_type(&p.ty);
                let calc_arg = if p.is_reference {
                    quote! { #id }
                } else {
                    quote! { &#id }
                };
                quote! {
                    let (#buf, #tmp) = ::rust2go::ToRef::calc_ref(#calc_arg);
                    let #id: #ref_ty = ::std::mem::transmute(#tmp);
                }
            })
            .collect();

        match (&m.resp, m.is_async) {
            (Some(resp), is_async) => {
                let resp_ref = rust_ref_type(resp);
                let resp_owned = rust_owned_path(resp, schema);
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
                let legacy_body = if is_async {
                    quote! {
                        ::rust2go::ipc::dispatch(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            &enc.take(),
                            slot as *const (),
                            cb as *const (),
                            handler,
                        );
                    }
                } else {
                    // The old public dispatch_sync was removed in 0.2.2. Both
                    // stub families now share the non-panicking Result dispatch;
                    // only the legacy #[r2g] stub restores the panic here so its
                    // signature/behavior stays drop-in compatible.
                    quote! {
                        if let Err(err) = ::rust2go::ipc::dispatch_sync_result(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            #method_name_str,
                            &enc.take(),
                            slot as *const (),
                            cb as *const (),
                            handler,
                        ) {
                            panic!("rust2go-ipc: sync call to method {} failed: {}", #method_id, err);
                        }
                    }
                };
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc, unused_variables, unsafe_op_in_unsafe_fn)]
                    pub unsafe fn #stub(#(#param_decls,)* #slot_cb) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        {
                            let enc = &mut enc;
                            #(#flatten_stmts)*
                        }
                        let handler = ::rust2go::ipc::ResponseHandler::new(#tramp, #cleanup_arg);
                        #legacy_body
                    }
                });

                if is_async {
                    // The upstream future owns the slot and has no error
                    // channel, so an oversized payload discovered inside
                    // `dispatch` would panic on poll (fatal under panic=abort).
                    // Generate a pre-flight length helper so `#[r2g_result]`
                    // can return IpcError::PayloadTooLarge *before* creating
                    // the future. It encodes the same way as the stub and
                    // borrows all arguments, so callers keep ownership.
                    let payload_len_stub = format_ident!("__r2g_payload_len_{}", stub);
                    // Every parameter is borrowed as `&OwnedTy`: the wrapper
                    // must still own the arguments when it later calls Impl.
                    // For a trait param declared `&T` this is already `&T`;
                    // for an owned `T` the caller passes `&arg`.
                    let len_param_decls: Vec<TokenStream> = m
                        .params
                        .iter()
                        .map(|p| {
                            let id = format_ident!("{}", p.name);
                            let ty = rust_owned_path(&p.ty, schema);
                            quote! { #id: &#ty }
                        })
                        .collect();
                    let to_ref_for_len: Vec<TokenStream> = m
                        .params
                        .iter()
                        .map(|p| {
                            let id = format_ident!("{}", p.name);
                            let buf = format_ident!("__r2g_buf_{}", p.name);
                            let tmp = format_ident!("__r2g_ref_{}", p.name);
                            let ref_ty = rust_ref_type(&p.ty);
                            quote! {
                                let (#buf, #tmp) = ::rust2go::ToRef::calc_ref(#id);
                                let #id: #ref_ty = ::std::mem::transmute(#tmp);
                            }
                        })
                        .collect();
                    let flatten_for_len = flatten_stmts.clone();
                    out.extend(quote! {
                        #[allow(non_snake_case, clippy::all, unused_variables, unsafe_op_in_unsafe_fn)]
                        pub unsafe fn #payload_len_stub(#(#len_param_decls,)*) -> usize {
                            let mut enc = ::rust2go::ipc::Encoder::new();
                            {
                                let enc = &mut enc;
                                #(#to_ref_for_len)*
                                #(#flatten_for_len)*
                            }
                            enc.take().len()
                        }
                    });
                }

                if !is_async {
                    let try_stub = format_ident!("__r2g_try_{}", stub);
                    let flatten_for_try = flatten_stmts.clone();
                    let to_ref_for_try = to_ref_stmts.clone();
                    out.extend(quote! {
                        #[allow(non_snake_case, clippy::missing_safety_doc, clippy::useless_transmute, clippy::transmute_ptr_to_ref, unused_variables, unsafe_op_in_unsafe_fn)]
                        pub unsafe fn #try_stub(#(#owned_param_decls,)*)
                            -> ::core::result::Result<#resp_owned, ::rust2go::IpcError>
                        {
                            let mut enc = ::rust2go::ipc::Encoder::new();
                            {
                                let enc = &mut enc;
                                #(#to_ref_for_try)*
                                #(#flatten_for_try)*
                            }
                            let mut slot: ::core::option::Option<#resp_owned> =
                                ::core::option::Option::None;
                            let handler = ::rust2go::ipc::ResponseHandler::new(
                                #tramp,
                                ::core::option::Option::None,
                            );
                            ::rust2go::ipc::dispatch_sync_result(
                                env!("RUST2GO_IPC_SERVER"),
                                #port,
                                #method_id,
                                #method_name_str,
                                &enc.take(),
                                // Derive the writable pointer from an exclusive
                                // borrow (`&mut`) so the write through the
                                // trampoline has a valid provenance under the
                                // aliasing model; `&slot as *const _` written
                                // through as *mut is technically UB.
                                &mut slot as *mut _ as *const (),
                                ::core::ptr::null(),
                                handler,
                            )?;
                            slot.take().ok_or(::rust2go::IpcError::NoResponse {
                                method: #method_name_str.to_string(),
                            })
                        }
                    });
                }

                // Upstream `ResponseFuture<Req, Resp>` attaches
                // `(ReqTuple, Vec<u8>)` to the slot, where ReqTuple is the
                // trait's ORIGINAL parameter tuple — including `&T` /
                // `&mut T` for by-reference params. The slot pointer is
                // type-erased, so a stripped owned type here compiles but
                // makes cleanup reinterpret `Some((&T, ..))` as
                // `Some((T, ..))` and destruct a String/Vec behind a borrowed
                // pointer. Preserve the borrow (and mutability) in A.
                let owned_types: Vec<TokenStream> = m
                    .params
                    .iter()
                    .map(|p| {
                        let ty = rust_owned_path(&p.ty, schema);
                        if p.is_reference {
                            if p.is_mutable {
                                quote! { &mut #ty }
                            } else {
                                quote! { &#ty }
                            }
                        } else {
                            ty
                        }
                    })
                    .collect();
                let write_back = if is_async {
                    // The slot writer's "drop exactly once" invariant must not
                    // depend on the write tail never panicking. Split into two
                    // phases:
                    //   1. restore (above) + convert to owned happen BEFORE
                    //      touching the slot. A panic here leaves the writer
                    //      unconsumed, so the reactor's cleanup is the sole,
                    //      correct drop.
                    //   2. from_ptr + write are wrapped in catch_unwind. From
                    //      the instant from_ptr runs, the writer is owned by
                    //      this frame and Rust unwind drops it exactly once;
                    //      swallowing the panic makes the trampoline return
                    //      Ok, so the reactor never runs cleanup a second time.
                    //      (A second drop after the reader was cancelled would
                    //      double-free the SlotInner.)
                    quote! {
                        let __r2g_owned: #resp_owned =
                            ::rust2go::FromRef::from_ref(::std::mem::transmute(&resp));
                        let __r2g_write = ::std::panic::catch_unwind(
                            ::std::panic::AssertUnwindSafe(|| {
                                ::rust2go::SlotWriter::<
                                    #resp_owned,
                                    ((#(#owned_types,)*), Vec<u8>),
                                >::from_ptr(slot).write(__r2g_owned);
                            }),
                        );
                        if __r2g_write.is_err() {
                            eprintln!(concat!(
                                "rust2go-ipc: response write panicked for ",
                                #trait_name_str, ".", #method_name_str,
                                " (slot released once; panic swallowed to avoid double drop)"
                            ));
                        }
                    }
                } else {
                    quote! {
                        *(slot as *mut Option<#resp_owned>) =
                            Some(::rust2go::FromRef::from_ref(::std::mem::transmute(&resp)));
                    }
                };
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc, unused_variables, unsafe_op_in_unsafe_fn)]
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
                        #[allow(non_snake_case, clippy::missing_safety_doc, unused_variables, unsafe_op_in_unsafe_fn)]
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
                let try_stub = format_ident!("__r2g_try_{}", stub);
                let flatten_for_try = flatten_stmts.clone();
                let to_ref_for_try = to_ref_stmts.clone();
                out.extend(quote! {
                    #[allow(non_snake_case, clippy::missing_safety_doc, unused_variables, unsafe_op_in_unsafe_fn)]
                    pub unsafe fn #stub(#(#param_decls,)*) {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        {
                            let enc = &mut enc;
                            #(#flatten_stmts)*
                        }
                        // Old dispatch_oneway was removed in 0.2.2; keep the
                        // legacy panic contract in the generated stub instead.
                        if let Err(err) = ::rust2go::ipc::dispatch_oneway_result(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            #method_name_str,
                            &enc.take(),
                        ) {
                            panic!("rust2go-ipc: oneway call to method {} failed: {}", #method_id, err);
                        }
                    }

                    #[allow(non_snake_case, clippy::missing_safety_doc, clippy::useless_transmute, clippy::transmute_ptr_to_ref, unused_variables, unsafe_op_in_unsafe_fn)]
                    pub unsafe fn #try_stub(#(#owned_param_decls,)*)
                        -> ::core::result::Result<(), ::rust2go::IpcError>
                    {
                        let mut enc = ::rust2go::ipc::Encoder::new();
                        {
                            let enc = &mut enc;
                            #(#to_ref_for_try)*
                            #(#flatten_for_try)*
                        }
                        ::rust2go::ipc::dispatch_oneway_result(
                            env!("RUST2GO_IPC_SERVER"),
                            #port,
                            #method_id,
                            #method_name_str,
                            &enc.take(),
                        )
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
    // Fixed prefix (header + helpers) lives in a template file so the Go
    // source stays readable and gofmt-able independent of the Rust generator.
    s.push_str(include_str!("templates/gen_prefix.gotpl"));

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
            "func restore{}Ref(d *dec) {}Ref {{\n\td.enter()\n\tvar r {}Ref\n",
            st.name, st.name, st.name
        ));
        for f in &st.fields {
            s.push_str(&format!(
                "\tr.{} = {}\n",
                f.name,
                go_restore_expr(&f.ty, "d", &widths)
            ));
        }
        s.push_str("\tif d.err { panic(\"rust2go-ipc: invalid payload\") }\n\td.leave()\n\treturn r\n}\n\n");

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
    s.push_str("\td := &dec{b: payload}\n");
    // With an empty schema (no methods), zero-parameter methods, or methods
    // whose params need no decoder, `d` is never read and Go fails to compile
    // with the misleading "d declared and not used". A blank assignment keeps
    // it referenced in every case without changing any generated decoder call.
    s.push_str("\t_ = d\n\tswitch methodID {\n");
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
    include_str!("templates/ipc_main.gotpl").to_string()
}

#[cfg(test)]
#[path = "builder_tests.rs"]
mod tests;
