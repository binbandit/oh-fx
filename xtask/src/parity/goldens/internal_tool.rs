use super::tool_schema::{function, properties, required, string, unique};

pub(super) fn extract(
    source: &str,
    tool: &str,
    constants: &[(&str, usize)],
    limit: usize,
) -> Result<String, String> {
    let description = unique(
        source,
        &format!(r#"(?m)^const {tool}_description =\n    ("[^"\n]*");$"#),
        &format!("const {tool}_description ="),
    )?;
    let declaration = format!("pub const {tool} = ToolSpec{{");
    let spec = unique(
        source,
        &format!(
            r#"(?ms)^pub const {tool} = ToolSpec\{{\s*\.name = ("[^"\n]*"),\s*\.internal = true,\s*\.description = {tool}_description,\s*\.model_schema = \.\{{\s*\.name = ("[^"\n]*"),\s*\.description = {tool}_description,\s*\.input_schema = \.\{{\s*\.properties = &\.\{{(.*?)\s*\}},\s*\.required = &\.\{{([^\n]*)\}},\s*(?:\.additional_properties = (false),\s*)?\}},\s*\}},\s*\.executor_kind ="#
        ),
        &declaration,
    )?;
    if spec[1] != spec[2] {
        return Err("product and model tool names differ".to_owned());
    }
    let (properties, names) = properties(&spec[3], &[], constants, limit)?;
    Ok(function(
        string(&spec[2], limit)?,
        string(&description[1], limit)?,
        &properties,
        spec.get(5).map(|value| value.as_str()),
        &required(&spec[4], &names, limit)?,
    ))
}
