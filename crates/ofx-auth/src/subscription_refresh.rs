use ofx_contract::parse_strict_json_value;
use serde_json::Value;

use crate::oauth;
use crate::secret::Secret;

#[derive(Debug)]
pub(crate) struct RefreshTokenResponse {
    pub(crate) access_token: Secret,
    pub(crate) refresh_token: Option<Secret>,
    pub(crate) expires_in: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParseRefreshError {
    InvalidJson,
    InvalidShape,
}

pub(crate) fn parse_refresh_token_response(
    bytes: &[u8],
) -> Result<RefreshTokenResponse, ParseRefreshError> {
    let value = parse_strict_json_value(bytes).map_err(|_| ParseRefreshError::InvalidJson)?;
    let Value::Object(object) = value else {
        return Err(ParseRefreshError::InvalidShape);
    };
    let access_token = oauth::required_string(&object, "access_token")
        .map_err(|_| ParseRefreshError::InvalidShape)?;
    let refresh_token = match object.get("refresh_token") {
        None => None,
        Some(Value::String(token)) if !token.is_empty() => Some(Secret::new(token.clone())),
        Some(_) => return Err(ParseRefreshError::InvalidShape),
    };
    let expires_in = oauth::optional_positive_integer(&object, "expires_in")
        .map_err(|_| ParseRefreshError::InvalidShape)?;
    Ok(RefreshTokenResponse {
        access_token,
        refresh_token,
        expires_in,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_syntax_failure_from_invalid_token_shape() {
        for bytes in [b"not JSON".as_slice(), b"{", b"{\"access_token\":"] {
            assert_eq!(
                parse_refresh_token_response(bytes).expect_err("invalid syntax"),
                ParseRefreshError::InvalidJson
            );
        }
        for bytes in [
            b"[]".as_slice(),
            b"null",
            br#"{"access_token":""}"#,
            br#"{"access_token":"a","refresh_token":null}"#,
            br#"{"access_token":"a","expires_in":0}"#,
        ] {
            assert_eq!(
                parse_refresh_token_response(bytes).expect_err("invalid token shape"),
                ParseRefreshError::InvalidShape
            );
        }
        let token = parse_refresh_token_response(br#"{"access_token":"a"}"#)
            .expect("optional refresh fields");
        assert_eq!(token.access_token.expose(), "a");
        assert!(token.refresh_token.is_none());
        assert!(token.expires_in.is_none());
    }
}
