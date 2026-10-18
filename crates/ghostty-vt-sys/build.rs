use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendored = manifest.join("../../vendor/libghostty-vt");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendored.display());
    println!("cargo:rerun-if-env-changed=ZIG");

    let version = fs::read_to_string(vendored.join("VERSION")).unwrap();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let zig = env::var("ZIG").unwrap_or_else(|_| "zig".into());
    let status = Command::new(&zig)
        .args([
            "build",
            "-Demit-lib-vt",
            "-Demit-xcframework=false",
            "-Doptimize=ReleaseFast",
        ])
        .arg(format!(
            "-Dtarget={}",
            zig_target(&env::var("TARGET").unwrap())
        ))
        .arg(format!("-Dversion-string={}", version.trim()))
        // `ghostty_build_info` reports the library's own version.
        .arg(format!("-Dlib-version-string={}", version.trim()))
        .arg("--prefix")
        .arg(out.join("zig-out"))
        // Outside the vendored tree, which rerun-if-changed watches, and
        // shared across build-script hashes so profiles reuse artifacts.
        .arg("--cache-dir")
        .arg(zig_cache(&out))
        .current_dir(&vendored)
        .status()
        .unwrap_or_else(|err| panic!("cannot run {zig} (set ZIG to a zig 0.16.0 binary): {err}"));
    assert!(
        status.success(),
        "zig build of libghostty-vt failed: {status}"
    );

    // Link from a directory holding only the static archive so the Apple
    // linker does not pick the sibling dylib.
    let lib = out.join("lib");
    fs::create_dir_all(&lib).unwrap();
    fs::copy(
        out.join("zig-out/lib/libghostty-vt.a"),
        lib.join("libghostty-vt.a"),
    )
    .unwrap();
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");
}

/// `aarch64-apple-darwin` -> `aarch64-macos`, `x86_64-unknown-linux-gnu` ->
/// `x86_64-linux-gnu`: zig drops the vendor and calls Darwin macOS.
fn zig_target(rust_target: &str) -> String {
    let mut parts = rust_target.splitn(4, '-');
    let arch = parts.next().unwrap();
    let _vendor = parts.next();
    let os = match parts.next() {
        Some("darwin") => "macos",
        Some(os) => os,
        None => panic!("unexpected target triple {rust_target}"),
    };
    match parts.next() {
        Some(abi) => format!("{arch}-{os}-{abi}"),
        None => format!("{arch}-{os}"),
    }
}

/// `<target dir>/zig-cache`, found from `OUT_DIR`
/// (`<target dir>/<profile>/build/<crate>-<hash>/out`).
fn zig_cache(out: &Path) -> PathBuf {
    out.ancestors()
        .find(|p| p.file_name().is_some_and(|n| n == "build"))
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map_or_else(|| out.join("zig-cache"), |target| target.join("zig-cache"))
}
