use std::collections::BTreeMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};

pub(crate) fn cargo_home() -> Option<PathBuf> {
    env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::home_dir().map(|home| home.join(".cargo")))
}

pub(crate) fn config_variables(
    repository: &Path,
    cargo_home: Option<&Path>,
) -> Result<Vec<(String, OsString)>, String> {
    let mut layers: Vec<(PathBuf, bool)> = repository
        .ancestors()
        .map(|directory| (directory.join(".cargo"), directory == repository))
        .collect();
    layers.extend(cargo_home.map(|home| (home.to_path_buf(), false)));
    let mut entries = BTreeMap::new();
    for (directory, local) in layers.into_iter().rev() {
        if let Some(file) = config_file(&directory) {
            load(&file, local, &mut entries, &mut Vec::new())?;
        }
    }
    Ok(entries
        .into_iter()
        .filter(|(_, entry)| entry.local())
        .filter_map(|(name, entry)| entry.exported().map(|value| (name, value)))
        .collect())
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

fn config_file(directory: &Path) -> Option<PathBuf> {
    [directory.join("config"), directory.join("config.toml")]
        .into_iter()
        .find(|file| file.exists())
}

struct Include {
    path: PathBuf,
    optional: bool,
}

struct Field {
    value: toml::Value,
    root: PathBuf,
    local: bool,
}

enum Entry {
    Plain(Field),
    Fields(BTreeMap<String, Field>),
}

impl Entry {
    fn local(&self) -> bool {
        match self {
            Self::Plain(field) => field.local,
            Self::Fields(fields) => fields.values().any(|field| field.local),
        }
    }

    fn exported(self) -> Option<OsString> {
        let mut fields = match self {
            Self::Plain(field) => return field.value.as_str().map(OsString::from),
            Self::Fields(fields) => fields,
        };
        let relative = match fields.remove("relative") {
            None => false,
            Some(field) => field.value.as_bool()?,
        };
        let value = fields.remove("value")?;
        let text = value.value.as_str()?;
        Some(if relative {
            value.root.join(text).into_os_string()
        } else {
            OsString::from(text)
        })
    }
}

fn load(
    file: &Path,
    local: bool,
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
        let path = normalize(&directory.join(include.path));
        if include.optional && !path.exists() {
            continue;
        }
        load(&path, local, entries, loading)?;
    }
    loading.pop();
    let root = directory.parent().unwrap_or(directory);
    merge_env(&config, root, local, entries)
        .map_err(|error| format!("{error} in {}", file.display()))
}

fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            component => normal.push(component),
        }
    }
    normal
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
    local: bool,
    entries: &mut BTreeMap<String, Entry>,
) -> Result<(), String> {
    let Some(table) = config.get("env") else {
        return Ok(());
    };
    let toml::Value::Table(table) = table else {
        return Err("`env` is not a table".to_owned());
    };
    let field = |value: &toml::Value| Field {
        value: value.clone(),
        root: root.to_path_buf(),
        local,
    };
    for (name, incoming) in table {
        let conflict = || format!("env.{name} is a table in one config file and not in another");
        let merged = match (entries.remove(name), incoming) {
            (existing, toml::Value::Table(incoming)) => {
                let mut fields = match existing {
                    None => BTreeMap::new(),
                    Some(Entry::Fields(fields)) => fields,
                    Some(Entry::Plain(_)) => return Err(conflict()),
                };
                fields.extend(
                    incoming
                        .iter()
                        .map(|(key, value)| (key.clone(), field(value))),
                );
                Entry::Fields(fields)
            }
            (Some(Entry::Fields(_)), _) => return Err(conflict()),
            (_, incoming) => Entry::Plain(field(incoming)),
        };
        entries.insert(name.clone(), merged);
    }
    Ok(())
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

    fn text(path: &Path) -> &str {
        path.to_str().expect("a UTF-8 path")
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
            config_variables(repository.path(), None),
            Ok(variables(&[
                ("ABSOLUTE", "tools/bin"),
                ("DOTTED", "v"),
                ("FORCED", "two"),
                ("PLAIN", "one"),
                ("RELATIVE", text(&relative)),
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
            config_variables(repository.path(), None),
            Ok(variables(&[
                ("FLAG_FIRST", text(&in_repository)),
                ("LATER_VALUE", text(&later)),
                ("SPLIT", text(&in_cargo)),
                ("TURNED_OFF", "vendor/sdk"),
            ]))
        );
    }

    #[test]
    fn completes_local_entries_from_ancestor_and_cargo_home_config() {
        let scratch = tempfile::tempdir().expect("a scratch directory");
        let home = scratch.path().join("home");
        let outer = scratch.path().join("outer");
        let repository = outer.join("repo");
        write(
            &home.join(".cargo/config.toml"),
            "[env]\nFROM_HOME = { value = \"vendor/sdk\", relative = false }\nRANKED = { value = \"home\" }\nSHADOWED = \"home\"\nHOME_ONLY = \"home\"\n",
        );
        write(
            &outer.join(".cargo/config.toml"),
            "[env]\nFROM_ANCESTOR = { value = \"tools\" }\nRANKED = { value = \"outer\" }\nSHADOWED = \"outer\"\nANCESTOR_ONLY = \"outer\"\n",
        );
        write(
            &repository.join(".cargo/config.toml"),
            "[env]\nFROM_HOME = { relative = true }\nFROM_ANCESTOR = { relative = true }\nRANKED = { relative = true }\nSHADOWED = \"repo\"\n",
        );
        assert_eq!(
            config_variables(&repository, Some(&home.join(".cargo"))),
            Ok(variables(&[
                ("FROM_ANCESTOR", text(&outer.join("tools"))),
                ("FROM_HOME", text(&home.join("vendor/sdk"))),
                ("RANKED", text(&outer.join("outer"))),
                ("SHADOWED", "repo"),
            ]))
        );
    }

    #[test]
    fn accepts_what_a_later_file_overrides_as_cargo_does() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        write(
            &cargo.join("inc.toml"),
            "[env]\nRETYPED = 1\nVALUE = { value = 1 }\nFLAG = { value = \"v\", relative = \"yes\" }\n",
        );
        write(
            &cargo.join("config.toml"),
            "include = [\"inc.toml\"]\n[env]\nRETYPED = \"s\"\nVALUE = { value = \"v\" }\nFLAG = { relative = true }\n",
        );
        let flagged = repository.path().join("v");
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[
                ("FLAG", text(&flagged)),
                ("RETYPED", "s"),
                ("VALUE", "v"),
            ]))
        );
    }

    #[test]
    fn skips_entries_that_resolve_to_no_value() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        write(
            &repository.path().join(".cargo/config.toml"),
            "[env]\nNUMBER = 1\nNO_VALUE = { relative = true }\nNUMERIC_VALUE = { value = 1 }\nTEXT_FLAG = { value = \"v\", relative = \"yes\" }\nKEPT = \"kept\"\n",
        );
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[("KEPT", "kept")]))
        );
    }

    #[test]
    fn refuses_entries_cargo_cannot_merge() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        for (included, own) in [
            ("[env]\nX = \"plain\"\n", "[env]\nX = { relative = true }\n"),
            ("[env]\nX = { value = \"v\" }\n", "[env]\nX = \"plain\"\n"),
            ("[env]\nX = { value = \"v\" }\n", "[env]\nX = 1\n"),
            ("env = 1\n", "[env]\nX = \"plain\"\n"),
        ] {
            write(&cargo.join("inc.toml"), included);
            write(
                &cargo.join("config.toml"),
                &format!("include = [\"inc.toml\"]\n{own}"),
            );
            assert!(
                config_variables(repository.path(), None).is_err(),
                "{included}{own}"
            );
        }
    }

    #[test]
    fn reads_only_the_file_cargo_picks() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        assert_eq!(config_variables(repository.path(), None), Ok(Vec::new()));
        write(&cargo.join("config.toml"), "[env]\nFROM_TOML = \"1\"\n");
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[("FROM_TOML", "1")]))
        );
        write(&cargo.join("config"), "[env]\nFROM_LEGACY = \"2\"\n");
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[("FROM_LEGACY", "2")]))
        );
        write(&cargo.join("config.toml"), "[env\n");
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[("FROM_LEGACY", "2")]))
        );
        write(&cargo.join("config"), "[env\n");
        assert!(config_variables(repository.path(), None).is_err());
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
            config_variables(repository.path(), None),
            Ok(variables(&[
                ("K0", "main"),
                ("K1", "b"),
                ("K2", "main"),
                ("K3", "a"),
                ("K4", text(&rel)),
                ("K5", "c"),
                ("K6", "d"),
                ("K7", "absolute"),
            ]))
        );
    }

    #[test]
    fn resolves_include_paths_lexically_as_cargo_does() {
        let repository = tempfile::tempdir().expect("a scratch repository");
        let cargo = repository.path().join(".cargo");
        write(
            &cargo.join("config.toml"),
            "include = [\"../outside.toml\", \"./a.toml\", \"sub/inner/../inner/i.toml\"]\n",
        );
        write(
            &repository.path().join("outside.toml"),
            "[env]\nO = { value = \"x\", relative = true }\n",
        );
        write(
            &cargo.join("a.toml"),
            "[env]\nA = { value = \"x\", relative = true }\n",
        );
        write(
            &cargo.join("sub/inner/i.toml"),
            "include = [\"../dn.toml\"]\n[env]\nI = { value = \"x\", relative = true }\n",
        );
        write(
            &cargo.join("sub/dn.toml"),
            "[env]\nN = { value = \"x\", relative = true }\n",
        );
        let above = repository.path().parent().expect("a parent").join("x");
        assert_eq!(
            config_variables(repository.path(), None),
            Ok(variables(&[
                ("A", text(&repository.path().join("x"))),
                ("I", text(&cargo.join("sub/x"))),
                ("N", text(&cargo.join("x"))),
                ("O", text(&above)),
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
            assert!(config_variables(repository.path(), None).is_err(), "{text}");
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
