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
    let mut variables = BTreeMap::new();
    if file.exists() {
        load(&file, &mut variables, &mut Vec::new())?;
    }
    Ok(variables.into_iter().collect())
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

fn load(
    file: &Path,
    variables: &mut BTreeMap<String, OsString>,
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
        load(&path, variables, loading)?;
    }
    loading.pop();
    variables.extend(env_table(&config, directory.parent().unwrap_or(directory)));
    Ok(())
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

fn env_table(config: &toml::Table, root: &Path) -> Vec<(String, OsString)> {
    let Some(toml::Value::Table(variables)) = config.get("env") else {
        return Vec::new();
    };
    variables
        .iter()
        .filter_map(|(name, entry)| {
            let value = match entry {
                toml::Value::String(value) => OsString::from(value),
                toml::Value::Table(entry) => {
                    let value = entry.get("value")?.as_str()?;
                    if entry.get("relative").and_then(toml::Value::as_bool) == Some(true) {
                        root.join(value).into_os_string()
                    } else {
                        OsString::from(value)
                    }
                }
                _ => return None,
            };
            Some((name.clone(), value))
        })
        .collect()
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
        let config: toml::Table = "[env]\nPLAIN = \"one\"\nFORCED = { value = \"two\", force = true }\nRELATIVE = { value = \"tools/bin\", relative = true }\n[target.x86_64-unknown-linux-musl]\nrustflags = [\"-C\", \"opt-level=3\"]\n"
            .parse()
            .expect("a valid config");
        let mut read = env_table(&config, Path::new("/work/oh-fx"));
        read.sort();
        assert_eq!(
            read,
            variables(&[
                ("FORCED", "two"),
                ("PLAIN", "one"),
                ("RELATIVE", "/work/oh-fx/tools/bin"),
            ])
        );
        let dotted: toml::Table = "env.DOTTED = \"v\"\n".parse().expect("a dotted key");
        assert_eq!(
            env_table(&dotted, Path::new("/r")),
            variables(&[("DOTTED", "v")])
        );
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
