fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Do not rerun for unrelated tests, reports or working-tree files. Cargo's
    // default without an explicit input watches the entire package directory.
    println!("cargo:rerun-if-changed=build.rs");
    // Only compile etcd protobuf files when the etcd feature is enabled
    #[cfg(feature = "etcd")]
    {
        println!("cargo:rerun-if-changed=proto/etcd");
        println!("cargo:rerun-if-env-changed=PROTOC");
        println!("cargo:rerun-if-env-changed=PROTOC_INCLUDE");
        // Compile proto files using prost-build
        let mut prost_build = prost_build::Config::new();

        prost_build.compile_protos(
            &["proto/etcd/rpc.proto", "proto/etcd/kv.proto"],
            &["proto/etcd"],
        )?;
    }

    Ok(())
}
