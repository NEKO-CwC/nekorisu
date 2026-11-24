// Compiles proto/ with protox (pure Rust), so building needs no protoc.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    // The platform this binary is for, so the updater picks the right archive.
    println!("cargo:rustc-env=NEKORISU_TARGET={}", std::env::var("TARGET")?);
    // Root public keys built into the binary.
    // NEKORISU_ROOT_PUB points elsewhere for tests; releases use keys/root.pub.
    println!("cargo:rerun-if-env-changed=NEKORISU_ROOT_PUB");
    println!("cargo:rerun-if-changed=keys/root.pub");
    let roots = match std::env::var("NEKORISU_ROOT_PUB") {
        Ok(path) => std::path::PathBuf::from(path),
        Err(_) => std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("keys/root.pub"),
    };
    println!("cargo:rerun-if-changed={}", roots.display());
    println!("cargo:rustc-env=NEKORISU_ROOT_PUB_FILE={}", roots.display());
    let descriptors = protox::compile(["nekorisu/v1/core.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)?;
    Ok(())
}
