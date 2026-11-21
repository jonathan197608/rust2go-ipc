#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(dead_code)]
#![allow(improper_ctypes)]
#![allow(clippy::useless_transmute)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::ptr_offset_with_cast)]

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
pub struct EsRequest {
    pub trace_id: String,
    pub order_id: String,
    pub dsl: String,
}

#[derive(R2G, Default)]
pub struct UIMRequest {
    pub trace_id: String,
    pub iccid: String,
}

#[derive(R2G, Default)]
pub struct TextRequest {
    pub trace_id: String,
    pub rule_id: String,
    pub biz_token: String,
    pub order_code: String,
    pub location: String,
}

#[derive(R2G, Default)]
pub struct AddressRequest {
    pub trace_id: String,
    pub location: String,
}

#[derive(R2G, Default)]
pub struct LabelRequest {
    pub trace_id: String,
    pub address_id: String,
}

#[derive(R2G, Default)]
pub struct ImageRequest {
    pub trace_id: String,
    pub rule_id: String,
    pub biz_token: String,
    pub id_card: String,
    pub started_at: String,
    pub order_code: String,
}

#[derive(R2G, Default)]
pub struct CoverRequest {
    pub trace_id: String,
    pub img1_bucket: String,
    pub img1_object: String,
    pub img2_bucket: String,
    pub img2_object: String,
    pub save_bucket: String,
    pub save_object: String,
}

#[derive(R2G, Default)]
pub struct CloudRequest {
    pub trace_id: String,
    pub bucket: String,
    pub filename: String,
    pub destination: String,
}

#[derive(R2G, Default)]
pub struct QYWXRequest {
    pub trace_id: String,
    pub phone: String,
    pub message: String,
}

#[derive(R2G, Default)]
pub struct Func014Request {
    pub trace_id: String,
    pub in1014001: String,
    pub in1014002: String,
    pub in1014003: String,
    pub in1014011: Vec<String>,
}
#[derive(R2G, Default)]
pub struct Func015Request {
    pub trace_id: String,
    pub in1015001: String,
    pub in1015002: String,
    pub in1015011: Vec<String>,
}

#[derive(R2G, Default)]
pub struct Func019Request {
    pub trace_id: String,
    pub in1019001: String,
    pub in1019002: String,
    pub in1019003: String,
}

#[derive(R2G, Default)]
pub struct Func157Request {
    pub trace_id: String,
    pub query_type: String,
    pub in1157011: String,
    pub in1157012: String,
    pub in1157013: String,
    pub in1157014: String,
    pub in1157015: String,
    pub in1157016: String,
    pub in1157017: String,
}

#[derive(R2G, Default)]
pub struct Func614Request {
    pub trace_id: String,
    pub In1614001: String,
    pub In1614002: String,
    pub In1614003: String,
    pub In1614004: String,
    pub In1614005: String,
    pub In1614006: String,
    pub In1614007: String,
    pub In1614008: String,
    pub In1614009: String,
    pub In1614010: String,
    pub In1614011: String,
    pub In1614041: String,
    pub In1614042: String,
    pub In1614043: String,
    pub In1614044: String,
    pub In1614045: String,
    pub In1614046: String,
    pub In1614047: String,
    pub In1614048: String,
    pub In1614050: String,
    pub In1614061: String,
    pub In1614091: String,
    pub In1614092: String,
    pub In1614093: String,
    pub In1614094: String,
    pub In1614095: String,
    pub In1614101: String,
    pub In1614102: String,
}

#[derive(R2G, Default)]
pub struct SMSRequest {
    pub trace_id: String,
    pub mobile: String,
    pub message: String,
    pub fixed_isp: String,
    pub sms_range: String,
    pub template_id: String,
}

#[derive(R2G, Default)]
pub struct ReceiptRequest {
    pub trace_id: String,
    pub codes: Vec<String>,
}

#[derive(R2G, Default)]
pub struct FunctionRequest {
    pub trace_id: String,
    pub name: String,
    pub req: Vec<u8>,
}

#[derive(R2G, Default)]
pub struct TouchRequest {
    pub trace_id: String,
    pub code: String,
    pub scene: i32,
    pub status: i32,
    pub count: i32,
    pub duration: i32,
}

#[derive(R2G, Default)]
pub struct BillRequest {
    pub trace_id: String,
    pub order_type: String,
    pub order_id: i64,
    pub order_product_name: String,
    pub interface_account: String,
    pub interface_method: String,
    pub influence_scope: String,
    pub err_message: String,
}

#[derive(R2G, Default)]
pub struct CommonResponse {
    pub err_msg: String,
    pub result: Vec<u8>,
    pub ok: bool,
}

#[derive(R2G, Default)]
pub struct HashResponse {
    pub hash: String,
}

#[derive(R2G, Default)]
pub struct ReceiptResponse {
    pub err_msg: String,
    pub files: Vec<String>,
}

#[derive(R2G, Default)]
pub struct AddressResponse {
    pub province: String,
    pub city: String,
    pub district: String,
    pub street: String,
    pub detail: String,
}

#[derive(R2G, Default)]
pub struct AddressLabel {
    pub desc: String,
    pub name: String,
    pub code: String,
    pub value: i32,
}

#[derive(R2G, Default)]
pub struct LabelResponse {
    pub code: String,
    pub message: String,
    pub labels: Vec<AddressLabel>,
}

#[derive(R2G, Default)]
pub struct OrderResponse {
    pub code: String,
}

#[derive(R2G, Default)]
pub struct FuncResult {
    pub id: String,
    pub name: String,
    pub value: String,
}

#[derive(R2G, Default)]
pub struct RowResult {
    pub cols: Vec<FuncResult>,
}

#[derive(R2G, Default)]
pub struct EOPResponse {
    pub result_code: String,
    pub err_msg: String,
    pub grid: Vec<RowResult>,
}

#[derive(R2G, Default)]
pub struct FileRequest {
    pub trace_id: String,
    pub bucket: String,
    pub objectKey: String,
    pub content: String,
}

#[derive(R2G, Default)]
pub struct DeleteFileRequest {
    pub trace_id: String,
    pub bucket: String,
    pub objectKeys: Vec<String>,
}
#[derive(R2G, Default)]
pub struct FileNameAndPath {
    pub file_name: String,
    pub file_path: String,
}

#[derive(R2G, Default)]
pub struct OrderAuditRequest {
    pub trace_id: String,
    pub image_path: Vec<FileNameAndPath>,
    pub audit_info: String,
}

#[r2g]
pub trait GolangCall {
    fn Init(req: EmptyRequest);
    fn Shutdown(req: EmptyRequest);
    fn touchBill(req: TouchRequest);
    fn newBill(req: BillRequest);
    fn newErrorBill(req: BillRequest);
    fn newID(req: EmptyRequest) -> OrderResponse;
    #[drop_safe_ret]
    async fn md5(req: HashRequest) -> HashResponse;
    #[drop_safe_ret]
    async fn esGetOrder(req: EsRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn eop014(req: Func014Request) -> EOPResponse;
    #[drop_safe_ret]
    async fn eop015(req: Func015Request) -> EOPResponse;
    #[drop_safe_ret]
    async fn eop019(req: Func019Request) -> EOPResponse;
    #[drop_safe_ret]
    async fn eop157(req: Func157Request) -> EOPResponse;
    #[drop_safe_ret]
    async fn eop614(req: Func614Request) -> EOPResponse;
    #[drop_safe_ret]
    async fn addressLabel(req: LabelRequest) -> LabelResponse;
    #[drop_safe_ret]
    async fn fetchAddress(req: AddressRequest) -> AddressResponse;
    #[drop_safe_ret]
    async fn function(req: FunctionRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn esDump(req: EsRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn fetchUIM(req: UIMRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn fetchText(req: TextRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn fetchImages(req: ImageRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn newCover(req: CoverRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn saveCloud(req: CloudRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn sendSMS(req: SMSRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn sendQYWX(req: QYWXRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn fetchReceipts(req: ReceiptRequest) -> ReceiptResponse;
    #[drop_safe_ret]
    async fn dailyReportES(req: EsRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn UploadFile(req: FileRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn DownloadFile(req: FileRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn DeleteFile(req: DeleteFileRequest) -> CommonResponse;
    #[drop_safe_ret]
    async fn OrderPreaudit(req: OrderAuditRequest) -> CommonResponse;
}
//本文件修改需要执行文件graph/gen.sh
//如果没有装工具，需要先运行：cargo install rust2go-cli --force
