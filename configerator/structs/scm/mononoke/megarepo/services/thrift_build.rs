// @generated by autocargo

use std::env;
use std::fs;
use std::path::Path;
use thrift_compiler::Config;
use thrift_compiler::GenContext;
const CRATEMAP: &str = "\
configerator/structs/scm/mononoke/megarepo/megarepo_configs.thrift crate //configerator/structs/scm/mononoke/megarepo:megarepo_configs-rust
thrift/annotation/rust.thrift rust //thrift/annotation:rust-rust
";
#[rustfmt::skip]
fn main() {
    println!("cargo:rerun-if-changed=thrift_build.rs");
    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR env not provided");
    let cratemap_path = Path::new(&out_dir).join("cratemap");
    fs::write(cratemap_path, CRATEMAP).expect("Failed to write cratemap");
    Config::from_env(GenContext::Services)
        .expect("Failed to instantiate thrift_compiler::Config")
        .base_path("../../../../../..")
        .types_crate("megarepo_configs__types")
        .clients_crate("megarepo_configs__clients")
        .options("serde")
        .run(["../megarepo_configs.thrift"])
        .expect("Failed while running thrift compilation");
}
