use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io,
    path::Path,
};

fn hash_assets(path: &Path, hash: &mut DefaultHasher) -> io::Result<()> {
    if path.is_dir() {
        let mut entries = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            hash_assets(&entry.path(), hash)?;
        }
    } else {
        path.hash(hash);
        fs::read(path)?.hash(hash);
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let mut hash = DefaultHasher::new();
    // The served stylesheet also includes the generated cartridge bitmaps.
    for path in [
        "assets",
        "src/cartridges.rs",
        "../ox-desk-core/src/cartridge.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
        hash_assets(Path::new(path), &mut hash)?;
    }
    println!(
        "cargo:rustc-env=OX_PHONE_SHELL_REVISION={:016x}",
        hash.finish()
    );
    Ok(())
}
