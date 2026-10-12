use super::*;

#[test]
fn every_resource_template_argument_is_a_required_field_and_expands_encoded() {
    let template = "custom://project/{project}/{path}{?lang,mode}";
    assert_eq!(
        template_fields(template),
        Ok(["project", "path", "lang", "mode"]
            .map(str::to_owned)
            .to_vec())
    );
    let values = [
        ("project", "alpha team"),
        ("path", "src/lib"),
        ("lang", "en"),
        ("mode", "fast"),
    ];
    let lookup = |name: &str| {
        values
            .iter()
            .find(|(field, _)| *field == name)
            .map(|(_, value)| *value)
    };
    assert_eq!(
        expand_template(template, lookup),
        Ok("custom://project/alpha%20team/src%2Flib?lang=en&mode=fast".to_owned())
    );
}

#[test]
fn template_operators_follow_their_prefixes_and_separators() {
    let lookup = |name: &str| match name {
        "a" => Some("x/y"),
        "b" => Some(""),
        _ => None,
    };
    for (template, expanded) in [
        ("{+a}", "x/y"),
        ("{#a}", "#x/y"),
        ("{.a,a}", ".x%2Fy.x%2Fy"),
        ("{/a}", "/x%2Fy"),
        ("{;a,b}", ";a=x%2Fy;b="),
        ("{&a}", "&a=x%2Fy"),
        ("{a:3}", "x%2Fy"),
        ("plain", "plain"),
    ] {
        assert_eq!(expand_template(template, lookup), Ok(expanded.to_owned()));
    }
    assert_eq!(
        template_fields("{a}{+a}{b*}{}"),
        Ok(vec!["a".to_owned(), "b".to_owned()])
    );
}

#[test]
fn malformed_templates_and_missing_values_are_refused() {
    assert_eq!(
        template_fields("custom://{open"),
        Err("McpResourceTemplateInvalid")
    );
    assert_eq!(
        expand_template("custom://{}", |_| Some("x")),
        Err("McpResourceTemplateInvalid")
    );
    assert_eq!(
        expand_template("custom://{open", |_| Some("x")),
        Err("McpResourceTemplateInvalid")
    );
    assert_eq!(
        expand_template("custom://{gone}", |_| None),
        Err("McpResourceTemplateArgumentMissing")
    );
}
