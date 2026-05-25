use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::identity::ProjectIdentity;
use crate::{EyesError, Result};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    ClaudeCode,
    Codex,
    Pi,
    Raw,
}

impl Harness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Harness::ClaudeCode => "claude-code",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
            Harness::Raw => "raw",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WatcherProfile {
    pub name: String,
    #[serde(default)]
    pub default: bool,
    pub prompt: String,
    pub harness: Harness,
    pub model: String,
    #[serde(default)]
    pub settings: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ResolvedProfile {
    pub source: ProfileSource,
    pub path: Option<PathBuf>,
    pub profile: WatcherProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Project,
    User,
    Bundled,
}

#[derive(Debug, Clone, PartialEq)]
struct LoadedProfile {
    source: ProfileSource,
    path: Option<PathBuf>,
    profile: WatcherProfile,
}

pub fn resolve_profile(project: Option<&Path>, requested: Option<&str>) -> Result<ResolvedProfile> {
    let identity = ProjectIdentity::resolve(project)?;
    let loaded = load_profiles(identity.root())?;
    let selected = match requested {
        Some(name) => select_by_name(&loaded, name)?,
        None => select_default(&loaded)?,
    };
    Ok(ResolvedProfile {
        source: selected.source,
        path: selected.path,
        profile: selected.profile,
    })
}

pub fn integer_setting(profile: &WatcherProfile, key: &str) -> Result<Option<u64>> {
    let Some(value) = profile.settings.get(key) else {
        return Ok(None);
    };
    let Some(integer) = value.as_integer() else {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.{key} must be an integer",
            profile.name
        )));
    };
    if integer < 0 {
        return Err(EyesError::Config(format!(
            "profile '{}' settings.{key} must be non-negative",
            profile.name
        )));
    }
    Ok(Some(integer as u64))
}

pub fn bool_setting(profile: &WatcherProfile, key: &str) -> Result<Option<bool>> {
    let Some(value) = profile.settings.get(key) else {
        return Ok(None);
    };
    value.as_bool().map(Some).ok_or_else(|| {
        EyesError::Config(format!(
            "profile '{}' settings.{key} must be a boolean",
            profile.name
        ))
    })
}

fn load_profiles(project_root: &Path) -> Result<Vec<LoadedProfile>> {
    let mut profiles = Vec::new();
    profiles.extend(load_profile_dir(
        &project_root.join(".eyes/watchers"),
        ProfileSource::Project,
    )?);
    if let Some(home) = extra_eyes_home() {
        profiles.extend(load_profile_dir(
            &home.join("watchers"),
            ProfileSource::User,
        )?);
    }
    profiles.push(LoadedProfile {
        source: ProfileSource::Bundled,
        path: None,
        profile: bundled_general_profile(),
    });
    Ok(profiles)
}

fn load_profile_dir(path: &Path, source: ProfileSource) -> Result<Vec<LoadedProfile>> {
    if !path.is_dir() {
        return Ok(Vec::new());
    }

    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort();

    let mut seen_names = BTreeSet::new();
    let mut profiles = Vec::new();
    for entry in entries {
        if entry.extension().and_then(|extension| extension.to_str()) != Some("toml") {
            continue;
        }
        let text = fs::read_to_string(&entry)?;
        let profile: WatcherProfile = toml::from_str(&text).map_err(|error| {
            EyesError::Config(format!("failed to parse {}: {error}", entry.display()))
        })?;
        validate_profile(&profile, &entry)?;
        if !seen_names.insert(profile.name.clone()) {
            return Err(EyesError::Config(format!(
                "duplicate watcher profile name '{}' in {}",
                profile.name,
                path.display()
            )));
        }
        profiles.push(LoadedProfile {
            source: source.clone(),
            path: Some(entry),
            profile,
        });
    }
    ensure_single_default(&profiles, path)?;
    Ok(profiles)
}

fn validate_profile(profile: &WatcherProfile, path: &Path) -> Result<()> {
    if !is_valid_name(&profile.name) {
        return Err(EyesError::Config(format!(
            "invalid watcher profile name '{}' in {}",
            profile.name,
            path.display()
        )));
    }
    if profile.prompt.trim().is_empty() {
        return Err(EyesError::Config(format!(
            "profile '{}' in {} has an empty prompt",
            profile.name,
            path.display()
        )));
    }
    if profile.model.trim().is_empty() {
        return Err(EyesError::Config(format!(
            "profile '{}' in {} has an empty model",
            profile.name,
            path.display()
        )));
    }
    if profile.harness != Harness::Raw {
        return Err(EyesError::Config(format!(
            "profile '{}' in {} uses harness='{}', but watcher profiles currently support only harness='raw'; connect working-agent harnesses with `eyes install claude-code`, `eyes install codex`, or `eyes install pi`",
            profile.name,
            path.display(),
            profile.harness.as_str()
        )));
    }
    Ok(())
}

fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn ensure_single_default(profiles: &[LoadedProfile], path: &Path) -> Result<()> {
    let defaults = profiles
        .iter()
        .filter(|profile| profile.profile.default)
        .count();
    if defaults > 1 {
        return Err(EyesError::Config(format!(
            "multiple default watcher profiles in {}",
            path.display()
        )));
    }
    Ok(())
}

fn select_by_name(profiles: &[LoadedProfile], name: &str) -> Result<LoadedProfile> {
    profiles
        .iter()
        .find(|loaded| loaded.profile.name == name)
        .cloned()
        .ok_or_else(|| EyesError::Config(format!("watcher profile '{name}' not found")))
}

fn select_default(profiles: &[LoadedProfile]) -> Result<LoadedProfile> {
    for source in [
        ProfileSource::Project,
        ProfileSource::User,
        ProfileSource::Bundled,
    ] {
        if let Some(profile) = profiles
            .iter()
            .find(|loaded| loaded.source == source && loaded.profile.default)
        {
            return Ok(profile.clone());
        }
    }
    profiles
        .iter()
        .find(|loaded| loaded.source == ProfileSource::Bundled && loaded.profile.name == "general")
        .cloned()
        .ok_or_else(|| EyesError::Config("no default watcher profile exists".to_owned()))
}

fn extra_eyes_home() -> Option<PathBuf> {
    if let Some(path) = env::var_os("EXTRA_EYES_HOME") {
        return Some(PathBuf::from(path));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".eyes"))
}

fn bundled_general_profile() -> WatcherProfile {
    let mut settings = BTreeMap::new();
    settings.insert("warm_start".to_owned(), toml::Value::Boolean(true));
    WatcherProfile {
        name: "general".to_owned(),
        default: true,
        prompt: "Watch the working agent for correctness, clarity, and risk. Keep messages short and actionable."
            .to_owned(),
        harness: Harness::Raw,
        model: "bundled-default".to_owned(),
        settings,
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn resolves_project_default_before_bundled_default() {
        let temp = TempDir::new().unwrap();
        let watchers = temp.path().join(".eyes/watchers");
        fs::create_dir_all(&watchers).unwrap();
        fs::write(
            watchers.join("harold.toml"),
            r#"
name = "harold"
default = true
prompt = "Watch for test failures."
harness = "raw"
model = "local-shell"
[settings]
command = ["sh", "-c", "cat >/dev/null"]
"#,
        )
        .unwrap();

        let resolved = resolve_profile(Some(temp.path()), None).unwrap();

        assert_eq!(resolved.source, ProfileSource::Project);
        assert_eq!(resolved.profile.name, "harold");
        assert_eq!(resolved.profile.harness, Harness::Raw);
    }

    #[test]
    fn rejects_unknown_profile_fields() {
        let temp = TempDir::new().unwrap();
        let watchers = temp.path().join(".eyes/watchers");
        fs::create_dir_all(&watchers).unwrap();
        fs::write(
            watchers.join("bad.toml"),
            r#"
name = "bad"
prompt = "Watch."
harness = "codex"
model = "gpt-5.3-codex"
surprise = true
"#,
        )
        .unwrap();

        let result = resolve_profile(Some(temp.path()), Some("bad"));

        assert!(matches!(result, Err(EyesError::Config(_))));
    }

    #[test]
    fn accepts_raw_watcher_harness() {
        let temp = TempDir::new().unwrap();
        let watchers = temp.path().join(".eyes/watchers");
        fs::create_dir_all(&watchers).unwrap();
        fs::write(
            watchers.join("raw.toml"),
            r#"
name = "raw"
prompt = "Watch."
harness = "raw"
model = "test-model"
"#,
        )
        .unwrap();

        let resolved = resolve_profile(Some(temp.path()), Some("raw")).unwrap();
        assert_eq!(resolved.profile.name, "raw");
        assert_eq!(resolved.profile.harness, Harness::Raw);
    }

    #[test]
    fn rejects_first_party_harness_values_for_watcher_profiles() {
        for harness in ["claude-code", "codex", "pi"] {
            let temp = TempDir::new().unwrap();
            let watchers = temp.path().join(".eyes/watchers");
            fs::create_dir_all(&watchers).unwrap();
            fs::write(
                watchers.join(format!("{harness}.toml")),
                format!(
                    r#"
name = "{harness}"
prompt = "Watch."
harness = "{harness}"
model = "test-model"
"#
                ),
            )
            .unwrap();

            let result = resolve_profile(Some(temp.path()), Some(harness));

            let Err(EyesError::Config(message)) = result else {
                panic!("expected config error for {harness}");
            };
            assert!(message.contains("watcher profiles currently support only harness='raw'"));
            assert!(message.contains("eyes install"));
        }
    }

    #[test]
    fn rejects_unknown_harness_values() {
        let temp = TempDir::new().unwrap();
        let watchers = temp.path().join(".eyes/watchers");
        fs::create_dir_all(&watchers).unwrap();
        fs::write(
            watchers.join("bad.toml"),
            r#"
name = "bad"
prompt = "Watch."
harness = "unknown"
model = "test-model"
"#,
        )
        .unwrap();

        let result = resolve_profile(Some(temp.path()), Some("bad"));

        assert!(matches!(result, Err(EyesError::Config(_))));
    }
}
