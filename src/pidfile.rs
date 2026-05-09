use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::unix::set_private_file_permissions;
use crate::{EyesError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PidInfo {
    pub pid: u32,
    pub project_root: String,
    pub project_hash: String,
    pub started_at_ms: u64,
}

#[derive(Debug)]
pub struct PidFileGuard {
    file: File,
    path: PathBuf,
    cleaned: bool,
}

impl PidFileGuard {
    pub fn acquire(path: &Path, info: &PidInfo) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(path)?;
        set_private_file_permissions(path)?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == ErrorKind::WouldBlock {
                EyesError::AlreadyRunning
            } else {
                EyesError::Io(error)
            }
        })?;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        serde_json::to_writer_pretty(&mut file, info)?;
        file.write_all(b"\n")?;
        file.sync_all()?;

        Ok(Self {
            file,
            path: path.to_path_buf(),
            cleaned: false,
        })
    }

    pub fn cleanup(&mut self) -> Result<()> {
        if self.cleaned {
            return Ok(());
        }
        let _ = self.file.unlock();
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}
