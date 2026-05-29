use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        let root = canonical_project_root(&base)?;
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

fn canonical_project_root(base: &Path) -> Result<PathBuf> {
    let canonical = base.canonicalize()?;
    if let Some(root) = git_root(&canonical)? {
        return Ok(root);
    }
    if let Some(root) = eyes_ancestor(&canonical) {
        return Ok(root);
    }
    Ok(canonical)
}

fn git_root(base: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(base)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(line) = text.lines().find(|line| !line.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(PathBuf::from(line).canonicalize()?))
}

fn eyes_ancestor(base: &Path) -> Option<PathBuf> {
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
    fn resolve_uses_the_nearest_eyes_ancestor_when_present() {
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
    fn resolve_uses_git_root_before_eyes_ancestor() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("a/b");
        fs::create_dir_all(project.join(".eyes")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        Command::new("git")
            .args(["init", "-q"])
            .arg(&project)
            .output()
            .unwrap();

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
