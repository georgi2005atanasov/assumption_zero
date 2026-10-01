fn main() {
    let output =
        std::process::Command::new(std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
            .arg("--version")
            .output()
            .expect("rustc version");
    println!(
        "cargo:rustc-env=RUSTC_VERSION={}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    println!("cargo:rerun-if-changed=build.rs");
}
