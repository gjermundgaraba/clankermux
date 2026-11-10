use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let check = args.iter().any(|a| a == "--check");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let (task, rendered) = match args.first().map(String::as_str) {
        Some(task @ "bindings") => (task, Ok(vec![bindings(&root)])),
        Some(task @ "protocol") => (task, protocol(&root)),
        _ => {
            eprintln!("usage: cargo xtask bindings|protocol [--check]");
            return ExitCode::from(2);
        }
    };
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    let mut code = ExitCode::SUCCESS;
    for (path, text) in rendered {
        if !check {
            std::fs::write(&path, text).expect("write a rendered file");
        } else if std::fs::read_to_string(&path).ok() != Some(text) {
            eprintln!("{} is stale; run `cargo xtask {task}`", path.display());
            code = ExitCode::FAILURE;
        }
    }
    code
}

fn bindings(root: &Path) -> (PathBuf, String) {
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
    (
        root.join("crates/ghostty-vt-sys/src/bindings.rs"),
        generated,
    )
}

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

fn protocol(root: &Path) -> Result<Vec<(PathBuf, String)>, String> {
    let catalog = proto::catalog::catalog(None);
    let doc_path = root.join("PROTOCOL.md");
    let doc = std::fs::read_to_string(&doc_path).expect("read PROTOCOL.md");
    let doc = proto::render::splice(&doc, &catalog)
        .map_err(|err| format!("{}: {err}", doc_path.display()))?;
    let types = proto::typescript::module(&catalog);
    let types_path = root.join("web/packages/clankermux/src/proto/messages.ts");
    Ok(vec![(doc_path, doc), (types_path, types)])
}
