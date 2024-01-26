use prost_build::Config;
use std::env;

fn main() {
    if env::var("PROTOC").is_err() {
        let (protoc_bin, _) = protoc_prebuilt::init("24.0").unwrap();
        env::set_var("PROTOC", protoc_bin);
    }

    println!("cargo:rerun-if-env-changed=BUILD_SHOW_GENPATH");
    if env::var("BUILD_SHOW_GENPATH").is_ok() {
        println!(
            "cargo:warning=Generating code into {}",
            env::var("OUT_DIR").unwrap()
        );
    }
    println!("cargo:rerun-if-changed=protobuf/ya_relay.proto");
    Config::new()
        .protoc_arg("--experimental_allow_proto3_optional")
        //.bytes(["session_id"])
        .compile_protos(&["protobuf/ya_relay.proto"], &["protobuf/"])
        .unwrap();
}
