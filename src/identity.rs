use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::{EyesError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    root: PathBuf,
    root_string: String,
    hash: String,
}

impl ProjectIdentity {
    pub fn resolve(project: Option<&Path>) -> Result<Self> {
        let base = match project {
            Some(path) => path.to_path_buf(),
            None => env::current_dir()?,
        };
        let canonical_base = base.canonicalize()?;
        let root = find_git_toplevel(&canonical_base)?
            .or_else(|| find_eyes_ancestor(&canonical_base))
            .unwrap_or(canonical_base);
        Self::from_root(root)
    }

    pub fn from_root(root: PathBuf) -> Result<Self> {
        let canonical = root.canonicalize()?;
        let root_string = canonical
            .to_str()
            .ok_or_else(|| EyesError::NonUtf8Path(canonical.clone()))?
            .to_owned();
        let mut hasher = Sha256::new();
        hasher.update(root_string.as_bytes());
        let hash = hex::encode(hasher.finalize());
        Ok(Self {
            root: canonical,
            root_string,
            hash,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn root_string(&self) -> &str {
        &self.root_string
    }

    pub fn hash(&self) -> &str {
        &self.hash
    }
}

fn find_git_toplevel(base: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["rev-parse", "--show-toplevel"])
        .output();

    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| EyesError::Protocol("git returned a non-UTF-8 project path".to_owned()))?;
    let path = stdout.trim();
    if path.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(path).canonicalize()?))
}

fn find_eyes_ancestor(base: &Path) -> Option<PathBuf> {
    for ancestor in base.ancestors() {
        if ancestor.join(".eyes").is_dir() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn uses_eyes_ancestor_when_git_is_absent() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("a/b");
        fs::create_dir_all(project.join(".eyes")).unwrap();
        fs::create_dir_all(&nested).unwrap();

        let identity = ProjectIdentity::resolve(Some(&nested)).unwrap();

        assert_eq!(identity.root(), project.canonicalize().unwrap());
        assert_eq!(identity.hash().len(), 64);
    }

    #[test]
    fn from_root_hashes_the_canonical_path() {
        let temp = TempDir::new().unwrap();
        let identity = ProjectIdentity::from_root(temp.path().to_path_buf()).unwrap();

        assert_eq!(identity.root(), temp.path().canonicalize().unwrap());
        assert_eq!(identity.hash().len(), 64);
    }
}
