use ofx_contract::{ToolArgValue, ToolOutput};

use super::url_policy::{PolicyError, ValidatedUrl, normalize};
use crate::tool_args::parse_arguments;

const TOOL_NAME: &str = "web_fetch";
const URL_WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

pub(crate) fn decode(arguments: &str) -> Result<String, ToolOutput> {
    let fields = parse_arguments(TOOL_NAME, arguments)?;
    if let Some(unknown) = fields.names().find(|name| *name != "url") {
        return Err(ToolOutput::failure(format!(
            "web_fetch field \"{unknown}\" is not allowed"
        )));
    }
    match fields.get("url") {
        None => Err(ToolOutput::failure("web_fetch field \"url\" is required")),
        Some(ToolArgValue::String(url)) => Ok(url.clone()),
        Some(_) => Err(ToolOutput::failure(
            "web_fetch field \"url\" must be a string",
        )),
    }
}

pub(crate) fn validate(url: &str) -> Result<ValidatedUrl, ToolOutput> {
    normalize(url.trim_matches(URL_WHITESPACE))
        .map_err(|error| ToolOutput::failure(validation_message(error)))
}

fn validation_message(error: PolicyError) -> &'static str {
    match error {
        PolicyError::EmptyUrl => "web_fetch field \"url\" must not be empty",
        PolicyError::UrlTooLong => "web_fetch field \"url\" must be at most 2000 bytes",
        PolicyError::UnsupportedScheme => "web_fetch url must start with http:// or https://",
        PolicyError::MissingHost => "web_fetch url must include a host",
        PolicyError::CredentialedUrl => "web_fetch refuses credential-bearing URLs",
        PolicyError::NonPublicAddress | PolicyError::SingleLabelHost => {
            "web_fetch only fetches known public HTTP(S) URLs"
        }
        _ => "web_fetch url is malformed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_failure(arguments: &str) -> String {
        decode(arguments).unwrap_err().content
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert_eq!(
            decode_failure("{"),
            "web_fetch arguments must be valid JSON"
        );
        assert_eq!(
            decode_failure("[]"),
            "web_fetch arguments must be an object"
        );
        assert_eq!(decode_failure("{}"), "web_fetch field \"url\" is required");
        assert_eq!(
            decode_failure("{\"url\":1}"),
            "web_fetch field \"url\" must be a string"
        );
    }

    #[test]
    fn rejects_repeated_fields_as_invalid_json_and_names_the_first_unknown_field() {
        assert_eq!(
            decode_failure(r#"{"url":"https://a.example/","url":"https://b.example/"}"#),
            "web_fetch arguments must be valid JSON"
        );
        assert_eq!(
            decode_failure(r#"{"url":"https://a.example/","zeta":1,"alpha":2}"#),
            "web_fetch field \"zeta\" is not allowed"
        );
        assert_eq!(
            decode_failure(r#"{"url":"https://a.example/","n":1e400}"#),
            "web_fetch field \"n\" is not allowed"
        );
    }

    #[test]
    fn requires_only_url_and_rejects_unknown_fields() {
        assert_eq!(
            decode_failure("{\"prompt\":\"extract\"}"),
            "web_fetch field \"prompt\" is not allowed"
        );
        assert_eq!(
            decode_failure("{\"url\":\"https://example.com\",\"prompt\":\"extract\"}"),
            "web_fetch field \"prompt\" is not allowed"
        );
        assert_eq!(
            decode_failure("{\"url\":\"https://example.com\",\"extra\":true}"),
            "web_fetch field \"extra\" is not allowed"
        );
        assert_eq!(
            decode("{\"url\":\" https://example.com \"}").unwrap(),
            " https://example.com "
        );
    }

    #[test]
    fn validates_known_public_http_urls_only() {
        let cases = [
            (" https://example.com/docs \n", None),
            ("http://example.com:8080/docs", None),
            ("https://example.com/docs", None),
            ("", Some("web_fetch field \"url\" must not be empty")),
            (
                "ftp://example.com",
                Some("web_fetch url must start with http:// or https://"),
            ),
            (
                "https://token@example.com/private",
                Some("web_fetch refuses credential-bearing URLs"),
            ),
            (
                "https://localhost:3000",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "https://127.0.0.1/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://192.168.1.10/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://[::1]/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://[fd00::1]/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://[fc00::1]/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://[::ffff:127.0.0.1]/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            (
                "http://[::ffff:192.168.1.10]/status",
                Some("web_fetch only fetches known public HTTP(S) URLs"),
            ),
            ("https:///docs", Some("web_fetch url must include a host")),
            (
                &"https://example.com/".repeat(120),
                Some("web_fetch field \"url\" must be at most 2000 bytes"),
            ),
            (
                "https://example.com/a b",
                Some("web_fetch url is malformed"),
            ),
            (
                "https://example.com:99999/",
                Some("web_fetch url is malformed"),
            ),
        ];
        for (url, expected) in cases {
            match (validate(url), expected) {
                (Ok(validated), None) => {
                    assert!(validated.retrieval_url.starts_with("https://"), "{url}");
                }
                (Err(failure), Some(message)) => assert_eq!(failure.content, message, "{url}"),
                (result, expected) => panic!("{url}: {result:?} vs {expected:?}"),
            }
        }
    }
}
