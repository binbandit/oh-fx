use super::tool_schema::{function, properties, required, string, unique};

const SPEC: &str = r#"(?ms)^pub const mcp_features = ToolSpec\{\s*\.name = ("[^"\n]*"),\s*\.description = mcp_features_description,\s*\.model_schema = \.\{\s*\.name = ("[^"\n]*"),\s*\.description = mcp_features_description,\s*\.input_schema = \.\{\s*\.properties = &\.\{(.*?)\s*\},\s*\.required = &\.\{([^\n]*)\},\s*\.additional_properties = (false),\s*\},\s*\},\s*\.executor_kind ="#;

pub(super) fn extract(source: &str, limit: usize) -> Result<String, String> {
    let description = unique(
        source,
        r#"(?m)^const mcp_features_description =\n    ("[^"\n]*");$"#,
        "const mcp_features_description =",
    )?;
    let spec = unique(source, SPEC, "pub const mcp_features = ToolSpec{")?;
    if spec[1] != spec[2] {
        return Err("product and model tool names differ".to_owned());
    }
    let (properties, names) = properties(&spec[3], &[], &[], limit)?;
    Ok(function(
        string(&spec[2], limit)?,
        string(&description[1], limit)?,
        &properties,
        Some(&spec[5]),
        &required(&spec[4], &names, limit)?,
    ))
}
