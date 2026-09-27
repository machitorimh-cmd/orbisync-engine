use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let repository_root = PathBuf::from(manifest_dir).join("..").join("..");
    let proto_root = repository_root.join("proto");
    let realtime = proto_root
        .join("orbisync")
        .join("v1")
        .join("realtime.proto");

    println!("cargo:rerun-if-changed={}", realtime.display());

    let descriptors = protox::compile([&realtime], [&proto_root])?;
    let mut config = prost_build::Config::new();
    config.compile_fds(descriptors)?;
    Ok(())
}
