// Takes the firmware version from ../../Cargo.toml so the sample Info page
// always shows the real one.
fn main() {
    println!("cargo:rerun-if-changed=../../Cargo.toml");
    let toml = std::fs::read_to_string("../../Cargo.toml").unwrap_or_default();
    let version = toml.lines()
        .find_map(|l| l.strip_prefix("version = "))
        .map(|v| v.trim().trim_matches('"').to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=FW_VERSION={}", version);
}
