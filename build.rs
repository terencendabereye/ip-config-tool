use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(
            new_manifest("ndabereye.IpConfigTool")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("unable to embed application manifest (requireAdministrator)");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
