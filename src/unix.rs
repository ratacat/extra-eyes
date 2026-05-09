use std::fs;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::Path;

use crate::Result;

pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() as u32 }
}

pub fn set_private_dir_permissions(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

pub fn set_private_file_permissions(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o600);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

pub fn is_socket(path: &Path) -> Result<bool> {
    Ok(fs::symlink_metadata(path)?.file_type().is_socket())
}
