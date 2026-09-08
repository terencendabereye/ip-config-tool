use embed_manifest::{embed_manifest, manifest::ExecutionLevel, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(
            new_manifest("ndabereye.IpConfigTool")
                .requested_execution_level(ExecutionLevel::RequireAdministrator),
        )
        .expect("unable to embed application manifest (requireAdministrator)");

        // Exe file icon (shown in Explorer/taskbar for the file itself).
        // The running window's icon is set separately at runtime in gui.rs
        // via Icon::from_bin on the same file, since nwg needs it explicit.
        winres::WindowsResource::new()
            .set_icon("assets/app.ico")
            .compile()
            .expect("unable to embed application icon");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/app.ico");
}
