use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    files: BTreeMap<String, FileSig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSig {
    len: u64,
    modified_ns: u128,
}

impl FileSnapshot {
    pub fn scan(root: &Path) -> Result<Self> {
        let mut files = BTreeMap::new();
        scan_dir(root, root, &mut files)?;
        Ok(Self { files })
    }

    pub fn has_changed(&self, other: &Self) -> bool {
        self != other
    }
}

fn scan_dir(root: &Path, dir: &Path, files: &mut BTreeMap<String, FileSig>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap_or(&path);
        if is_ignored(relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            scan_dir(root, &path, files)?;
        } else if metadata.is_file() {
            let modified_ns = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            files.insert(
                normalize_relative(relative),
                FileSig {
                    len: metadata.len(),
                    modified_ns,
                },
            );
        }
    }
    Ok(())
}

pub fn is_ignored(relative: &Path) -> bool {
    relative.components().any(|component| {
        let text = component.as_os_str().to_string_lossy();
        matches!(
            text.as_ref(),
            ".git" | ".eyes" | "node_modules" | "target" | "dist"
        )
    }) || relative
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.ends_with(".lock")
                || matches!(
                    name,
                    "Cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock"
                )
        })
        .unwrap_or(false)
}

fn normalize_relative(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn detects_create_write_delete_and_rename() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let first = FileSnapshot::scan(root).unwrap();

        fs::write(root.join("a.txt"), "one").unwrap();
        let created = FileSnapshot::scan(root).unwrap();
        assert!(first.has_changed(&created));

        fs::write(root.join("a.txt"), "two").unwrap();
        let written = FileSnapshot::scan(root).unwrap();
        assert!(created.has_changed(&written));

        fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
        let renamed = FileSnapshot::scan(root).unwrap();
        assert!(written.has_changed(&renamed));

        fs::remove_file(root.join("b.txt")).unwrap();
        let deleted = FileSnapshot::scan(root).unwrap();
        assert!(renamed.has_changed(&deleted));
    }

    #[test]
    fn ignores_generated_and_internal_paths() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".eyes/state")).unwrap();
        fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        let first = FileSnapshot::scan(root).unwrap();

        fs::write(root.join(".git/index"), "changed").unwrap();
        fs::write(root.join(".eyes/state/messages.jsonl"), "changed").unwrap();
        fs::write(root.join("node_modules/pkg/index.js"), "changed").unwrap();
        fs::write(root.join("target/debug/app"), "changed").unwrap();
        fs::write(root.join("Cargo.lock"), "changed").unwrap();
        let second = FileSnapshot::scan(root).unwrap();

        assert!(!first.has_changed(&second));
    }
}
