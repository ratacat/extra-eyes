use std::env;
use std::io::{self, IsTerminal};

#[derive(Debug, Copy, Clone, PartialEq, Eq, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Copy, Clone)]
pub struct Style {
    enabled: bool,
}

impl Style {
    pub fn stdout(choice: ColorChoice) -> Self {
        Self::for_stream(choice, Stream::Stdout)
    }

    pub fn stderr(choice: ColorChoice) -> Self {
        Self::for_stream(choice, Stream::Stderr)
    }

    pub fn for_stream(choice: ColorChoice, stream: Stream) -> Self {
        Self {
            enabled: color_enabled(choice, stream),
        }
    }

    pub fn line(&self, app: &str, target: &str, state: &str, details: impl AsRef<str>) -> String {
        let state = self.state(state, state);
        let details = details.as_ref();
        if target.is_empty() && details.is_empty() {
            format!("{} {state}", self.brand(app))
        } else if target.is_empty() {
            format!("{} {state} {details}", self.brand(app))
        } else if details.is_empty() {
            format!("{} {target} {state}", self.brand(app))
        } else {
            format!("{} {target} {state} {details}", self.brand(app))
        }
    }

    pub fn brand(&self, text: &str) -> String {
        self.paint("\x1b[1;36m", text)
    }

    pub fn arrow(&self) -> String {
        self.paint("\x1b[37m", "->")
    }

    pub fn success(&self, text: &str) -> String {
        self.paint("\x1b[32m", text)
    }

    pub fn muted(&self, text: &str) -> String {
        self.paint("\x1b[90m", text)
    }

    fn state(&self, state: &str, text: &str) -> String {
        match state {
            "error" | "failed" | "timeout" => self.paint("\x1b[1;31m", text),
            "warning" | "warn" => self.paint("\x1b[33m", text),
            "info" | "message" | "ready" => self.paint("\x1b[36m", text),
            "started" | "running" | "installed" | "trusted" | "queued" | "stopping" | "quiet" => {
                self.paint("\x1b[32m", text)
            }
            _ => text.to_owned(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("{code}{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}

pub fn details(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("  ")
}

pub fn compact_text(text: &str, max_bytes: usize) -> String {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() <= max_bytes {
        return normalized;
    }
    if max_bytes <= 3 {
        return ".".repeat(max_bytes);
    }
    format!("{}...", prefix_by_char_boundary(&normalized, max_bytes - 3))
}

fn color_enabled(choice: ColorChoice, stream: Stream) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => color_enabled_auto(stream),
    }
}

fn color_enabled_auto(stream: Stream) -> bool {
    if env_present_nonzero("NO_COLOR") {
        return false;
    }
    if env::var("TERM").is_ok_and(|term| term == "dumb") {
        return false;
    }
    if env::var("CLICOLOR").is_ok_and(|value| value == "0") {
        return false;
    }
    if env_present_nonzero("CLICOLOR_FORCE") || env_present_nonzero("FORCE_COLOR") {
        return true;
    }
    match stream {
        Stream::Stdout => io::stdout().is_terminal(),
        Stream::Stderr => io::stderr().is_terminal(),
    }
}

fn env_present_nonzero(name: &str) -> bool {
    env::var(name).is_ok_and(|value| !value.is_empty() && value != "0")
}

fn prefix_by_char_boundary(value: &str, max_bytes: usize) -> &str {
    if max_bytes >= value.len() {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_text_normalizes_and_truncates_at_char_boundaries() {
        assert_eq!(compact_text("one\n two\tthree", 50), "one two three");
        assert_eq!(compact_text("abcdef", 6), "abcdef");
        assert_eq!(compact_text("abcdef", 5), "ab...");
        assert_eq!(compact_text("éclair forever", 6), "éc...");
    }
}
