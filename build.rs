fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=src/api/punkfs.proto");
    tonic_prost_build::configure()
        .out_dir("src/client/grpc/gen")
        .compile_protos(&["src/api/punkfs.proto"], &["proto"])
        .unwrap();
    Ok(())
}
