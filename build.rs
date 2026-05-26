use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let mut files = vec![
        manifest_dir.join("Cargo.toml"),
        manifest_dir.join("Cargo.lock"),
        manifest_dir.join("build.rs"),
    ];
    collect_rs_files(&manifest_dir.join("src"), &mut files);
    files.sort();

    let mut hasher = Fnv1a64::default();
    for path in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        path.strip_prefix(&manifest_dir)
            .unwrap_or(path)
            .display()
            .to_string()
            .hash(&mut hasher);
        if let Ok(bytes) = fs::read(path) {
            bytes.hash(&mut hasher);
        }
    }

    println!(
        "cargo:rustc-env=EXTRA_EYES_BUILD_ID={:016x}",
        hasher.finish()
    );
}

fn collect_rs_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, files);
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("rs") {
            files.push(path);
        }
    }
}

#[derive(Default)]
struct Fnv1a64(u64);

impl Hasher for Fnv1a64 {
    fn write(&mut self, bytes: &[u8]) {
        if self.0 == 0 {
            self.0 = 0xcbf29ce484222325;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
