fn main() -> anyhow::Result<()> {
    // Build date/time shown on the Info page and the Web UI. Re-run whenever
    // the sources change so it tracks the actual build, not the first one.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let stamp = std::process::Command::new("date")
        .arg("+%Y-%m-%d %H:%M:%S %Z")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", stamp);

    // Rebuild when C component sources change
    println!("cargo:rerun-if-changed=components/ssh_bridge/ssh_bridge.c");
    println!("cargo:rerun-if-changed=components/ssh_bridge/include/ssh_bridge.h");
    println!("cargo:rerun-if-changed=components/ssh_bridge/CMakeLists.txt");

    embuild::build::CfgArgs::output_propagated("ESP_IDF")?;
    embuild::build::LinkArgs::output_propagated("ESP_IDF")
}
