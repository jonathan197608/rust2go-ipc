mod golang;

use golang::{binding, GolangCall, GolangCallImpl, HashRequest, ListRequest, NestedItem};

#[tokio::main]
async fn main() {
    let cases = [
        "hello",
        "",
        "rust2go-ipc",
        "The quick brown fox jumps over the lazy dog",
    ];
    for input in cases {
        let req = HashRequest {
            message: input.to_string(),
        };
        let (resp, _req) = GolangCallImpl::md5(req).await;
        let digest = <md5::Md5 as md5::Digest>::digest(input.as_bytes());
        let expected = format!("{:x}", digest);
        assert_eq!(resp.hash, expected, "md5 mismatch for {input:?}");
        println!("md5({input:?}) = {}", resp.hash);
    }

    let req = ListRequest {
        raw: vec![0, 1, 2, 255],
        names: vec!["alpha".to_string(), String::new(), "世界".to_string()],
        items: vec![
            NestedItem {
                tag: "first".to_string(),
                value: 42,
            },
            NestedItem {
                tag: String::new(),
                value: -7,
            },
        ],
        count: 1234,
        flag: true,
        flags: vec![true, false, true, true, false, false, false, true, true],
    };
    let (resp, _req) = GolangCallImpl::echoList(req).await;
    assert_eq!(resp.raw, vec![0u8, 1, 2, 255]);
    assert_eq!(resp.names, vec!["alpha", "", "世界"]);
    assert_eq!(resp.items.len(), 2);
    assert_eq!(resp.items[0].tag, "first");
    assert_eq!(resp.items[0].value, 42);
    assert_eq!(resp.items[1].tag, "");
    assert_eq!(resp.items[1].value, -7);
    assert_eq!(resp.count, 1234);
    assert!(resp.flag);
    assert_eq!(
        resp.flags,
        vec![true, false, true, true, false, false, false, true, true]
    );
    println!(
        "list roundtrip: raw={:?} names={:?} items={} count={} flag={}",
        resp.raw,
        resp.names,
        resp.items.len(),
        resp.count,
        resp.flag
    );

    let mut set = tokio::task::JoinSet::new();
    for i in 0..200 {
        set.spawn(async move {
            let input = format!("case-{i}");
            let req = HashRequest {
                message: input.clone(),
            };
            let (resp, _) = GolangCallImpl::md5(req).await;
            let digest = <md5::Md5 as md5::Digest>::digest(input.as_bytes());
            assert_eq!(resp.hash, format!("{:x}", digest), "case {i}");
        });
    }
    while let Some(res) = set.join_next().await {
        res.expect("concurrent task panicked");
    }
    println!("200 concurrent md5 calls passed");

    // The oneway Shutdown makes the Go process exit; the reactor must detect
    // EOF, respawn it and rebind the writer before this next call.
    GolangCallImpl::Shutdown(golang::EmptyRequest {});
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let req = HashRequest {
        message: "after-restart".to_string(),
    };
    let (resp, _) = GolangCallImpl::md5(req).await;
    let digest = <md5::Md5 as md5::Digest>::digest(b"after-restart");
    assert_eq!(resp.hash, format!("{:x}", digest));
    println!("auto-reconnect passed: {}", resp.hash);

    // binding module is referenced so generated symbols stay linked.
    let _ = std::mem::size_of::<binding::HashResponseRef>();
    println!("all cases passed");
}
