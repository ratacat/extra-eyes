use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::identity::ProjectIdentity;
use crate::unix::{effective_uid, set_private_dir_permissions};
use crate::Result;

#[derive(Debug, Clone)]
pub struct ProjectPaths {
    identity: ProjectIdentity,
    runtime_dir: PathBuf,
    socket_path: PathBuf,
    pid_path: PathBuf,
    eyes_dir: PathBuf,
    inbox_path: PathBuf,
    state_dir: PathBuf,
    messages_path: PathBuf,
    cursors_path: PathBuf,
    sessions_path: PathBuf,
    conversation_path: PathBuf,
    watcher_status_path: PathBuf,
}

impl ProjectPaths {
    pub fn resolve(project: Option<&Path>) -> Result<Self> {
        let identity = ProjectIdentity::resolve(project)?;
        Self::from_identity(identity)
    }

    pub fn from_identity(identity: ProjectIdentity) -> Result<Self> {
        let runtime_base = runtime_base_dir();
        Self::from_identity_with_runtime_base(identity, runtime_base)
    }

    pub fn from_identity_with_runtime_base(
        identity: ProjectIdentity,
        runtime_base: PathBuf,
    ) -> Result<Self> {
        let runtime_dir = runtime_base.join("daemon");
        let socket_path = runtime_dir.join("eyesd.sock");
        let pid_path = runtime_dir.join("eyesd.pid.json");
        let eyes_dir = identity.root().join(".eyes");
        let inbox_path = eyes_dir.join("inbox.md");
        let state_dir = eyes_dir.join("state");
        let messages_path = state_dir.join("messages.jsonl");
        let cursors_path = state_dir.join("cursors.jsonl");
        let sessions_path = state_dir.join("sessions.jsonl");
        let conversation_path = state_dir.join("conversation.jsonl");
        let watcher_status_path = state_dir.join("watcher-status.jsonl");

        Ok(Self {
            identity,
            runtime_dir,
            socket_path,
            pid_path,
            eyes_dir,
            inbox_path,
            state_dir,
            messages_path,
            cursors_path,
            sessions_path,
            conversation_path,
            watcher_status_path,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.runtime_dir)?;
        set_private_dir_permissions(&self.runtime_dir)?;
        fs::create_dir_all(&self.state_dir)?;
        set_private_dir_permissions(&self.eyes_dir)?;
        set_private_dir_permissions(&self.state_dir)?;
        Ok(())
    }

    pub fn identity(&self) -> &ProjectIdentity {
        &self.identity
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn pid_path(&self) -> &Path {
        &self.pid_path
    }

    pub fn eyes_dir(&self) -> &Path {
        &self.eyes_dir
    }

    pub fn inbox_path(&self) -> &Path {
        &self.inbox_path
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn messages_path(&self) -> &Path {
        &self.messages_path
    }

    pub fn cursors_path(&self) -> &Path {
        &self.cursors_path
    }

    pub fn sessions_path(&self) -> &Path {
        &self.sessions_path
    }

    pub fn conversation_path(&self) -> &Path {
        &self.conversation_path
    }

    pub fn watcher_status_path(&self) -> &Path {
        &self.watcher_status_path
    }
}

pub fn runtime_base_dir() -> PathBuf {
    if let Some(path) = env::var_os("EXTRA_EYES_RUNTIME_DIR") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(path).join("extra-eyes");
    }
    PathBuf::from(format!("/tmp/extra-eyes-{}", effective_uid()))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn builds_single_daemon_runtime_and_project_state_paths() {
        let project = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        let identity = ProjectIdentity::from_root(project.path().to_path_buf()).unwrap();
        let paths =
            ProjectPaths::from_identity_with_runtime_base(identity.clone(), runtime.path().into())
                .unwrap();

        assert_eq!(paths.runtime_dir(), runtime.path().join("daemon"));
        assert_eq!(paths.socket_path(), paths.runtime_dir().join("eyesd.sock"));
        assert_eq!(
            paths.state_dir(),
            project.path().canonicalize().unwrap().join(".eyes/state")
        );
    }
}
