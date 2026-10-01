use ofx_contract::ProviderError;

const UNAUTHORIZED: u16 = 401;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFailure<'a> {
    pub message: String,
    pub unauthorized: bool,
    pub explanation: Option<&'a str>,
}

pub fn http_failure<'a>(error: &'a ProviderError, source: &str) -> Option<HttpFailure<'a>> {
    let status = error.status?;
    let bare = format!("HTTP {status}");
    let detail = error.detail.as_deref().filter(|detail| *detail != bare);
    if status == UNAUTHORIZED {
        return Some(HttpFailure {
            message: format!("{source} authentication failed · {bare}"),
            unauthorized: true,
            explanation: detail,
        });
    }
    Some(HttpFailure {
        message: detail.map_or(bare, str::to_owned),
        unauthorized: false,
        explanation: None,
    })
}

#[cfg(test)]
mod tests {
    use ofx_contract::ProviderErrorKind;

    use super::*;

    const SOURCE: &str = "configured provider";

    fn http_error(status: u16, detail: Option<&str>) -> ProviderError {
        let mut error = ProviderError::new(ProviderErrorKind::ServerError, "ServerError");
        error.status = Some(status);
        error.detail = detail.map(str::to_owned);
        error
    }

    #[test]
    fn unauthorized_answers_name_their_source_and_keep_the_gateway_explanation() {
        let error = http_error(401, Some("API request failed · HTTP 401 · bad key"));
        assert_eq!(
            http_failure(&error, "configured provider"),
            Some(HttpFailure {
                message: "configured provider authentication failed · HTTP 401".to_owned(),
                unauthorized: true,
                explanation: Some("API request failed · HTTP 401 · bad key"),
            })
        );
        assert_eq!(
            http_failure(&http_error(401, Some("HTTP 401")), "Codex subscription"),
            Some(HttpFailure {
                message: "Codex subscription authentication failed · HTTP 401".to_owned(),
                unauthorized: true,
                explanation: None,
            })
        );
    }

    #[test]
    fn other_http_answers_show_the_formatted_detail_or_the_bare_status() {
        let error = http_error(
            400,
            Some("API request failed · HTTP 400 · invalid_request_error: bad request body"),
        );
        assert_eq!(
            http_failure(&error, SOURCE).unwrap().message,
            "API request failed · HTTP 400 · invalid_request_error: bad request body"
        );
        let unavailable = http_error(503, None);
        let bare = http_failure(&unavailable, SOURCE).unwrap();
        assert_eq!(bare.message, "HTTP 503");
        assert!(!bare.unauthorized);
    }

    #[test]
    fn transport_failures_have_no_http_failure() {
        let error = ProviderError::new(ProviderErrorKind::ConnectionFailed, "ConnectionFailed");
        assert_eq!(http_failure(&error, SOURCE), None);
    }
}
