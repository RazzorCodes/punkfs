fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/api/grpc/proto/punkfs.proto");
    tonic_prost_build::configure()
        .out_dir("src/api/grpc/gen")
        .compile_protos(&["src/api/grpc/proto/punkfs.proto"], &["src/api/grpc/proto"])
        .unwrap();
    Ok(())
}
