use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::{EyesError, Result};

#[derive(Debug, Clone, Serialize)]
pub struct PiInstallResult {
    pub extension_path: PathBuf,
    pub eyes_bin: PathBuf,
}

pub fn install_pi_extension(extension_path: &Path, eyes_bin: &Path) -> Result<PiInstallResult> {
    if let Some(parent) = extension_path.parent() {
        fs::create_dir_all(parent)?;
    }
    atomic_write(extension_path, &extension_source(eyes_bin)?)?;
    Ok(PiInstallResult {
        extension_path: extension_path.to_path_buf(),
        eyes_bin: eyes_bin.to_path_buf(),
    })
}

fn extension_source(eyes_bin: &Path) -> Result<String> {
    let eyes_bin = serde_json::to_string(&eyes_bin.display().to_string())?;
    Ok(format!(
        r#"import type {{ ExtensionAPI, ExtensionContext }} from "@mariozechner/pi-coding-agent";
import {{ spawnSync }} from "node:child_process";

const EYES_BIN = {eyes_bin};
const CURSOR_CHANNEL = "hook";

export default function (pi: ExtensionAPI) {{
  pi.on("input", async (event, ctx) => {{
    if (event.source === "extension") {{
      return {{ action: "continue" }};
    }}

    runEyes(ctx, [
      "feed",
      "--harness",
      "pi",
      "--event",
      "input",
      "--payload-json",
      JSON.stringify({{
        type: "input",
        text: event.text,
        source: event.source,
        session_id: ctx.sessionManager.getSessionId(),
      }}),
      "--project",
      ctx.cwd,
    ]);

    const feedback = fetchFeedback(ctx);
    if (!feedback) {{
      return {{ action: "continue" }};
    }}

    return {{
      action: "transform",
      text: `${{feedback}}\n\n${{event.text}}`,
      images: event.images,
    }};
  }});

  pi.on("session_shutdown", async (_event, ctx) => {{
    runEyes(ctx, [
      "feed",
      "--harness",
      "pi",
      "--event",
      "session_shutdown",
      "--payload-json",
      JSON.stringify({{
        type: "session_shutdown",
        session_id: ctx.sessionManager.getSessionId(),
        last_assistant_message: lastAssistantMessage(ctx),
      }}),
      "--project",
      ctx.cwd,
    ]);
  }});
}}

function fetchFeedback(ctx: ExtensionContext): string | undefined {{
  const result = runEyes(ctx, [
    "hook",
    "fetch",
    "--channel",
    CURSOR_CHANNEL,
    "--cursor-key",
    `pi:${{ctx.sessionManager.getSessionId()}}:hook`,
    "--project",
    ctx.cwd,
  ]);
  const text = result.stdout.trim();
  return text.length > 0 ? text : undefined;
}}

function runEyes(ctx: ExtensionContext, args: string[]): {{ stdout: string }} {{
  const result = spawnSync(EYES_BIN, args, {{
    cwd: ctx.cwd,
    encoding: "utf8",
    timeout: 1500,
  }});
  if (result.error || result.status !== 0) {{
    return {{ stdout: "" }};
  }}
  return {{ stdout: result.stdout ?? "" }};
}}

function lastAssistantMessage(ctx: ExtensionContext): string {{
  const entries = ctx.sessionManager.getEntries();
  for (let index = entries.length - 1; index >= 0; index--) {{
    const entry = entries[index];
    if (entry.type !== "message" || entry.message.role !== "assistant") {{
      continue;
    }}
    const content = entry.message.content;
    if (!Array.isArray(content)) {{
      return "";
    }}
    return content
      .filter((part): part is {{ type: "text"; text: string }} => part.type === "text")
      .map((part) => part.text)
      .join("\n");
  }}
  return "";
}}
"#
    ))
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| EyesError::Config("pi extension path must have a parent".to_owned()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| EyesError::Config("pi extension path must have a file name".to_owned()))?
        .to_string_lossy();
    let temp_path = parent.join(format!(".{file_name}.extra-eyes.tmp"));
    fs::write(&temp_path, contents)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp_path, metadata.permissions())?;
    }
    fs::rename(temp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn installs_pi_extension() {
        let temp = TempDir::new().unwrap();
        let extension = temp.path().join(".pi/extensions/extra-eyes.ts");
        let eyes_bin = temp.path().join("bin/eyes");

        let result = install_pi_extension(&extension, &eyes_bin).unwrap();

        assert_eq!(result.extension_path, extension);
        let written = fs::read_to_string(&result.extension_path).unwrap();
        assert!(written.contains("pi.on(\"input\""));
        assert!(written.contains("pi.on(\"session_shutdown\""));
        assert!(written.contains("hook"));
        assert!(written.contains("fetch"));
        assert!(written.contains(&eyes_bin.display().to_string()));
    }
}
