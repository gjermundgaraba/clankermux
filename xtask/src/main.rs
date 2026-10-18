use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bindings") => bindings(args.iter().any(|a| a == "--check")),
        _ => {
            eprintln!("usage: cargo xtask bindings [--check]");
            ExitCode::from(2)
        }
    }
}

fn bindings(check: bool) -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let include = root.join("vendor/libghostty-vt/include");
    let generated = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg("-DGHOSTTY_STATIC")
        .allowlist_type("Ghostty.*")
        .allowlist_function("ghostty_.*")
        .allowlist_var("GHOSTTY_.*")
        .derive_default(true)
        .prepend_enum_name(false)
        .generate()
        .expect("bindgen")
        .to_string();
    let path = root.join("crates/ghostty-vt-sys/src/bindings.rs");
    if check {
        if std::fs::read_to_string(&path).ok().as_deref() == Some(&generated) {
            return ExitCode::SUCCESS;
        }
        eprintln!("{} is stale; run `cargo xtask bindings`", path.display());
        return ExitCode::FAILURE;
    }
    std::fs::write(&path, generated).expect("write bindings");
    ExitCode::SUCCESS
}
