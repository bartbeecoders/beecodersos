fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={dir}");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=src/boot.s");

    // OS version and build number, passed in by the Makefile.
    let version = std::env::var("BEECODERS_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let build = std::env::var("BEECODERS_BUILD").unwrap_or_else(|_| "0".into());
    println!("cargo:rustc-env=BEECODERS_VERSION={version}");
    println!("cargo:rustc-env=BEECODERS_BUILD={build}");
    println!("cargo:rerun-if-env-changed=BEECODERS_VERSION");
    println!("cargo:rerun-if-env-changed=BEECODERS_BUILD");
}
