fn main() {
    println!("cargo:rustc-env=RUSTC_VERSION={}", rustc_version_str());
}

fn rustc_version_str() -> String {
    let output = std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .expect("failed to execute rustc");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}
