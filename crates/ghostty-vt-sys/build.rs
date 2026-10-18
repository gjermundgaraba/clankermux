use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendored = manifest.join("../../vendor/libghostty-vt");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed={}", vendored.display());
    println!("cargo:rerun-if-env-changed=ZIG");

    let target = env::var("TARGET").unwrap();
    let zig_target = match target.as_str() {
        "aarch64-apple-darwin" => "aarch64-macos",
        "x86_64-apple-darwin" => "x86_64-macos",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        other => panic!("no zig target mapping for {other}"),
    };
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
        .arg(format!("-Dtarget={zig_target}"))
        .arg(format!("-Dversion-string={}", version.trim()))
        .arg("--prefix")
        .arg(out.join("zig-out"))
        .arg("--cache-dir")
        .arg(out.join("zig-cache"))
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
    if target.contains("apple") {
        println!("cargo:rustc-link-lib=c++");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
