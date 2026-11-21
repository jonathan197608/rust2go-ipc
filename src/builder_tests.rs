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
