use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) fn config_variables(repository: &Path) -> Result<Vec<(String, OsString)>, String> {
    let cargo = repository.join(".cargo");
    let legacy = cargo.join("config");
    let file = if legacy.exists() {
        legacy
    } else {
        cargo.join("config.toml")
    };
    let mut entries = BTreeMap::new();
    if file.exists() {
        load(&file, &mut entries, &mut Vec::new())?;
    }
    entries
        .into_iter()
        .map(|(name, entry)| {
            let value = entry
                .exported()
                .ok_or_else(|| format!("env.{name} in the head's cargo config has no value"))?;
            Ok((name, value))
        })
        .collect()
}

pub(crate) fn set_by_head(
    variables: &[(String, OsString)],
    current: impl Fn(&str) -> Option<OsString>,
) -> Vec<String> {
    variables
        .iter()
        .filter(|(name, value)| current(name).as_ref() == Some(value))
        .map(|(name, _)| name.clone())
        .collect()
}

struct Include {
    path: PathBuf,
    optional: bool,
}

enum Entry {
    Plain(OsString),
    Fields {
        value: Option<(String, PathBuf)>,
        relative: Option<bool>,
    },
}

impl Entry {
    fn exported(self) -> Option<OsString> {
        match self {
            Self::Plain(value) => Some(value),
            Self::Fields {
                value: Some((value, root)),
                relative: Some(true),
            } => Some(root.join(value).into_os_string()),
            Self::Fields {
                value: Some((value, _)),
                ..
            } => Some(OsString::from(value)),
            Self::Fields { value: None, .. } => None,
        }
    }
}

fn load(
    file: &Path,
    entries: &mut BTreeMap<String, Entry>,
    loading: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let identity =
        fs::canonicalize(file).map_err(|error| format!("read {}: {error}", file.display()))?;
    if loading.contains(&identity) {
        return Err(format!("{} includes itself", file.display()));
    }
    let text =
        fs::read_to_string(file).map_err(|error| format!("read {}: {error}", file.display()))?;
    let config: toml::Table = text
        .parse()
        .map_err(|error| format!("parse {}: {error}", file.display()))?;
    let directory = file.parent().unwrap_or(Path::new("."));
    loading.push(identity);
    for include in includes(&config).map_err(|error| format!("{error} in {}", file.display()))? {
        let path = directory.join(include.path);
        if include.optional && !path.exists() {
            continue;
        }
        load(&path, entries, loading)?;
    }
    loading.pop();
    merge_env(&config, directory.parent().unwrap_or(directory), entries)
        .map_err(|error| format!("{error} in {}", file.display()))
}

fn includes(config: &toml::Table) -> Result<Vec<Include>, &'static str> {
    let Some(include) = config.get("include") else {
        return Ok(Vec::new());
    };
    let toml::Value::Array(entries) = include else {
        return Err("`include` is not a list of strings or tables");
    };
    entries
        .iter()
        .map(|entry| match entry {
            toml::Value::String(path) => Ok(Include {
                path: PathBuf::from(path),
                optional: false,
            }),
            toml::Value::Table(entry) => match entry.get("path") {
                Some(toml::Value::String(path)) => Ok(Include {
                    path: PathBuf::from(path),
                    optional: entry.get("optional").and_then(toml::Value::as_bool) == Some(true),
                }),
                _ => Err("an `include` table has no `path` string"),
            },
            _ => Err("`include` is not a list of strings or tables"),
        })
        .collect()
}

fn merge_env(
    config: &toml::Table,
    root: &Path,
    entries: &mut BTreeMap<String, Entry>,
) -> Result<(), String> {
    let Some(table) = config.get("env") else {
        return Ok(());
    };
    let toml::Value::Table(table) = table else {
        return Err("`env` is not a table".to_owned());
    };
    for (name, incoming) in table {
        let merged = match (entries.remove(name), incoming) {
            (None | Some(Entry::Plain(_)), toml::Value::String(value)) => {
                Entry::Plain(OsString::from(value))
            }
            (None, toml::Value::Table(fields)) => fields_entry(name, fields, root, None, None)?,
            (Some(Entry::Fields { value, relative }), toml::Value::Table(fields)) => {
                fields_entry(name, fields, root, value, relative)?
            }
            _ => return Err(format!("env.{name} mixes a string with a table")),
        };
        entries.insert(name.clone(), merged);
    }
    Ok(())
}

fn fields_entry(
    name: &str,
    fields: &toml::Table,
    root: &Path,
    value: Option<(String, PathBuf)>,
    relative: Option<bool>,
) -> Result<Entry, String> {
    let value = match fields.get("value") {
        None => value,
        Some(toml::Value::String(value)) => Some((value.clone(), root.to_path_buf())),
        Some(_) => return Err(format!("env.{name}.value is not a string")),
    };
    let relative = match fields.get("relative") {
        None => relative,
        Some(toml::Value::Boolean(relative)) => Some(*relative),
        Some(_) => return Err(format!("env.{name}.relative is not a boolean")),
    };
    Ok(Entry::Fields { value, relative })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn variables(entries: &[(&str, &str)]) -> Vec<(String, OsString)> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), OsString::from(value)))
            .collect()
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("a config directory");
        fs::write(path, text).expect("a config file");
    }

    #[test]
    fn reads_the_value_cargo_exports_for_each_entry() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        write(
            &repository.path().join(".cargo/config.toml"),
            "[env]\nDOTTED.value = \"v\"\nPLAIN = \"one\"\nFORCED = { value = \"two\", force = true }\nRELATIVE = { value = \"tools/bin\", relative = true }\nABSOLUTE = { value = \"tools/bin\", relative = false }\n[target.x86_64-unknown-linux-musl]\nrustflags = [\"-C\", \"opt-level=3\"]\n",
        );
        let relative = repository.path().join("tools/bin");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[
                ("ABSOLUTE", "tools/bin"),
                ("DOTTED", "v"),
                ("FORCED", "two"),
                ("PLAIN", "one"),
                ("RELATIVE", relative.to_str().expect("a UTF-8 path")),
            ]))
        );
    }

    #[test]
    fn merges_entry_fields_across_files_as_cargo_does() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        write(
            &cargo.join("sub/inc.toml"),
            "[env]\nSPLIT = { value = \"vendor/sdk\" }\nFLAG_FIRST = { relative = true }\nTURNED_OFF = { value = \"vendor/sdk\", relative = true }\nLATER_VALUE = { value = \"a\", relative = true }\n",
        );
        write(
            &cargo.join("inc2.toml"),
            "[env]\nLATER_VALUE = { value = \"b\" }\n",
        );
        write(
            &cargo.join("config.toml"),
            "include = [\"sub/inc.toml\", \"inc2.toml\"]\n[env]\nSPLIT = { relative = true }\nFLAG_FIRST = { value = \"vendor/sdk\" }\nTURNED_OFF = { relative = false }\n",
        );
        let in_cargo = cargo.join("vendor/sdk");
        let in_repository = repository.path().join("vendor/sdk");
        let later = repository.path().join("b");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[
                ("FLAG_FIRST", in_repository.to_str().expect("a UTF-8 path")),
                ("LATER_VALUE", later.to_str().expect("a UTF-8 path")),
                ("SPLIT", in_cargo.to_str().expect("a UTF-8 path")),
                ("TURNED_OFF", "vendor/sdk"),
            ]))
        );
    }

    #[test]
    fn refuses_entries_cargo_cannot_merge() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        write(
            &cargo.join("config.toml"),
            "include = [\"inc.toml\"]\n[env]\nX = { relative = true }\n",
        );
        for included in [
            "[env]\nX = \"plain\"\n",
            "[env]\nY = 1\n",
            "[env]\nX = { value = 1 }\n",
            "[env]\nX = { value = \"v\", relative = \"yes\" }\n",
            "env = 1\n",
        ] {
            write(&cargo.join("inc.toml"), included);
            assert!(config_variables(repository.path()).is_err(), "{included}");
        }
        write(&cargo.join("inc.toml"), "");
        assert!(config_variables(repository.path()).is_err());
    }

    #[test]
    fn reads_only_the_file_cargo_picks() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        assert_eq!(config_variables(repository.path()), Ok(Vec::new()));
        write(&cargo.join("config.toml"), "[env]\nFROM_TOML = \"1\"\n");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[("FROM_TOML", "1")]))
        );
        write(&cargo.join("config"), "[env]\nFROM_LEGACY = \"2\"\n");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[("FROM_LEGACY", "2")]))
        );
        write(&cargo.join("config.toml"), "[env\n");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[("FROM_LEGACY", "2")]))
        );
        write(&cargo.join("config"), "[env\n");
        assert!(config_variables(repository.path()).is_err());
    }

    #[test]
    fn follows_includes_with_cargos_precedence() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        let absolute = repository.path().join("elsewhere.toml");
        write(
            &cargo.join("config.toml"),
            &format!(
                "include = [\"a.toml\", \"b.toml\", {{ path = \"missing.toml\", optional = true }}, \"sub/c.toml\", \"{}\"]\n[env]\nK0 = \"main\"\nK2 = \"main\"\n",
                absolute.display()
            ),
        );
        write(
            &cargo.join("a.toml"),
            "[env]\nK1 = \"a\"\nK2 = \"a\"\nK3 = \"a\"\n",
        );
        write(&cargo.join("b.toml"), "[env]\nK1 = \"b\"\n");
        write(
            &cargo.join("sub/c.toml"),
            "include = [\"d.toml\"]\n[env]\nK4 = { value = \"rel\", relative = true }\nK5 = \"c\"\n",
        );
        write(&cargo.join("sub/d.toml"), "[env]\nK5 = \"d\"\nK6 = \"d\"\n");
        write(&absolute, "[env]\nK7 = \"absolute\"\n");
        let rel = cargo.join("rel");
        assert_eq!(
            config_variables(repository.path()),
            Ok(variables(&[
                ("K0", "main"),
                ("K1", "b"),
                ("K2", "main"),
                ("K3", "a"),
                ("K4", rel.to_str().expect("a UTF-8 path")),
                ("K5", "c"),
                ("K6", "d"),
                ("K7", "absolute"),
            ]))
        );
    }

    #[test]
    fn refuses_includes_cargo_refuses() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let config = repository.path().join(".cargo/config.toml");
        for text in [
            "include = [\"missing.toml\"]\n",
            "include = \"a.toml\"\n",
            "include = [{ optional = true }]\n",
            "include = [\"config.toml\"]\n",
            "include = [\"./sub/../config.toml\"]\n",
        ] {
            fs::create_dir_all(repository.path().join(".cargo/sub")).expect("a sub directory");
            write(&config, text);
            assert!(config_variables(repository.path()).is_err(), "{text}");
        }
    }

    #[test]
    fn hides_only_the_variables_whose_value_the_head_config_supplied() {
        let head = variables(&[
            ("EXPORTED", "head"),
            ("USER_SET", "head"),
            ("UNSET", "head"),
        ]);
        let current = |name: &str| match name {
            "EXPORTED" => Some(OsString::from("head")),
            "USER_SET" => Some(OsString::from("mine")),
            _ => None,
        };
        assert_eq!(set_by_head(&head, current), ["EXPORTED"]);
    }
}
