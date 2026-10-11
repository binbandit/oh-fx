use ofx_auth::{GatewayKey, GatewaySource, valid_gateway_team};
use ofx_gateway::{CatalogAccess, PublicOnlyReason};

pub fn gateway_catalog_access(
    key: Option<&GatewayKey>,
    explicit: bool,
    now_ms: i64,
) -> CatalogAccess {
    let Some(key) = key else {
        return CatalogAccess::PublicOnly(PublicOnlyReason::NoCredential);
    };
    let login = key.source == GatewaySource::FxLogin;
    if login && key.needs_refresh_at(now_ms) {
        return CatalogAccess::PublicOnly(PublicOnlyReason::LoginRefreshRequired);
    }
    let login_team = match key.gateway_team() {
        Some(team) if login && valid_gateway_team(team) => Some(team.to_owned()),
        _ if login => return CatalogAccess::PublicOnly(PublicOnlyReason::LoginTeamRequired),
        _ => None,
    };
    CatalogAccess::Authenticated {
        source: key.source.name(),
        credential: key.secret.clone(),
        login_team,
        explicit,
    }
}

#[cfg(test)]
mod tests;
