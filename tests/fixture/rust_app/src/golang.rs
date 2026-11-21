#![allow(non_snake_case, non_camel_case_types, dead_code, clippy::all)]

use rust2go::{r2g, R2G};

pub mod binding {
    rust2go::r2g_include_binding!();
}

#[derive(R2G, Default)]
pub struct EmptyRequest {}

#[derive(R2G, Default)]
pub struct HashRequest {
    pub message: String,
}

#[derive(R2G, Default)]
pub struct HashResponse {
    pub hash: String,
}

#[derive(R2G, Default)]
pub struct NestedItem {
    pub tag: String,
    pub value: i32,
}

#[derive(R2G, Default)]
pub struct ListRequest {
    pub raw: Vec<u8>,
    pub names: Vec<String>,
    pub items: Vec<NestedItem>,
    pub count: i32,
    pub flag: bool,
    pub flags: Vec<bool>,
}

#[derive(R2G, Default)]
pub struct ListResponse {
    pub raw: Vec<u8>,
    pub names: Vec<String>,
    pub items: Vec<NestedItem>,
    pub count: i32,
    pub flag: bool,
    pub flags: Vec<bool>,
}

#[r2g]
pub trait GolangCall {
    #[drop_safe_ret]
    async fn md5(req: HashRequest) -> HashResponse;
    #[drop_safe_ret]
    async fn echoList(req: ListRequest) -> ListResponse;
    fn Shutdown(req: EmptyRequest);
}
