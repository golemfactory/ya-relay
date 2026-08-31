use prost_build::Config;
use std::env;

fn main() {
    println!("cargo:rerun-if-env-changed=BUILD_SHOW_GENPATH");
    if env::var("BUILD_SHOW_GENPATH").is_ok() {
        eprintln!("Generating code into {}", env::var("OUT_DIR").unwrap());
    }
    println!("cargo:rerun-if-changed=protobuf/ya_relay.proto");
    Config::new()
        // Keep the generated protobuf API stable instead of boxing Request::Session.
        .enum_attribute(
            ".ya_relay_proto.Request.kind",
            "#[allow(clippy::large_enum_variant)]",
        )
        .protoc_arg("--experimental_allow_proto3_optional")
        //.bytes(["session_id"])
        .compile_protos(&["protobuf/ya_relay.proto"], &["protobuf/"])
        .unwrap();
}
