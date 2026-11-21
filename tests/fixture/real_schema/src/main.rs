mod golang;

use golang::{GolangCall, GolangCallImpl};

fn main() {
    let resp = GolangCallImpl::newID(golang::EmptyRequest {});
    assert_eq!(resp.code, "ID-123");
    println!("sync newID = {}", resp.code);

    let _ = std::mem::size_of::<golang::binding::CommonResponseRef>();
}
