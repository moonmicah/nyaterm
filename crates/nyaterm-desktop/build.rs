use std::hash::{Hash, Hasher};

fn main() {
    // `rust_i18n::i18n!` reads `locales/` while the proc macro expands, so cargo
    // tracks none of those paths the way `include_str!` used to. Without this,
    // editing a translation leaves the previous catalog compiled into the binary
    // until something else forces `lib.rs` to rebuild.
    //
    // A directory name alone is not enough: cargo only notices the directory
    // entry's own timestamp, which changes when files are added or removed but
    // not when an existing catalog is edited. List every file so a translation
    // fix re-expands the macro instead of silently keeping the old strings.
    //
    // Listing the files still relies on cargo recompiling the crate, and the
    // proc macro's expansion is cached by content hashes: a catalog edit alone
    // produces identical crate sources, so incremental builds happily reuse the
    // old expansion. Folding a hash of the catalogs into `rustc-env` changes the
    // compilation inputs and therefore the fingerprint.
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
