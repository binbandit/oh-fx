use ofx_jsonrpc::RpcError;
use serde_json::{Map, Value};

use crate::error::McpError;
use crate::mcp_contract::{EnvVar, validate_json_rpc_response_envelope};

pub(crate) const LEGACY_PROTOCOL_VERSION: &str = "2024-11-05";
pub(crate) const LEGACY_2025_03_PROTOCOL_VERSION: &str = "2025-03-26";
pub(crate) const LEGACY_2025_06_PROTOCOL_VERSION: &str = "2025-06-18";
pub(crate) const LEGACY_2025_11_PROTOCOL_VERSION: &str = "2025-11-25";
pub(crate) const PROTOCOL_VERSION_ENVIRONMENT: &str = "OH_FX_MCP_PROTOCOL_VERSION";
pub(crate) const UNSUPPORTED_PROTOCOL_VERSION_CODE: i64 = -32_022;
const INVALID_PARAMS_CODE: i64 = -32_602;

pub(crate) fn validate_startup_mode(
    server_environment: &[EnvVar],
    inherited_version: Option<&str>,
) -> Result<(), McpError> {
    let requested = server_environment
        .iter()
        .find(|entry| entry.key == PROTOCOL_VERSION_ENVIRONMENT)
        .map(|entry| entry.value.as_str())
        .or(inherited_version);
    match requested {
        None | Some(LEGACY_2025_11_PROTOCOL_VERSION) => Ok(()),
        Some(_) => Err(McpError::McpUnsupportedProtocolVersion),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElicitationWire {
    LegacyMcp2025_06,
    LegacyMcp2025_11,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum LegacyStdioVersion {
    V2024_11_05,
    V2025_03_26,
    V2025_06_18,
    V2025_11_25,
}

impl LegacyStdioVersion {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::V2025_11_25 => LEGACY_2025_11_PROTOCOL_VERSION,
            Self::V2025_06_18 => LEGACY_2025_06_PROTOCOL_VERSION,
            Self::V2025_03_26 => LEGACY_2025_03_PROTOCOL_VERSION,
            Self::V2024_11_05 => LEGACY_PROTOCOL_VERSION,
        }
    }

    pub(crate) fn wire(self) -> Option<ElicitationWire> {
        match self {
            Self::V2025_11_25 => Some(ElicitationWire::LegacyMcp2025_11),
            Self::V2025_06_18 => Some(ElicitationWire::LegacyMcp2025_06),
            Self::V2025_03_26 | Self::V2024_11_05 => None,
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            LEGACY_2025_11_PROTOCOL_VERSION => Some(Self::V2025_11_25),
            LEGACY_2025_06_PROTOCOL_VERSION => Some(Self::V2025_06_18),
            LEGACY_2025_03_PROTOCOL_VERSION => Some(Self::V2025_03_26),
            LEGACY_PROTOCOL_VERSION => Some(Self::V2024_11_05),
            _ => None,
        }
    }

    pub(crate) fn older(self) -> Option<Self> {
        match self {
            Self::V2025_11_25 => Some(Self::V2025_06_18),
            Self::V2025_06_18 => Some(Self::V2025_03_26),
            Self::V2025_03_26 => Some(Self::V2024_11_05),
            Self::V2024_11_05 => None,
        }
    }

    const NEWEST_FIRST: [Self; 4] = [
        Self::V2025_11_25,
        Self::V2025_06_18,
        Self::V2025_03_26,
        Self::V2024_11_05,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyInitializeObservation {
    Accepted(LegacyStdioVersion),
    Unsupported(Option<LegacyStdioVersion>),
    ConnectionClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyInitializeTransition {
    Accept(LegacyStdioVersion),
    Retry(LegacyStdioVersion),
    Fail,
}

pub(crate) fn decide_legacy_initialize_transition(
    offered: LegacyStdioVersion,
    observation: LegacyInitializeObservation,
) -> LegacyInitializeTransition {
    let retry_older = || {
        offered.older().map_or(
            LegacyInitializeTransition::Fail,
            LegacyInitializeTransition::Retry,
        )
    };
    match observation {
        LegacyInitializeObservation::Accepted(negotiated) => {
            LegacyInitializeTransition::Accept(negotiated)
        }
        LegacyInitializeObservation::ConnectionClosed
        | LegacyInitializeObservation::Unsupported(None) => retry_older(),
        LegacyInitializeObservation::Unsupported(Some(version)) if version < offered => {
            LegacyInitializeTransition::Retry(version)
        }
        LegacyInitializeObservation::Unsupported(Some(_)) => LegacyInitializeTransition::Fail,
    }
}

pub(crate) enum ResponsePayload<'a> {
    Complete(&'a Map<String, Value>),
    ProtocolError(RpcError),
}

pub(crate) fn classify_response_payload(value: &Value) -> Result<ResponsePayload<'_>, McpError> {
    validate_json_rpc_response_envelope(value)?;
    let object = value.as_object().ok_or(McpError::McpInvalidJson)?;
    if let Some(error) = object.get("error") {
        let error = error.as_object().ok_or(McpError::McpInvalidProtocolError)?;
        let code = error
            .get("code")
            .and_then(Value::as_i64)
            .ok_or(McpError::McpInvalidProtocolError)?;
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .ok_or(McpError::McpInvalidProtocolError)?;
        return Ok(ResponsePayload::ProtocolError(RpcError {
            code,
            message: message.to_owned(),
            data: error.get("data").cloned(),
        }));
    }
    let result = object.get("result").ok_or(McpError::McpNoResult)?;
    result
        .as_object()
        .map(ResponsePayload::Complete)
        .ok_or(McpError::McpInvalidResult)
}

pub(crate) fn classify_legacy_initialize_response(
    value: &Value,
    offered: LegacyStdioVersion,
) -> Result<LegacyInitializeObservation, McpError> {
    let result = match classify_response_payload(value)? {
        ResponsePayload::Complete(result) => result,
        ResponsePayload::ProtocolError(error)
            if error.code == UNSUPPORTED_PROTOCOL_VERSION_CODE =>
        {
            return Ok(LegacyInitializeObservation::Unsupported(
                select_legacy_version_for_request(&error, offered.as_str()),
            ));
        }
        ResponsePayload::ProtocolError(error) if error.code == INVALID_PARAMS_CODE => {
            return select_legacy_version_for_request(&error, offered.as_str())
                .map(|version| LegacyInitializeObservation::Unsupported(Some(version)))
                .ok_or(McpError::McpInitFailed);
        }
        ResponsePayload::ProtocolError(_) => return Err(McpError::McpInitFailed),
    };
    result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(LegacyStdioVersion::parse)
        .map(LegacyInitializeObservation::Accepted)
        .ok_or(McpError::McpUnsupportedProtocolVersion)
}

fn select_legacy_version_for_request(
    error: &RpcError,
    requested_version: &str,
) -> Option<LegacyStdioVersion> {
    LegacyStdioVersion::NEWEST_FIRST
        .into_iter()
        .find(|version| {
            protocol_error_supports_version_for_request(error, requested_version, version.as_str())
        })
}

fn protocol_error_supports_version_for_request(
    error: &RpcError,
    requested_version: &str,
    version: &str,
) -> bool {
    let Some(data) = error.data.as_ref().and_then(Value::as_object) else {
        return false;
    };
    let (Some(Value::Array(supported)), Some(Value::String(requested))) =
        (data.get("supported"), data.get("requested"))
    else {
        return false;
    };
    if requested != requested_version {
        return false;
    }
    let mut found = false;
    for supported_version in supported {
        let Some(supported_version) = supported_version.as_str() else {
            return false;
        };
        found |= supported_version == version;
    }
    found
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use LegacyInitializeObservation as Observation;
    use LegacyInitializeTransition as Transition;
    use LegacyStdioVersion as Version;

    #[test]
    fn startup_mode_accepts_only_the_legacy_lifecycle() {
        assert_eq!(validate_startup_mode(&[], None), Ok(()));
        let pinned = [EnvVar {
            key: PROTOCOL_VERSION_ENVIRONMENT.to_owned(),
            value: LEGACY_2025_11_PROTOCOL_VERSION.to_owned(),
        }];
        assert_eq!(validate_startup_mode(&pinned, Some("2026-07-28")), Ok(()));
        assert_eq!(
            validate_startup_mode(&[], Some("unknown")),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
        assert_eq!(
            validate_startup_mode(&[], Some("2026-07-28")),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
    }

    #[test]
    fn legacy_initialization_transitions_are_bounded_and_monotonic() {
        let cases = [
            (
                Version::V2025_11_25,
                Observation::Accepted(Version::V2025_11_25),
                Transition::Accept(Version::V2025_11_25),
            ),
            (
                Version::V2025_11_25,
                Observation::Accepted(Version::V2024_11_05),
                Transition::Accept(Version::V2024_11_05),
            ),
            (
                Version::V2025_06_18,
                Observation::Accepted(Version::V2025_11_25),
                Transition::Accept(Version::V2025_11_25),
            ),
            (
                Version::V2025_03_26,
                Observation::Accepted(Version::V2025_03_26),
                Transition::Accept(Version::V2025_03_26),
            ),
            (
                Version::V2025_11_25,
                Observation::ConnectionClosed,
                Transition::Retry(Version::V2025_06_18),
            ),
            (
                Version::V2025_06_18,
                Observation::ConnectionClosed,
                Transition::Retry(Version::V2025_03_26),
            ),
            (
                Version::V2025_03_26,
                Observation::ConnectionClosed,
                Transition::Retry(Version::V2024_11_05),
            ),
            (
                Version::V2024_11_05,
                Observation::ConnectionClosed,
                Transition::Fail,
            ),
            (
                Version::V2025_11_25,
                Observation::Unsupported(None),
                Transition::Retry(Version::V2025_06_18),
            ),
            (
                Version::V2025_06_18,
                Observation::Unsupported(None),
                Transition::Retry(Version::V2025_03_26),
            ),
            (
                Version::V2025_03_26,
                Observation::Unsupported(None),
                Transition::Retry(Version::V2024_11_05),
            ),
            (
                Version::V2024_11_05,
                Observation::Unsupported(None),
                Transition::Fail,
            ),
            (
                Version::V2025_11_25,
                Observation::Unsupported(Some(Version::V2024_11_05)),
                Transition::Retry(Version::V2024_11_05),
            ),
            (
                Version::V2025_11_25,
                Observation::Unsupported(Some(Version::V2025_11_25)),
                Transition::Fail,
            ),
            (
                Version::V2025_06_18,
                Observation::Unsupported(Some(Version::V2025_11_25)),
                Transition::Fail,
            ),
        ];
        for (offered, observation, expected) in cases {
            assert_eq!(
                decide_legacy_initialize_transition(offered, observation),
                expected,
                "{offered:?} {observation:?}"
            );
        }
    }

    #[test]
    fn mcp_legacy_stdio_negotiation_includes_2025_03_26() {
        assert_eq!(Version::parse("2025-03-26"), Some(Version::V2025_03_26));
        assert_eq!(Version::V2025_06_18.older(), Some(Version::V2025_03_26));
        assert_eq!(Version::V2025_03_26.older(), Some(Version::V2024_11_05));
    }

    #[test]
    fn legacy_initialize_responses_select_hinted_or_reported_versions() {
        let accepted = json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"2025-06-18"}});
        assert_eq!(
            classify_legacy_initialize_response(&accepted, Version::V2025_11_25),
            Ok(Observation::Accepted(Version::V2025_06_18))
        );
        let hinted = json!({"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"Unsupported protocol version","data":{"supported":["2024-11-05"],"requested":"2025-11-25"}}});
        assert_eq!(
            classify_legacy_initialize_response(&hinted, Version::V2025_11_25),
            Ok(Observation::Unsupported(Some(Version::V2024_11_05)))
        );
        let unhinted =
            json!({"jsonrpc":"2.0","id":0,"error":{"code":-32602,"message":"Invalid params"}});
        assert_eq!(
            classify_legacy_initialize_response(&unhinted, Version::V2025_11_25),
            Err(McpError::McpInitFailed)
        );
        let unsupported =
            json!({"jsonrpc":"2.0","id":0,"error":{"code":-32022,"message":"Unsupported"}});
        assert_eq!(
            classify_legacy_initialize_response(&unsupported, Version::V2025_11_25),
            Ok(Observation::Unsupported(None))
        );
        let unknown = json!({"jsonrpc":"2.0","id":0,"result":{"protocolVersion":"1999-01-01"}});
        assert_eq!(
            classify_legacy_initialize_response(&unknown, Version::V2025_11_25),
            Err(McpError::McpUnsupportedProtocolVersion)
        );
        let other = json!({"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"boom"}});
        assert_eq!(
            classify_legacy_initialize_response(&other, Version::V2025_11_25),
            Err(McpError::McpInitFailed)
        );
    }

    #[test]
    fn version_hints_must_name_the_offered_version() {
        let mismatched = json!({"jsonrpc":"2.0","id":0,"error":{"code":-32022,"message":"x","data":{"supported":["2024-11-05"],"requested":"2025-06-18"}}});
        assert_eq!(
            classify_legacy_initialize_response(&mismatched, Version::V2025_11_25),
            Ok(Observation::Unsupported(None))
        );
    }
}
