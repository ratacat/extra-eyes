use std::env;
use std::path::{Path, PathBuf};

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
        Self::from_root(base)
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

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolve_uses_the_requested_folder_when_eyes_ancestor_exists() {
        let temp = TempDir::new().unwrap();
        let project = temp.path().join("project");
        let nested = project.join("a/b");
        fs::create_dir_all(project.join(".eyes")).unwrap();
        fs::create_dir_all(&nested).unwrap();

        let identity = ProjectIdentity::resolve(Some(&nested)).unwrap();

        assert_eq!(identity.root(), nested.canonicalize().unwrap());
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
