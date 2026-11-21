fn main() {
    rust2go::Builder::new()
        .with_go_src("../real_go_app")
        .build();
}
