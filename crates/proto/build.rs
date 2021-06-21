use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_SHOW_GENPATH");
    if env::var("BUILD_SHOW_GENPATH").is_ok() {
        println!(
            "cargo:warning=Generating code into {}",
            env::var("OUT_DIR").unwrap()
        );
    }
    println!("cargo:rerun-if-changed=protobuf/ya_relay.proto");
    prost_build::compile_protos(&["protobuf/ya_relay.proto"], &["protobuf/"]).unwrap();
}
