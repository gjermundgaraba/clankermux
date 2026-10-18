fn main() {
    let version = std::fs::read_to_string("../../vendor/libghostty-vt/VERSION").unwrap();
    println!("cargo:rerun-if-changed=../../vendor/libghostty-vt/VERSION");
    println!("cargo:rustc-env=GHOSTTY_VT_VERSION={}", version.trim());
}
