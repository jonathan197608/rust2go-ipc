use rust2go::Builder;

fn main() {
    Builder::new().with_go_src("../go_app").build();
}
