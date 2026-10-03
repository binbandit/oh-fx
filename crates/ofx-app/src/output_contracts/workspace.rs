use std::fmt::Write as _;
use std::path::Path;

use ofx_cli::OutputFormat;
use ofx_config::MAX_ADDITIONAL_DIRECTORIES;
use ofx_text::encode_terminal_safe;
use ofx_workspace::{Entry, Mutation};
use serde_json::{Map, Value, json};

pub struct WorkspaceSnapshot<'a> {
    pub primary_directory: &'a Path,
    pub additional_directories: &'a [Entry],
    pub mutation: Option<&'a Mutation>,
}

impl WorkspaceSnapshot<'_> {
    pub fn render(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.render_text(),
            OutputFormat::Json => {
                let mut line = Value::Object(self.json()).to_string();
                line.push('\n');
                line
            }
        }
    }

    fn render_text(&self) -> String {
        let primary = self.primary_directory.to_string_lossy();
        let mut out = format!(
            "[workspace] primary={}\n[workspace] saved_suppressed=false limit={MAX_ADDITIONAL_DIRECTORIES}\n",
            safe(&primary)
        );
        if let Some(mutation) = self.mutation {
            let path = mutation
                .path
                .as_deref()
                .map_or_else(String::new, |path| format!(" {}", safe(path)));
            let _ = writeln!(
                out,
                "[workspace] {}{path} saved_changed={} runtime_changed={} launch_flag_can_restore=false",
                mutation.action, mutation.saved_changed, mutation.runtime_changed
            );
        }
        if self.additional_directories.is_empty() {
            out.push_str("[workspace] additional directories: (none)\n");
            return out;
        }
        out.push_str("[workspace] additional directories:\n");
        for entry in self.additional_directories {
            let _ = writeln!(
                out,
                " - {} saved=true command_line=false available={} active={}",
                safe(&entry.path),
                entry.available,
                entry.available
            );
        }
        out
    }

    fn json(&self) -> Map<String, Value> {
        let mut object = Map::new();
        let action = self.mutation.map_or("list", |mutation| mutation.action);
        let changed = self
            .mutation
            .is_some_and(|mutation| mutation.saved_changed || mutation.runtime_changed);
        object.insert("kind".to_owned(), json!("workspace"));
        object.insert("action".to_owned(), json!(action));
        object.insert("changed".to_owned(), json!(changed));
        object.insert(
            "primary_directory".to_owned(),
            json!(self.primary_directory.to_string_lossy()),
        );
        object.insert("saved_suppressed".to_owned(), json!(false));
        object.insert("limit".to_owned(), json!(MAX_ADDITIONAL_DIRECTORIES));
        if let Some(mutation) = self.mutation {
            if let Some(path) = &mutation.path {
                object.insert("path".to_owned(), json!(path));
            }
            object.insert("saved_changed".to_owned(), json!(mutation.saved_changed));
            object.insert(
                "runtime_changed".to_owned(),
                json!(mutation.runtime_changed),
            );
            object.insert("launch_flag_can_restore".to_owned(), json!(false));
        }
        let entries: Vec<Value> = self
            .additional_directories
            .iter()
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "saved": true,
                    "command_line": false,
                    "available": entry.available,
                    "active": entry.available,
                })
            })
            .collect();
        object.insert("additional_directories".to_owned(), json!(entries));
        object
    }
}

pub fn workspace_error_message(code: &str) -> &'static str {
    match code {
        "InvalidPath" => "path is invalid",
        "PathNotFound" => "directory does not exist",
        "NotDirectory" => "path is not a directory",
        "UnknownAdditionalDirectory" => "directory is not configured as an additional workspace",
        "PrimaryDirectory" => "the primary workspace cannot be added or removed",
        "TooManyDirectories" => "additional directory limit reached",
        "HomeNotSet" => "HOME is not set",
        "SettingsStoreUnavailable" => "settings are unavailable",
        "DurablePathUnsafe" | "PrivateStatePermissionsUnsupported" => "settings path is unsafe",
        _ => "workspace update failed",
    }
}

fn safe(raw: &str) -> String {
    encode_terminal_safe(raw.as_bytes(), usize::MAX).text
}

#[cfg(test)]
mod tests;
