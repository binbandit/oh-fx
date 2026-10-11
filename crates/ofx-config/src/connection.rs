use std::fmt;
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

use crate::configured_provider::{
    MaxTokensParameter, ProviderAuth, ProviderDefinition, ToolChoiceMode,
};
use crate::header_template::InterpolationError;

pub(crate) const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
pub(crate) const REDACTED: &str = "[redacted]";

#[derive(Clone, PartialEq, Eq)]
pub struct ResolvedConnection {
    pub id: String,
    pub chat_url: String,
    pub bearer_token: Option<String>,
    pub headers: Vec<(String, String)>,
    pub secrets: Vec<String>,
    pub tool_choice_mode: ToolChoiceMode,
    pub max_tokens_parameter: MaxTokensParameter,
    pub ca_file: Option<PathBuf>,
    pub proxy: Option<String>,
}

impl fmt::Debug for ResolvedConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let header_names: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(name, _)| (name.as_str(), REDACTED))
            .collect();
        formatter
            .debug_struct("ResolvedConnection")
            .field("id", &self.id)
            .field("chat_url", &self.chat_url)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| REDACTED),
            )
            .field("headers", &header_names)
            .field("secrets", &self.secrets.len())
            .field("tool_choice_mode", &self.tool_choice_mode)
            .field("max_tokens_parameter", &self.max_tokens_parameter)
            .field("ca_file", &self.ca_file)
            .field("proxy", &self.proxy.as_ref().map(|_| REDACTED))
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionError {
    #[error(
        "The configured provider credential is unavailable. Check its auth environment variable in settings.json; no other provider was selected."
    )]
    MissingCredentials,
    #[error(
        "the configured provider credential is not a valid bearer token; it must be at most 16 KiB of visible ASCII characters"
    )]
    InvalidCredential,
    #[error(
        "header {header} needs the environment variable {variable}, which is not set; export it or give a default with ${{{variable}:-value}}"
    )]
    MissingHeaderVariable { header: String, variable: String },
    #[error("header {header} resolved to a value that is not valid in an HTTP header")]
    InvalidHeaderValue { header: String },
    #[error("tls.ca_file starts with ~/ but HOME is not set")]
    HomeUnavailable,
}

impl ConnectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingCredentials | Self::MissingHeaderVariable { .. } => "MissingCredentials",
            Self::InvalidCredential => "InvalidConfiguredProviderCredential",
            Self::InvalidHeaderValue { .. } => "InvalidHeaderValue",
            Self::HomeUnavailable => "HomeNotSet",
        }
    }
}

impl ProviderDefinition {
    pub fn resolve(
        &self,
        lookup: &dyn Fn(&str) -> Option<String>,
        home: Option<&Path>,
    ) -> Result<ResolvedConnection, ConnectionError> {
        let mut secrets = Vec::new();
        let bearer_token = match &self.auth {
            ProviderAuth::None => None,
            ProviderAuth::Bearer { env } => {
                let token = lookup(env)
                    .filter(|token| !token.trim().is_empty())
                    .ok_or(ConnectionError::MissingCredentials)?;
                if token.len() > MAX_CREDENTIAL_BYTES
                    || !token.bytes().all(|byte| byte.is_ascii_graphic())
                {
                    return Err(ConnectionError::InvalidCredential);
                }
                secrets.push(token.clone());
                Some(token)
            }
        };
        let headers = self
            .headers
            .iter()
            .map(|header| {
                let name = header.name().to_owned();
                header
                    .interpolate(lookup, &mut secrets)
                    .map(|value| (name.clone(), value))
                    .map_err(|error| match error {
                        InterpolationError::MissingVariable(variable) => {
                            ConnectionError::MissingHeaderVariable {
                                header: name,
                                variable,
                            }
                        }
                        InterpolationError::InvalidValue => {
                            ConnectionError::InvalidHeaderValue { header: name }
                        }
                    })
            })
            .collect::<Result<_, _>>()?;
        let ca_file = self
            .ca_file
            .as_deref()
            .map(|path| expand_home(path, home))
            .transpose()?;
        if let Some(proxy) = &self.proxy {
            secrets.extend(proxy_credentials(proxy));
        }
        let mut unique_secrets: Vec<String> = Vec::with_capacity(secrets.len());
        for secret in secrets {
            if !unique_secrets.contains(&secret) {
                unique_secrets.push(secret);
            }
        }
        Ok(ResolvedConnection {
            id: self.id().to_owned(),
            chat_url: self.chat_url(),
            bearer_token,
            headers,
            secrets: unique_secrets,
            tool_choice_mode: self.tool_choice_mode,
            max_tokens_parameter: self.max_tokens_parameter,
            ca_file,
            proxy: self.proxy.clone(),
        })
    }
}

fn proxy_credentials(proxy: &str) -> Vec<String> {
    let Some((_, rest)) = proxy.split_once("://") else {
        return Vec::new();
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let Some((userinfo, _)) = authority.rsplit_once('@') else {
        return Vec::new();
    };
    let mut credentials = vec![userinfo.to_owned()];
    if let Some((_, password)) = userinfo.split_once(':')
        && !password.is_empty()
    {
        credentials.push(password.to_owned());
    }
    let decoded: Vec<String> = credentials
        .iter()
        .map(|credential| {
            percent_decode_str(credential)
                .decode_utf8_lossy()
                .into_owned()
        })
        .filter(|credential| !credentials.contains(credential))
        .collect();
    credentials.extend(decoded);
    credentials
}

fn expand_home(path: &str, home: Option<&Path>) -> Result<PathBuf, ConnectionError> {
    match path.strip_prefix("~/") {
        Some(relative) => home
            .map(|home| home.join(relative))
            .ok_or(ConnectionError::HomeUnavailable),
        None => Ok(PathBuf::from(path)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configured_provider::ProviderRegistry;

    #[test]
    fn debug_output_redacts_credentials_headers_and_proxy() {
        let connection = ResolvedConnection {
            id: "portkey".to_owned(),
            chat_url: "https://gateway.example/v1/chat/completions".to_owned(),
            bearer_token: Some("sk-live-token".to_owned()),
            headers: vec![("x-portkey-api-key".to_owned(), "pk-live-secret".to_owned())],
            secrets: vec!["pk-live-secret".to_owned(), "sk-live-token".to_owned()],
            tool_choice_mode: ToolChoiceMode::default(),
            max_tokens_parameter: MaxTokensParameter::default(),
            ca_file: None,
            proxy: Some("http://user:proxy-password@proxy.example:8080".to_owned()),
        };
        let rendered = format!("{connection:?}");
        for secret in ["sk-live-token", "pk-live-secret", "proxy-password"] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
        assert!(rendered.contains("x-portkey-api-key"), "{rendered}");
    }

    const PORTKEY: &str = r#"{"portkey":{"protocol":"openai-chat-completions","base_url":"https://portkey.internal.example.com/v1","auth":{"type":"none"},"headers":{"x-portkey-api-key":"${PORTKEY_API_KEY}","x-portkey-config":"${PORTKEY_CONFIG:-pc-default}"},"tls":{"ca_file":"~/certs/corp.pem"},"proxy":"http://proxy.corp:3128"},
"router":{"protocol":"openai-chat-completions","base_url":"https://openrouter.ai/api/v1","auth":{"type":"bearer","env":"OPENROUTER_API_KEY"}}}"#;

    fn registry() -> ProviderRegistry {
        ProviderRegistry::parse_json(PORTKEY.as_bytes()).unwrap()
    }

    #[test]
    fn resolves_portkey_headers_paths_and_secrets() {
        let registry = registry();
        let lookup = |name: &str| (name == "PORTKEY_API_KEY").then(|| "pk-123".to_owned());
        let resolved = registry
            .get("portkey")
            .unwrap()
            .resolve(&lookup, Some(Path::new("/home/ada")))
            .unwrap();
        assert_eq!(
            resolved.chat_url,
            "https://portkey.internal.example.com/v1/chat/completions"
        );
        assert_eq!(resolved.bearer_token, None);
        assert_eq!(
            resolved.headers,
            [
                ("x-portkey-api-key".to_owned(), "pk-123".to_owned()),
                ("x-portkey-config".to_owned(), "pc-default".to_owned()),
            ]
        );
        assert_eq!(resolved.secrets, ["pk-123"]);
        assert_eq!(resolved.max_tokens_parameter, MaxTokensParameter::MaxTokens);
        assert_eq!(
            resolved.ca_file.as_deref(),
            Some(Path::new("/home/ada/certs/corp.pem"))
        );
        assert_eq!(resolved.proxy.as_deref(), Some("http://proxy.corp:3128"));
    }

    #[test]
    fn missing_header_variables_are_named_without_values() {
        let registry = registry();
        let error = registry
            .get("portkey")
            .unwrap()
            .resolve(&|_| None, Some(Path::new("/home/ada")))
            .unwrap_err();
        assert_eq!(
            error,
            ConnectionError::MissingHeaderVariable {
                header: "x-portkey-api-key".to_owned(),
                variable: "PORTKEY_API_KEY".to_owned(),
            }
        );
        assert_eq!(
            error.to_string(),
            "header x-portkey-api-key needs the environment variable PORTKEY_API_KEY, which is not set; export it or give a default with ${PORTKEY_API_KEY:-value}"
        );
        let lookup = |_: &str| Some("pk-123".to_owned());
        assert_eq!(
            registry.get("portkey").unwrap().resolve(&lookup, None),
            Err(ConnectionError::HomeUnavailable)
        );
    }

    #[test]
    fn bearer_connections_require_a_non_blank_token() {
        let registry = registry();
        let router = registry.get("router").unwrap();
        assert_eq!(
            router.resolve(&|_| Some("  ".to_owned()), None),
            Err(ConnectionError::MissingCredentials)
        );
        let resolved = router
            .resolve(&|_| Some("sk-or-1".to_owned()), None)
            .unwrap();
        assert_eq!(resolved.bearer_token.as_deref(), Some("sk-or-1"));
        assert_eq!(resolved.secrets, ["sk-or-1"]);
        for invalid in [
            "sk or 1".to_owned(),
            "sk-é".to_owned(),
            "k".repeat(MAX_CREDENTIAL_BYTES + 1),
        ] {
            let error = router
                .resolve(&|_| Some(invalid.clone()), None)
                .unwrap_err();
            assert_eq!(error, ConnectionError::InvalidCredential);
            assert_eq!(error.code(), "InvalidConfiguredProviderCredential");
        }
    }

    #[test]
    fn proxy_credentials_join_the_masked_secrets() {
        let json = r#"{"corp":{"protocol":"openai-chat-completions","base_url":"https://gateway.example.com/v1","auth":{"type":"none"},"proxy":"http://svc-user:Pr0xy-Pass@proxy.corp:3128"}}"#;
        let registry = ProviderRegistry::parse_json(json.as_bytes()).unwrap();
        let resolved = registry
            .get("corp")
            .unwrap()
            .resolve(&|_| None, None)
            .unwrap();
        assert_eq!(resolved.secrets, ["svc-user:Pr0xy-Pass", "Pr0xy-Pass"]);
        assert_eq!(
            proxy_credentials("http://proxy.corp:3128"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn proxy_credentials_include_the_decoded_forms_the_client_sends() {
        assert_eq!(
            proxy_credentials("http://svc%2Duser:p%40ss%3Aword@proxy.corp:3128"),
            [
                "svc%2Duser:p%40ss%3Aword",
                "p%40ss%3Aword",
                "svc-user:p@ss:word",
                "p@ss:word",
            ]
        );
        assert_eq!(
            proxy_credentials("http://user:bad%zzpass@proxy.corp:3128"),
            ["user:bad%zzpass", "bad%zzpass"]
        );
        assert_eq!(
            proxy_credentials("http://user:bad%41%zz@proxy.corp:3128"),
            ["user:bad%41%zz", "bad%41%zz", "user:badA%zz", "badA%zz"]
        );
    }
}
