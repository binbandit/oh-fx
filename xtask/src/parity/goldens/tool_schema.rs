use std::collections::BTreeSet;

use regex::{Captures, Regex};

pub(super) fn unique<'a>(
    source: &'a str,
    pattern: &str,
    declaration: &str,
) -> Result<Captures<'a>, String> {
    if source.matches(declaration).count() != 1 {
        return Err(format!("expected one {declaration}"));
    }
    Regex::new(pattern)
        .map_err(|error| error.to_string())?
        .captures(source)
        .ok_or_else(|| format!("unsupported {declaration} grammar"))
}

pub(super) fn description_limit(writer: &str) -> Result<usize, String> {
    let limit = unique(
        writer,
        r"(?m)^pub const description_max_bytes: usize = ([0-9]+);$",
        "pub const description_max_bytes:",
    )?;
    limit[1]
        .parse()
        .map_err(|_| "invalid description limit".to_owned())
}

pub(super) fn strings(value: &str, limit: usize) -> Result<Vec<&str>, String> {
    let literal = Regex::new(r#""[ -!#-\[\]-~]*""#).map_err(|error| error.to_string())?;
    let mut rest = value.trim();
    let mut values = Vec::new();
    while !rest.is_empty() {
        let found = literal
            .find(rest)
            .filter(|found| found.start() == 0)
            .ok_or("unsupported plain ASCII string syntax")?;
        if found.as_str().len() - 2 > limit {
            return Err("string exceeds the audited writer's description limit".to_owned());
        }
        values.push(found.as_str());
        rest = rest[found.end()..].trim_start();
        if !rest.is_empty() {
            rest = rest.strip_prefix(',').ok_or("expected comma")?.trim_start();
        }
    }
    if values.is_empty() {
        return Err("expected a nonempty string list".to_owned());
    }
    Ok(values)
}

pub(super) fn string(value: &str, limit: usize) -> Result<&str, String> {
    match strings(value, limit)?.as_slice() {
        [single] => Ok(single),
        _ => Err("expected one string".to_owned()),
    }
}

pub(super) fn properties(
    source: &str,
    enums: &[(&str, &str)],
    constants: &[(&str, usize)],
    limit: usize,
) -> Result<(String, BTreeSet<String>), String> {
    let property = Regex::new(
        r#"^\s*\.\{\s*\.name = ("[^"\n]*"),\s*\.json_type = \.(string|integer|object),\s*(?:\.shape = &\.\{ \.enum_values = (?:([a-z_]+)\[0\.\.\]|&\.\{ ([^{}\n]*) \}) \},\s*)?(?:\.bounds = &\.\{ ([^{}\n]*) \},\s*)?\.description = ("[^"\n]*"),?\s*\},"#,
    )
    .map_err(|error| error.to_string())?;
    let mut rest = source;
    let mut json = Vec::new();
    let mut names = BTreeSet::new();
    while !rest.trim().is_empty() {
        let captures = property
            .captures(rest)
            .ok_or("unsupported property grammar")?;
        let name = string(&captures[1], limit)?;
        if !names.insert(name.to_owned()) {
            return Err("duplicate property name".to_owned());
        }
        let shape = match (captures.get(3), captures.get(4)) {
            (Some(declaration), _) => {
                let values = enums
                    .iter()
                    .find(|(name, _)| *name == declaration.as_str())
                    .ok_or("unsupported enum declaration")?
                    .1;
                format!(r#","enum":[{values}]"#)
            }
            (None, Some(inline)) => format!(
                r#","enum":[{}]"#,
                strings(inline.as_str(), limit)?.join(",")
            ),
            (None, None) => String::new(),
        };
        let bounds = captures
            .get(5)
            .map(|bounds| length_bounds(bounds.as_str(), constants))
            .transpose()?
            .unwrap_or_default();
        let description = string(&captures[6], limit)?;
        if description == r#""""# {
            return Err("an empty description needs a writer audit".to_owned());
        }
        json.push(format!(
            r#"{name}:{{"type":"{}"{shape}{bounds},"description":{description}}}"#,
            &captures[2]
        ));
        rest = &rest[captures[0].len()..];
    }
    if json.is_empty() {
        return Err("empty property list".to_owned());
    }
    Ok((json.join(","), names))
}

fn length_bounds(source: &str, constants: &[(&str, usize)]) -> Result<String, String> {
    let bound = Regex::new(r"^\.(min_length|max_length) = ([0-9]+|[a-z_]+\.[a-z_]+)$")
        .map_err(|error| error.to_string())?;
    let mut minimum = None;
    let mut maximum = None;
    for entry in source.split(", ") {
        let captures = bound
            .captures(entry)
            .ok_or("unsupported property bounds grammar")?;
        let value = match captures[2].parse::<usize>() {
            Ok(value) => value,
            Err(_) => {
                constants
                    .iter()
                    .find(|(name, _)| *name == &captures[2])
                    .ok_or("unsupported bound constant")?
                    .1
            }
        };
        let slot = if &captures[1] == "min_length" {
            &mut minimum
        } else {
            &mut maximum
        };
        if slot.replace(value).is_some() {
            return Err("repeated property bound".to_owned());
        }
    }
    Ok(minimum
        .map(|value| format!(r#","minLength":{value}"#))
        .into_iter()
        .chain(maximum.map(|value| format!(r#","maxLength":{value}"#)))
        .collect())
}

pub(super) fn constant(source: &str, name: &str) -> Result<usize, String> {
    let declaration = format!("pub const {name}: usize =");
    let value = unique(
        source,
        &format!(r"(?m)^pub const {name}: usize = ([0-9]+)(?: \* ([0-9]+))?;$"),
        &declaration,
    )?;
    let factor = |index: usize| {
        value
            .get(index)
            .map_or(Ok(1), |text| text.as_str().parse::<usize>())
            .map_err(|_| format!("invalid {name}"))
    };
    factor(1)?
        .checked_mul(factor(2)?)
        .ok_or_else(|| format!("invalid {name}"))
}

pub(super) fn required(
    value: &str,
    names: &BTreeSet<String>,
    limit: usize,
) -> Result<String, String> {
    let required = strings(value, limit)?;
    let mut seen = BTreeSet::new();
    for name in &required {
        if !names.contains(*name) || !seen.insert(*name) {
            return Err("required field is missing or repeated".to_owned());
        }
    }
    Ok(required.join(","))
}

pub(super) fn function(
    name: &str,
    description: &str,
    properties: &str,
    additional_properties: Option<&str>,
    required: &str,
) -> String {
    let additional = additional_properties
        .map(|value| format!(r#","additionalProperties":{value}"#))
        .unwrap_or_default();
    format!(
        r#"{{"type":"function","name":{name},"description":{description},"inputSchema":{{"type":"object","properties":{{{properties}}}{additional},"required":[{required}]}}}}"#
    )
}
