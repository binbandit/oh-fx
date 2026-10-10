use sha2::{Digest, Sha256};

use super::tool_schema::{function, properties, required, string, strings, unique};

const FUNCTION_SCHEMA: &str = r#"(?ms)^pub const function_schema: model_tool_schema.FunctionSchema = \.\{\s*\.name = tool_name,\s*\.description = ("[^"\n]*"),\s*\.input_schema = \.\{\s*\.properties = schema_properties\[0\.\.\],\s*\.required = schema_required\[0\.\.\],\s*\.additional_properties = (false),\s*\},\s*\};$"#;
const TOOLS_JSON: &str = r#"(?ms)^fn toolsJsonAlloc\(alloc: std.mem.Allocator\) !\[\]u8 \{\s*const schema_json = try model_tool_schema.builtinFunctionSchemaJsonAlloc\(alloc, function_schema\);\s*defer alloc.free\(schema_json\);\s*return std.fmt.allocPrint\(alloc, "\[\{s\}\]", \.\{schema_json\}\);\s*\}$"#;
const ARTIFACT_TEST: &str =
    r#"test "automatic review model-facing tool contract stays byte exact""#;
const ARTIFACT_DIGEST: &str = r#"(?ms)^test "automatic review model-facing tool contract stays byte exact" \{\s*const tools_json = try toolsJsonAlloc\(std.testing.allocator\);\s*defer std.testing.allocator.free\(tools_json\);\s*var digest: \[std.crypto.hash.sha2.Sha256.digest_length\]u8 = undefined;\s*std.crypto.hash.sha2.Sha256.hash\(tools_json, &digest, \.\{\}\);\s*const actual_hex = std.fmt.bytesToHex\(digest, \.lower\);\s*try std.testing.expectEqualStrings\(\s*"([0-9a-f]{64})",\s*&actual_hex,\s*\);\s*\}$"#;

pub(super) fn extract(source: &str, limit: usize) -> Result<String, String> {
    let name = unique(
        source,
        r#"(?m)^pub const tool_name = ("[^"\n]*");$"#,
        "pub const tool_name =",
    )?;
    let decisions = unique(
        source,
        r"(?m)^const decision_values = \[_\]\[\]const u8\{([^\n]*)\};$",
        "const decision_values =",
    )?;
    let required_names = unique(
        source,
        r"(?m)^const schema_required = \[_\]\[\]const u8\{([^\n]*)\};$",
        "const schema_required =",
    )?;
    let declared = unique(
        source,
        r"(?ms)^const schema_properties = \[_\]model_tool_schema.Property\{\n(.*?)^\};$",
        "const schema_properties =",
    )?;
    let schema = unique(source, FUNCTION_SCHEMA, "pub const function_schema:")?;
    unique(source, TOOLS_JSON, "fn toolsJsonAlloc(")?;
    let decisions = strings(&decisions[1], limit)?.join(",");
    let (properties, names) =
        properties(&declared[1], &[("decision_values", &decisions)], &[], limit)?;
    let tool = format!(
        "[{}]",
        function(
            string(&name[1], limit)?,
            string(&schema[1], limit)?,
            &properties,
            Some(&schema[2]),
            &required(&required_names[1], &names, limit)?,
        )
    );
    if digest(&tool) != unique(source, ARTIFACT_DIGEST, ARTIFACT_TEST)?[1] {
        return Err("extracted tool differs from upstream's artifact digest".to_owned());
    }
    Ok(tool)
}

fn digest(text: &str) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    Sha256::digest(text.as_bytes())
        .iter()
        .flat_map(|byte| {
            [
                char::from(DIGITS[usize::from(byte >> 4)]),
                char::from(DIGITS[usize::from(byte & 15)]),
            ]
        })
        .collect()
}
