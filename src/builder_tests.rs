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
    assert_eq!(schema.methods.len(), 6);
    assert_eq!(
        schema.traits,
        vec!["First".to_string(), "Second".to_string()]
    );
    let ids: Vec<u32> = schema.methods.iter().map(|m| m.index).collect();
    assert_eq!(ids, vec![0, 1, 2, 3, 4, 5]);
    assert!(!schema.methods.iter().any(|m| m.go_ptr && m.name == "byval"));

    let rust = generate_rust_bindings(&schema, 1234).to_string();
    syn::parse_file(&rust).expect("generated Rust must parse");
    assert!(rust.contains("CFirst_zero"));
    assert!(rust.contains("CSecond_other"));
    assert!(rust.contains("SlotWriter"));

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

    // P1 regression: with concurrent dispatch the read loop is idle while a
    // handler runs, so a 5-minute read deadline must not close the connection
    // mid-handler. It must be renewed only while requests are in flight.
    assert!(
        main.contains("inflight.Load() > 0"),
        "idle timeout must check in-flight handlers before closing"
    );
    assert!(
        main.contains("errors.Is(err, os.ErrDeadlineExceeded)"),
        "idle timeout must use errors.Is(os.ErrDeadlineExceeded), not deprecated net.Error.Timeout"
    );
    assert!(
        main.contains("n == 0 && inflight.Load() > 0"),
        "partial header (n > 0) must still close even with handlers in flight"
    );
    assert!(
        main.contains("\"sync/atomic\""),
        "sync/atomic import required for in-flight counter"
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
