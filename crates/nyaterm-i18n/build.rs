use std::hash::{Hash, Hasher};

fn main() {
    // `rust_i18n::i18n!("locales")` expands while this crate compiles, and cargo
    // does not track the catalog files the way `include_str!` does. Listing every
    // file makes edits visible; folding a content hash into `rustc-env` changes
    // the compilation fingerprint so incremental builds cannot reuse a stale
    // macro expansion after a translation-only change.
    println!("cargo:rerun-if-changed=locales");
    let locales = std::path::Path::new("locales");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(locales) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                println!("cargo:rerun-if-changed={}", path.display());
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        path.display().to_string().hash(&mut hasher);
        std::fs::read(&path)
            .map(|bytes| bytes.hash(&mut hasher))
            .ok();
    }
    println!(
        "cargo:rustc-env=NYATERM_LOCALES_HASH={:016x}",
        hasher.finish()
    );
}
