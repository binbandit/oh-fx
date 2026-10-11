use ofx_auth::FormBody;
use reqwest::StatusCode;

use super::{Credentials, post_form, token_endpoint_authentication};
use crate::error::McpError;

pub(crate) async fn revoke_credentials(
    http: &reqwest::Client,
    credentials: &Credentials,
) -> Result<(), McpError> {
    let endpoint = credentials
        .revocation_endpoint
        .as_deref()
        .ok_or(McpError::RevocationEndpointMissing)?;
    let refresh = match &credentials.refresh_token {
        Some(token) => revoke_token(http, credentials, endpoint, token, "refresh_token").await,
        None => Ok(()),
    };
    let access = revoke_token(
        http,
        credentials,
        endpoint,
        &credentials.access_token,
        "access_token",
    )
    .await;
    refresh.and(access)
}

async fn revoke_token(
    http: &reqwest::Client,
    credentials: &Credentials,
    endpoint: &str,
    token: &str,
    hint: &str,
) -> Result<(), McpError> {
    let mut form = FormBody::default();
    form.append("token", token);
    form.append("token_type_hint", hint);
    let authorization = token_endpoint_authentication(
        &mut form,
        &credentials.token_endpoint_auth_method,
        &credentials.client_id,
        credentials.client_secret.as_deref().map(String::as_str),
    )?;
    let response = post_form(http, endpoint, &form, authorization.as_ref()).await?;
    if response.status == StatusCode::OK {
        Ok(())
    } else {
        Err(McpError::TokenRevocationFailed)
    }
}

#[cfg(test)]
mod tests;
