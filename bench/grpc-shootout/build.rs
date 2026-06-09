fn main() {
    // Generates both the prost message types and the tonic client/server stubs
    // into OUT_DIR; `src/lib.rs` includes them via `tonic::include_proto!`.
    tonic_build::compile_protos("proto/shootout.proto")
        .expect("failed to compile proto -- is `protoc` on PATH? (brew install protobuf)");
    println!("cargo:rerun-if-changed=proto/shootout.proto");
}
