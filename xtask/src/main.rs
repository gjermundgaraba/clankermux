use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bindings") => bindings(args.iter().any(|a| a == "--check")),
        Some("protocol") => protocol(args.iter().any(|a| a == "--check")),
        _ => {
            eprintln!("usage: cargo xtask bindings|protocol [--check]");
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
        // The same parse on every host: no system headers, and C23 so
        // `GHOSTTY_ENUM_TYPED` is `: int` whether or not this libclang
        // has `c_fixed_enum` (20+); `-std=c23` needs libclang 18+.
        .clang_args([
            "--target=x86_64-unknown-linux-gnu",
            "-ffreestanding",
            "-std=c23",
        ])
        .allowlist_type("Ghostty.*")
        .allowlist_function("ghostty_.*")
        .allowlist_var("GHOSTTY_.*")
        .derive_default(true)
        .prepend_enum_name(false)
        .parse_callbacks(Box::new(ModsBits))
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

/// Types the `GHOSTTY_MODS_*` bits as `GhosttyMods`, which they combine
/// into, rather than `u32`.
#[derive(Debug)]
struct ModsBits;

impl bindgen::callbacks::ParseCallbacks for ModsBits {
    fn int_macro(&self, name: &str, _value: i64) -> Option<bindgen::callbacks::IntKind> {
        name.starts_with("GHOSTTY_MODS_")
            .then_some(bindgen::callbacks::IntKind::Custom {
                name: "GhosttyMods",
                is_signed: false,
            })
    }
}

/// Renders PROTOCOL.md's generated regions and the web package's protocol
/// types from the catalog.
fn protocol(check: bool) -> ExitCode {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    // Neither shows the release.
    let catalog = proto::catalog::catalog("");
    let doc_path = root.join("PROTOCOL.md");
    let doc = std::fs::read_to_string(&doc_path).expect("read PROTOCOL.md");
    let doc = match proto::render::splice(&doc, &catalog) {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("{}: {err}", doc_path.display());
            return ExitCode::FAILURE;
        }
    };
    let types = proto::typescript::module(&catalog);
    let types_path = root.join("web/packages/clankermux/src/proto/messages.ts");
    let mut code = ExitCode::SUCCESS;
    for (path, rendered) in [(doc_path, doc), (types_path, types)] {
        if !check {
            std::fs::write(&path, rendered).expect("write a rendered file");
        } else if std::fs::read_to_string(&path).ok() != Some(rendered) {
            eprintln!("{} is stale; run `cargo xtask protocol`", path.display());
            code = ExitCode::FAILURE;
        }
    }
    code
}
