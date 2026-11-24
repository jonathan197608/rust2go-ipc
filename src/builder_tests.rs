use super::*;

const EDGE_SCHEMA: &str = r#"
#[derive(r2g::R2G, Default)]
pub struct Empty {}

#[derive(R2G, Default)]
pub struct Req {
    pub text: String,
    pub flags: Vec<bool>,
    pub scalar: char,
}

#[derive(R2G, Default)]
pub struct Resp {
    pub ok: bool,
}

#[r2g]
pub trait First {
    fn zero() -> Resp;
    fn multi(a: Req, b: i32) -> Resp;
    fn scalar(name: String) -> String;
    #[go_pass_struct]
    fn byval(req: Req);
    fn byref(req: &Req) -> Resp;
}

#[r2g]
pub trait Second {
    fn other(req: Req) -> Vec<u8>;
    async fn async_one(req: Req) -> Resp;
}
"#;

#[test]
fn edge_schema_generates_valid_rust_and_go() {
    let schema = parse_schema(EDGE_SCHEMA);
    // Qualified derive paths (r2g::R2G) must be recognized just like bare R2G.
    assert!(
        schema.structs.iter().any(|s| s.name == "Empty"),
        "qualified #[derive(r2g::R2G)] must be recognized, got structs: {:?}",
        schema.structs.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert_eq!(schema.methods.len(), 7);
    assert_eq!(
        schema.traits,
        vec!["First".to_string(), "Second".to_string()]
    );
    let ids: Vec<u32> = schema.methods.iter().map(|m| m.index).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4, 5, 6]);
    assert!(!schema.methods.iter().any(|m| m.go_ptr && m.name == "byval"));

    let rust = generate_rust_bindings(&schema, 1234).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
    assert!(rust.contains("CFirst_zero"));
    assert!(rust.contains("CSecond_other"));
    assert!(rust.contains("SlotWriter"));
    assert!(rust.contains("__r2g_try_CFirst_zero"));
    assert!(rust.contains("__r2g_try_CFirst_byval"));
    assert!(rust.contains("__r2g_try_CFirst_byref"));
    assert!(rust.contains("dispatch_sync_result"));
    assert!(rust.contains("dispatch_oneway_result"));
    // Legacy stubs must keep their panic contract after the old public
    // dispatch_sync/dispatch_oneway were removed in 0.2.2: they now call the
    // Result variant and panic locally on Err.
    assert!(
        rust.contains("sync call to method"),
        "legacy sync stub lost its panic wrapper"
    );
    assert!(
        rust.contains("oneway call to method"),
        "legacy oneway stub lost its panic wrapper"
    );
    assert!(
        !rust.contains("ipc :: dispatch_sync (") && !rust.contains("ipc :: dispatch_oneway ("),
        "generated code must not call the removed dispatch APIs"
    );

    let go = generate_go_sources(&schema);
    assert!(go.contains("var FirstImpl First"));
    assert!(go.contains("var SecondImpl Second"));
    assert!(go.contains("primList[bool]"));
    assert!(go.contains("func dispatch("));

    if let Some(gofmt) = find_gofmt() {
        let path = std::env::temp_dir().join(format!(
            "rust2go-ipc-edge-{}-{}.go",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, &go).unwrap();
        let status = Command::new(gofmt).arg("-e").arg(&path).output();
        let _ = fs::remove_file(&path);
        let status = status.expect("cannot run gofmt");
        assert!(
            status.status.success(),
            "generated Go failed gofmt parse: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}

const NESTED_SCHEMA: &str = r#"
#[derive(R2G, Default)]
pub struct Inner {
    pub v: i32,
}

#[derive(R2G, Default)]
pub struct Outer {
    pub inner: Inner,
    pub tags: Vec<String>,
}

#[r2g]
pub trait GolangCall {
    fn nested(req: Outer) -> Outer;
}
"#;

#[test]
fn nested_struct_and_short_string_list_generate_correct_go() {
    let schema = parse_schema(NESTED_SCHEMA);
    let go = generate_go_sources(&schema);

    // Direct nested struct field must be converted via newInner(), not a raw
    // InnerRef-to-Inner assignment (which fails to compile in Go).
    assert!(
        go.contains("v.inner = newInner(r.inner)"),
        "nested struct field must call newInner():\n{}",
        go
    );
    // Vec<String> bound must use the 8-byte minimum wire width, not the
    // 16-byte in-memory StringRef size.
    assert!(
        go.contains("minStringWire = 8"),
        "Vec<String> must use 8-byte wire bound:/n{}",
        go
    );
    assert!(
        go.contains("n > (len(d.b)-d.p)/minStringWire"),
        "stringList must use minStringWire bound:/n{}",
        go
    );

    let rust = generate_rust_bindings(&schema, 1234).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
    assert!(
        rust.contains("list_len_min (8u32)")
            || rust.contains("list_len_min (8)")
            || rust.contains("list_len_min(8u32)")
            || rust.contains("list_len_min(8)"),
        "Rust Vec<String> restore must use list_len_min(8):\n{}",
        rust
    );

    // Nested Ref lists in requests must be read unaligned: rust2go packs child
    // Refs into a byte buffer with 1-byte alignment.
    assert!(
        rust.contains("read_unaligned") && rust.contains("base.add(i)")
            || rust.contains("read_unaligned") && rust.contains("base . add (i)"),
        "nested Ref list flatten must read each element unaligned:\n{}",
        rust
    );

    if let Some(gofmt) = find_gofmt() {
        let path = std::env::temp_dir().join(format!(
            "rust2go-ipc-nested-{}-{}.go",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, &go).unwrap();
        let status = Command::new(gofmt).arg("-e").arg(&path).output();
        let _ = fs::remove_file(&path);
        let status = status.expect("cannot run gofmt");
        assert!(
            status.status.success(),
            "generated Go failed gofmt parse: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
}

#[test]
fn go_server_dispatches_concurrently_with_bounded_semaphore() {
    let main = generate_go_main();
    // Each accepted connection's serve loop must dispatch in a goroutine so a
    // slow handler cannot serialise later requests on the same connection.
    assert!(main.contains("go func(id uint64, method uint32, body []byte)"));
    assert!(main.contains("dispatch(method, body)"));
    // Concurrency is bounded and configurable; writes are serialised to keep
    // response frames intact.
    assert!(main.contains("sem <- struct{}{}"), "missing back-pressure");
    assert!(main.contains("RUST2GO_IPC_MAX_CONCURRENCY"));
    assert!(
        main.contains("writeMu.Lock()"),
        "response writes must be serialised"
    );
    assert!(
        main.contains("\"sync\""),
        "sync import required for writeMu"
    );

    // P1 regression (seventh audit): an idle read deadline is a keep-alive
    // tick, NOT a disconnect. Closing an idle connection makes the Rust
    // reactor see EOF and kill+respawn a still-healthy child, losing in-process
    // state (e.g. one-way Init). n == 0 still guards partial-header desync.
    assert!(
        main.contains("n == 0 && errors.Is(err, os.ErrDeadlineExceeded)"),
        "idle timeout must renew the deadline rather than closing the connection"
    );
    assert!(
        main.contains("continue\n\t\t\t}"),
        "idle deadline must continue the read loop"
    );
    assert!(
        main.contains("errors.Is(err, os.ErrDeadlineExceeded)"),
        "idle timeout must use errors.Is(os.ErrDeadlineExceeded), not deprecated net.Error.Timeout"
    );
    assert!(
        !main.contains("inflight"),
        "in-flight counter is obsolete once idle deadlines always renew"
    );
    assert!(
        !main.contains("\"sync/atomic\""),
        "sync/atomic import must be removed with the in-flight counter"
    );
    // Write failure must close the connection so the Rust reactor reconnects;
    // the lock must be released via defer even if writing panics.
    assert!(
        main.contains("if _, err := conn.Write(out); err != nil"),
        "write errors must be detected"
    );
    assert!(
        main.contains("writeMu.Lock()\n\t\t\t\tdefer writeMu.Unlock()"),
        "writeMu must be released via defer"
    );

    // sem acquire must be cancellable: a torn-down connection must not hang
    // forever blocking on a full semaphore and leak a serve goroutine.
    assert!(
        main.contains("case <-done:"),
        "semaphore acquire must be cancellable via done channel"
    );
    // HARDEN: a hung handler holding every slot must not leak the blocked
    // serve goroutine forever after Rust times out and closes its side. The
    // timeout uses one reusable timer (not a per-request time.After) so bursts
    // don't pile up uncollectable timers.
    assert!(
        main.contains("case <-semWait.C:"),
        "semaphore acquire must have a timeout to avoid goroutine leak"
    );
    assert!(
        !main.contains("case <-time.After(60 * time.Second):"),
        "semaphore timeout must not allocate a per-request time.After timer"
    );
    assert!(
        main.contains("close(done)") && main.contains("sync.Once"),
        "connection teardown must close done exactly once"
    );
    // On exit, in-flight handlers must get a bounded chance to release their
    // semaphore slot, but a hung business handler must not block the exit.
    assert!(
        main.contains("var wg sync.WaitGroup") && main.contains("wg.Wait()"),
        "serve must wait for in-flight handlers on exit"
    );
    assert!(
        main.contains("time.After(2 * time.Second)"),
        "in-flight drain must be bounded so a hung handler cannot block exit"
    );
    assert!(
        main.contains("wg.Done()"),
        "each dispatched handler must mark its WaitGroup slot done"
    );
}

#[test]
#[should_panic(expected = "collides with")]
fn struct_name_colliding_with_go_helper_is_rejected() {
    // `String` is a fixed Go helper (newString) and a predeclared identifier.
    let schema = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct String { pub v: i32 }
        #[r2g]
        pub trait GolangCall { fn f(x: String) -> String; }
        "#,
    );
    let _ = generate_go_sources(&schema);
}

#[test]
#[should_panic(expected = "collides with")]
fn trait_name_colliding_with_go_helper_is_rejected() {
    let schema = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct Req { pub v: i32 }
        #[r2g]
        pub trait dispatch { fn f(x: Req) -> Req; }
        "#,
    );
    let _ = generate_go_sources(&schema);
}

#[test]
#[should_panic(expected = "collides with")]
fn struct_name_colliding_with_64bit_assert_is_rejected() {
    let schema = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct _require64bitGoTarget { pub v: i32 }
        #[r2g]
        pub trait GolangCall { fn f(x: _require64bitGoTarget) -> _require64bitGoTarget; }
        "#,
    );
    let _ = generate_go_sources(&schema);
}

#[test]
#[should_panic(expected = "must have named fields")]
fn tuple_struct_is_rejected() {
    let _ = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct Req(i32);
        #[r2g]
        pub trait GolangCall { fn f(x: Req) -> Req; }
        "#,
    );
}

#[test]
#[should_panic(expected = "must have named fields")]
fn unit_struct_is_rejected() {
    let _ = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct Req;
        #[r2g]
        pub trait GolangCall { fn f(x: Req) -> Req; }
        "#,
    );
}

#[test]
fn raw_identifiers_generate_clean_go_names() {
    // Non-keyword raw identifiers (r#foo) are legal Rust but the `r#` prefix
    // must be stripped before generating Go/Rust identifiers. Keyword ones
    // like r#type are rejected by validate_ident as Go keywords.
    let schema = parse_schema(
        r#"
        #[derive(R2G)]
        pub struct r#Payload { pub r#value: i32 }
        #[r2g]
        pub trait GolangCall { fn r#fetch(r#req: r#Payload) -> r#Payload; }
        "#,
    );
    assert_eq!(schema.structs[0].name, "Payload");
    assert_eq!(schema.structs[0].fields[0].name, "value");
    assert_eq!(schema.methods[0].name, "fetch");
    assert_eq!(schema.methods[0].params[0].name, "req");

    let go = generate_go_sources(&schema);
    assert!(
        !go.contains("r#"),
        "generated Go must not contain r# prefix:\n{go}"
    );
    let rust = generate_rust_bindings(&schema, 1234).to_string();
    assert!(
        !rust.contains("r#"),
        "generated Rust identifiers must not contain r# prefix:\n{rust}"
    );
    syn::parse_file(&rust).expect("generated Rust must parse");
}

#[test]
fn extra_ldflags_empty_is_noop() {
    let args = vec![
        OsString::from("-buildmode"),
        OsString::from("pie"),
        OsString::from("."),
    ];
    let out = with_extra_ldflags(args.clone(), "");
    assert_eq!(out, args);
    let out = with_extra_ldflags(args.clone(), "   ");
    assert_eq!(out, args);
}

#[test]
fn extra_ldflags_merges_into_separate_ldflags() {
    // Mirrors the real build.rs: `-ldflags` followed by the flags value.
    let args = vec![
        OsString::from("-ldflags"),
        OsString::from(r#"-extldflags "-static-pie" -s -w"#),
    ];
    let out = with_extra_ldflags(args, "-linkmode external");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], OsString::from("-ldflags"));
    assert_eq!(
        out[1],
        OsString::from(r#"-extldflags "-static-pie" -s -w -linkmode external"#)
    );
}

#[test]
fn extra_ldflags_merges_into_equals_ldflags() {
    let args = vec![OsString::from("-ldflags=-s -w")];
    let out = with_extra_ldflags(args, "-linkmode external");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0], OsString::from("-ldflags=-s -w -linkmode external"));
}

#[test]
fn extra_ldflags_completes_trailing_separate_flag() {
    let args = vec![OsString::from("-ldflags")];
    let out = with_extra_ldflags(args, "-linkmode external");
    assert_eq!(
        out,
        vec![
            OsString::from("-ldflags"),
            OsString::from("-linkmode external")
        ]
    );
}

#[test]
fn extra_ldflags_env_value_wins_on_duplicate_linkmode() {
    // cmd/link takes the last occurrence of a string flag. The env-injected
    // value must come last so it can override a conflicting build.rs value.
    let args = vec![OsString::from("-ldflags=-linkmode internal -s -w")];
    let out = with_extra_ldflags(args, "-linkmode external");
    assert_eq!(
        out[0],
        OsString::from("-ldflags=-linkmode internal -s -w -linkmode external")
    );
}

#[test]
fn extra_ldflags_adds_ldflags_when_absent() {
    let args = vec![
        OsString::from("-buildmode"),
        OsString::from("pie"),
        OsString::from("."),
    ];
    let out = with_extra_ldflags(args.clone(), "-linkmode external");
    assert_eq!(out.len(), args.len() + 2);
    assert_eq!(out[args.len()], OsString::from("-ldflags"));
    assert_eq!(out[args.len() + 1], OsString::from("-linkmode external"));
    // The upstream args (including the trailing package ".") stay in order.
    assert_eq!(&out[..args.len()], args.as_slice());
}

#[test]
#[should_panic(expected = "parameter name `slot` conflicts")]
fn reserved_param_name_slot_is_rejected() {
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn f(slot: R) -> R; }
"#;
    parse_schema(src);
}

#[test]
#[should_panic(expected = "parameter name `enc` conflicts")]
fn reserved_param_name_enc_is_rejected() {
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn f(enc: R) -> R; }
"#;
    parse_schema(src);
}

#[test]
#[should_panic(expected = "duplicate parameter name `x`")]
fn duplicate_parameter_names_are_rejected() {
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn f(x: R, x: bool) -> R; }
"#;
    parse_schema(src);
}

#[test]
#[should_panic(expected = "generated Go symbol `maxRestoreDepth`")]
fn template_max_restore_depth_symbol_is_reserved() {
    // gen_prefix.gotpl declares `const maxRestoreDepth = 64` at top level.
    // A lowercase struct of the same name would redeclare it in generated Go.
    let src = r#"
#[derive(R2G, Default)]
pub struct maxRestoreDepth { pub ok: bool }
#[r2g]
trait T { fn f() -> maxRestoreDepth; }
"#;
    parse_schema(src);
}

#[test]
fn nested_module_r2g_types_are_collected() {
    let src = r#"
mod types {
    #[derive(R2G, Default)]
    pub struct Req { pub id: i32 }
}
mod api {
    #[r2g]
    pub trait T { fn f(r: types::Req) -> types::Req; }
}
"#;
    let schema = parse_schema(src);
    assert_eq!(
        schema.structs.len(),
        1,
        "nested R2G struct must be collected"
    );
    assert_eq!(schema.structs[0].name, "Req");
    assert_eq!(schema.traits, vec!["T".to_string()]);
    assert_eq!(schema.methods.len(), 1);
    assert_eq!(schema.structs[0].module_path, vec!["types".to_string()]);
    let rust = generate_rust_bindings(&schema, 1234).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
    // The nested struct's owned type must be addressed as `super::types::Req`
    // (not `super::Req`), or the generated binding fails to compile.
    assert!(
        rust.contains("super :: types :: Req"),
        "nested module path missing in generated owned type: {rust}"
    );
}

#[test]
#[should_panic(expected = "#[cfg] on struct")]
fn cfg_gated_r2g_struct_is_rejected() {
    let _ = parse_schema(
        r#"
#[cfg(feature = "x")]
#[derive(R2G, Default)]
pub struct Foo { pub v: i32 }
"#,
    );
}

#[test]
#[should_panic(expected = "#[cfg] on method")]
fn cfg_gated_r2g_method_is_rejected() {
    let _ = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct Foo { pub v: i32 }
#[r2g]
pub trait T {
    #[cfg(target_os = "linux")]
    fn f(r: Foo);
}
"#,
    );
}

#[test]
#[should_panic(expected = "#[cfg] on field")]
fn cfg_gated_r2g_field_is_rejected() {
    // A cfg-gated field is silently stripped by rustc before derive expansion
    // but would still be emitted into the Go side by the builder (which does
    // not evaluate cfg). That Ref layout mismatch reads stale bytes, so reject.
    let _ = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct Foo {
    pub keep: i32,
    #[cfg(feature = "x")]
    pub drop: i32,
}
"#,
    );
}

#[test]
#[should_panic(expected = "external module `types` is not supported")]
fn external_schema_module_is_rejected_clearly() {
    // `mod types;` cannot be parsed from the single schema source file. Silently
    // skipping it would drop R2G types inside it; fail at build time instead.
    let _ = parse_schema(
        r#"
mod types;
#[derive(R2G, Default)]
pub struct Foo { pub v: i32 }
"#,
    );
}

#[test]
#[should_panic(expected = "name `gen` is a reserved identifier")]
fn rust_2024_gen_keyword_is_rejected() {
    // `gen` is reserved in edition 2024; generated bare idents would not
    // compile in a 2024 consumer. Fail fast with an actionable message.
    let _ = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct Foo { pub gen: i32 }
"#,
    );
}

#[test]
#[should_panic(expected = "generic struct `Foo` is not wire-encodable")]
fn generic_r2g_struct_is_rejected_clearly() {
    let src = r#"
#[derive(R2G, Default)]
pub struct Foo<T> { pub v: T }
"#;
    parse_schema(src);
}

#[test]
fn default_go_compiler_exposes_no_args_and_module_env() {
    // The default compiler is a marker: no extra args, but GO111MODULE=on so
    // its behaviour matches CustomArgGoCompiler::new().
    let args = DefaultGoCompiler.args();
    assert!(args.is_empty());
    let envs = DefaultGoCompiler.envs();
    assert_eq!(
        envs,
        vec![(OsString::from("GO111MODULE"), OsString::from("on"))]
    );
}

#[test]
fn custom_go_compiler_trait_exposes_its_args_and_envs() {
    let mut comp = CustomArgGoCompiler::new();
    comp.arg("-buildmode=pie").env("CC", "zig cc");
    assert_eq!(comp.args(), vec![OsString::from("-buildmode=pie")]);
    let envs = comp.envs();
    assert!(envs.contains(&(OsString::from("CC"), OsString::from("zig cc"))));
    assert!(envs.contains(&(OsString::from("GO111MODULE"), OsString::from("on"))));
}

#[test]
fn oneway_method_may_use_slot_as_parameter_name() {
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn notify(slot: R); }
"#;
    let schema = parse_schema(src);
    let _ = generate_go_sources(&schema);
    let rust = generate_rust_bindings(&schema, 1234).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
}

#[test]
#[should_panic(expected = "list of empty struct Wrapper")]
fn indirect_empty_struct_in_list_is_rejected() {
    // Wrapper has one field but that field is an empty struct, so its wire
    // width converges to 0. A list of it cannot round-trip.
    let src = r#"
#[derive(R2G, Default)]
pub struct Empty {}
#[derive(R2G, Default)]
pub struct Wrapper { pub inner: Empty }
#[r2g]
trait T { fn f(items: Vec<Wrapper>) -> bool; }
"#;
    parse_schema(src);
}

#[test]
fn single_empty_struct_field_is_allowed() {
    // An empty struct on its own (not as a list element) is fine: it occupies
    // 0 wire bytes but there is exactly one, so it round-trips unambiguously.
    let src = r#"
#[derive(R2G, Default)]
pub struct Empty {}
#[derive(R2G, Default)]
pub struct Wrapper { pub inner: Empty }
#[r2g]
trait T { fn f(w: Wrapper) -> bool; }
"#;
    let schema = parse_schema(src);
    assert_eq!(schema.structs.len(), 2);
    // Must still generate parseable Rust/Go (no panic, valid syntax).
    syn::parse_file(&generate_rust_bindings(&schema, 1).to_string()).unwrap();
}

#[test]
#[should_panic(expected = "parameter name `__r2g_flatten_R` conflicts")]
fn generated_prefix_param_name_is_rejected() {
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn f(__r2g_flatten_R: R) -> bool; }
"#;
    parse_schema(src);
}

#[test]
fn dec_and_handler_param_names_are_allowed() {
    // dec lives in the trampoline's separate scope; handler is declared after
    // flatten consumes the parameters. Neither collides, so both are legal.
    let src = r#"
#[derive(R2G, Default)]
pub struct R { pub ok: bool }
#[r2g]
trait T { fn f(dec: R, handler: R) -> bool; }
"#;
    let schema = parse_schema(src);
    assert_eq!(schema.methods.len(), 1);
    syn::parse_file(&generate_rust_bindings(&schema, 1).to_string()).unwrap();
}

#[test]
fn all_primitive_types_generate_wire_accessors() {
    // Every primitive maps to a distinct Go dec accessor / primList element
    // type. Lock all of them down so a regression in any dispatch arm is caught
    // without needing a full cross-language round trip.
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct AllPrims {
    pub a: i8, pub b: i16, pub c: i32, pub d: i64,
    pub e: u8, pub f: u16, pub g: u32, pub h: u64,
    pub i: f32, pub j: f64, pub k: bool, pub l: char,
    pub m: usize, pub n: isize,
    pub ints: Vec<i32>, pub floats: Vec<f64>, pub usizes: Vec<usize>,
}
#[r2g]
trait T { fn echo(p: AllPrims) -> AllPrims; }
"#,
    );
    let go = generate_go_sources(&schema);
    for needle in [
        "d.i8()",
        "d.i16()",
        "d.i32()",
        "d.i64()",
        "d.u8()",
        "d.u16()",
        "d.u32()",
        "d.u64()",
        "d.f32()",
        "d.f64()",
        "d.boolean()",
        "primList[int32](d)",
        "primList[float64](d)",
        "primList[uint](d)",
    ] {
        assert!(go.contains(needle), "missing Go accessor {needle}");
    }
    // The generic decodePrims/enc.put dispatch tables must cover all widths.
    let main = generate_go_main();
    let _ = main; // prefix lives in gen.go, checked via go string above
    assert!(go.contains("case int8:"), "decodePrims missing int8");
    assert!(go.contains("case uint64:"), "decodePrims missing uint64");
    assert!(go.contains("case float32:"), "decodePrims missing float32");
    assert!(go.contains("case float64:"), "decodePrims missing float64");

    let rust = generate_rust_bindings(&schema, 1).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
}

#[test]
fn restore_helpers_guard_recursion_depth() {
    // Every generated restore helper (Rust and Go) must enter/leave a depth
    // counter so a self-referential schema or a hostile frame cannot recurse
    // past MAX_RESTORE_DEPTH and overflow the stack.
    let schema = parse_schema(NESTED_SCHEMA);
    let rust = generate_rust_bindings(&schema, 1234).to_string();
    assert!(
        (rust.contains("dec.enter()") || rust.contains("dec . enter ()"))
            && (rust.contains("dec.leave()") || rust.contains("dec . leave ()")),
        "generated Rust restore helpers must guard recursion depth:\n{rust}"
    );
    let go = generate_go_sources(&schema);
    assert!(
        go.contains("d.enter()") && go.contains("d.leave()"),
        "generated Go restore helpers must guard recursion depth"
    );
    assert!(
        go.contains("const maxRestoreDepth = 64"),
        "Go decoder must define the depth limit"
    );
}

#[test]
fn option_fields_generate_consistent_wire() {
    // Option<T> is the only type reinterpreted as a 0/1-element list on the
    // wire but restored as Option<T> on the Rust owned side. Four separate
    // builder branches (classify / rust_owned_path / go_type / go_restore_expr)
    // cooperate here; lock them down together so a regression in any one is
    // caught even though no cross-language round-trip fixture exercises it.
    let src = r#"
#[derive(R2G, Default)]
pub struct Inner { pub v: i32 }
#[derive(R2G, Default)]
pub struct Req {
    pub maybe_string: Option<String>,
    pub maybe_struct: Option<Inner>,
    pub maybe_int: Option<i64>,
}
#[r2g]
trait T {
    fn f(r: Req) -> Option<String>;
    fn g(r: Req) -> Option<Inner>;
}
"#;
    let schema = parse_schema(src);
    let rust = generate_rust_bindings(&schema, 1).to_string();
    let compact: String = rust.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains("::std::option::Option<::std::string::String>"),
        "Option<String> owned return slot missing in generated Rust:\n{rust}"
    );
    assert!(
        compact.contains("option::Option<Inner>")
            || compact.contains("option::Option<super::Inner>"),
        "Option<struct> owned return slot missing in generated Rust:\n{rust}"
    );
    syn::parse_file(&rust).expect("generated Rust must parse");

    let go = generate_go_sources(&schema);
    // On the Go side every Option degrades to a slice: []string / []Inner / []int64.
    assert!(
        go.contains("[]string"),
        "Option<String> must map to []string in Go"
    );
    assert!(
        go.contains("[]Inner"),
        "Option<struct> must map to []Inner in Go"
    );
    assert!(
        go.contains("[]int64"),
        "Option<i64> must map to []int64 in Go"
    );
    assert!(
        go.contains("stringList(d)"),
        "Option<String> must use the minimal-width string list decoder"
    );
}

#[test]
fn empty_schema_generated_go_keeps_decoder_referenced() {
    // A schema with no R2G methods used to generate `d := &dec{...}` with no
    // reads, and Go rejects that with "d declared and not used". The generator
    // now emits a blank assignment so empty/zero-arg-only schemas compile.
    let schema = parse_schema("// no R2G items\n");
    assert!(schema.methods.is_empty());
    let go = generate_go_sources(&schema);
    assert!(
        go.contains("\t_ = d\n"),
        "dispatch must keep the decoder referenced when there are no methods"
    );
    assert!(go.contains("func dispatch("));
}

#[test]
fn zero_arg_only_schema_generated_go_keeps_decoder_referenced() {
    // Even with methods present, a method with no parameters never reads `d`.
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct R {}
#[r2g]
pub trait T {
    fn ping() -> R;
}
"#,
    );
    let go = generate_go_sources(&schema);
    assert!(
        go.contains("\t_ = d\n"),
        "dispatch must keep the decoder referenced for zero-arg methods"
    );
}

#[test]
#[should_panic(expected = "include!() is not supported")]
fn include_macro_in_schema_is_rejected() {
    // The builder reads a single file and cannot expand include!(); silently
    // dropping included R2G types would produce undefined symbols at use sites.
    parse_schema("include!(\"types.rs\");\n");
}

#[test]
fn async_write_is_guarded_by_catch_unwind_after_owned_conversion() {
    // The slot writer must be dropped exactly once. Conversion to the owned
    // response happens first (a panic there leaves the slot untouched so the
    // reactor's cleanup is the sole drop); from_ptr + write are then inside a
    // catch_unwind so a panicking waker cannot reach the reactor and trigger a
    // second cleanup drop.
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct R { pub v: i64 }
#[r2g]
pub trait T {
    async fn get() -> R;
}
"#,
    );
    let rust = generate_rust_bindings(&schema, 1).to_string();
    let owned = rust
        .find("__r2g_owned")
        .expect("owned conversion must be emitted");
    let guard = rust
        .find("catch_unwind")
        .expect("async write must be panic-guarded");
    let from_ptr = rust
        .find("from_ptr")
        .expect("slot writer must be reconstructed");
    assert!(
        owned < guard,
        "owned conversion must precede the panic boundary"
    );
    assert!(
        guard < from_ptr,
        "from_ptr(slot) must be inside catch_unwind"
    );
}

#[test]
fn sync_trampoline_has_no_catch_unwind_guard() {
    // Sync handlers write straight into a stack Option<T> and carry no cleanup,
    // so they must not pay for a panic boundary.
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct R { pub v: i64 }
#[r2g]
pub trait T {
    fn get() -> R;
}
"#,
    );
    let rust = generate_rust_bindings(&schema, 1).to_string();
    assert!(
        !rust.contains("catch_unwind"),
        "sync writes have no cleanup, so no panic guard is needed"
    );
}

#[test]
fn try_stub_borrows_go_pass_struct_param_by_reference() {
    // Regression: the Result try-stub declared an owned parameter but called
    // `ToRef::calc_ref(req)` (no &) because it keyed off `go_ptr`, producing
    // E0308 for every #[go_pass_struct] method. go_ptr controls the Go-side
    // pointer star, NOT the Rust-side borrowing of an owned parameter.
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct Req { pub v: i64 }
#[derive(R2G, Default)]
pub struct Resp { pub ok: bool }
#[r2g]
pub trait T {
    #[go_pass_struct]
    fn byval(req: Req) -> Resp;
}
"#,
    );
    let rust = generate_rust_bindings(&schema, 1).to_string();
    assert!(
        rust.contains("calc_ref (& req)"),
        "owned #[go_pass_struct] param must be borrowed for calc_ref"
    );
    assert!(
        !rust.contains("calc_ref (req)"),
        "calc_ref must not receive an owned value (E0308)"
    );
}

#[test]
fn try_stub_keeps_reference_param_and_calc_ref_without_amp() {
    // A reference parameter must keep `&Req` in the try-stub signature (the
    // Result wrapper forwards the trait's original argument), and calc_ref
    // receives `req` directly (upstream ref_marks semantics).
    let schema = parse_schema(
        r#"
#[derive(R2G, Default)]
pub struct Req { pub v: i64 }
#[derive(R2G, Default)]
pub struct Resp { pub ok: bool }
#[r2g]
pub trait T {
    fn byref(req: &Req) -> Resp;
}
"#,
    );
    let rust = generate_rust_bindings(&schema, 1).to_string();
    assert!(
        rust.contains("req : & super :: Req") || rust.contains("req : & Req"),
        "reference parameter must keep & in try-stub signature"
    );
    assert!(
        rust.contains("calc_ref (req)"),
        "reference parameter passes straight to calc_ref"
    );
}

#[test]
fn cfg_gated_parameter_is_rejected() {
    // A #[cfg] on a parameter is not evaluated by syn; emitting it
    // unconditionally causes an argument-count mismatch at call sites.
    let schema = r#"
#[derive(R2G, Default)]
pub struct R { pub v: i64 }
#[r2g]
pub trait T {
    fn get(#[cfg(feature = "x")] a: R, b: i64) -> R;
}
"#;
    let result = std::panic::catch_unwind(|| parse_schema(schema));
    assert!(result.is_err(), "cfg-gated parameter must fail fast");
}
