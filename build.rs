//! Embeds everything under assets/ into the exe. Generates a table of
//! ("/assets/<path>", bytes) that src/web.rs includes, so adding a map is just
//! dropping its files into assets/ and rebuilding.

use std::{env, fs, path::Path, path::PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    println!("cargo:rerun-if-changed=assets");

    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();

    let mut table = String::from("&[\n");
    for f in &files {
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        table += &format!("    (\"/assets/{rel}\", include_bytes!({:?}) as &[u8]),\n", f.to_string_lossy());
    }
    table += "]\n";
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("assets.rs"), table).unwrap();
}
