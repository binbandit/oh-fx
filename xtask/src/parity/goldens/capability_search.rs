use super::tool_schema::{constant, function, properties, required, string, unique};

const SPEC: &str = r#"(?ms)^pub const capability_search = ToolSpec\{\s*\.name = ("[^"\n]*"),\s*\.internal = true,\s*\.description = capability_search_description,\s*\.model_schema = \.\{\s*\.name = ("[^"\n]*"),\s*\.description = capability_search_description,\s*\.input_schema = \.\{\s*\.properties = &\.\{(.*?)\s*\},\s*\.required = &\.\{([^\n]*)\},\s*\.additional_properties = (false),\s*\},\s*\},\s*\.executor_kind ="#;

pub(super) fn extract(source: &str, lexical: &str, limit: usize) -> Result<String, String> {
    let description = unique(
        source,
        r#"(?m)^const capability_search_description =\n    ("[^"\n]*");$"#,
        "const capability_search_description =",
    )?;
    let spec = unique(source, SPEC, "pub const capability_search = ToolSpec{")?;
    if spec[1] != spec[2] {
        return Err("product and model tool names differ".to_owned());
    }
    let max_query_bytes = constant(lexical, "max_query_bytes")?;
    let (properties, names) = properties(
        &spec[3],
        &[],
        &[("lexical_relevance.max_query_bytes", max_query_bytes)],
        limit,
    )?;
    Ok(function(
        string(&spec[2], limit)?,
        string(&description[1], limit)?,
        &properties,
        Some(&spec[5]),
        &required(&spec[4], &names, limit)?,
    ))
}
