use ofx_text::lowercase_hex;
use sha2::{Digest, Sha256};

const CREDENTIAL_AUTHORITY_DOMAIN: &[u8] = b"fx-credential-authority-v1\0";
pub(crate) const CREDENTIAL_IDENTITY_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteCredential {
    source: &'static str,
    identity: Option<[u8; CREDENTIAL_IDENTITY_BYTES]>,
}

impl RouteCredential {
    pub fn configured() -> Self {
        Self::derived("configured", None)
    }

    pub fn chatgpt_subscription(account_id: &str) -> Self {
        Self::derived("chatgpt_subscription", Some(account_id))
    }

    pub(super) fn saved(
        source: &'static str,
        identity: Option<[u8; CREDENTIAL_IDENTITY_BYTES]>,
    ) -> Self {
        Self { source, identity }
    }

    pub(super) fn source(self) -> &'static str {
        self.source
    }

    pub(super) fn identity_hex(self) -> Option<String> {
        self.identity.map(|identity| lowercase_hex(&identity))
    }

    pub(super) fn identifies(self, current: Self) -> bool {
        self.identity.is_some() && self == current
    }

    fn derived(source: &'static str, account_id: Option<&str>) -> Self {
        let mut hash = Sha256::new();
        hash.update(CREDENTIAL_AUTHORITY_DOMAIN);
        hash.update(source);
        match account_id {
            None => hash.update(b"\0slot\0"),
            Some("") => return Self::saved(source, None),
            Some(account_id) => {
                hash.update(b"\0account\0");
                hash.update(account_id);
            }
        }
        let mut identity = [0_u8; CREDENTIAL_IDENTITY_BYTES];
        identity.copy_from_slice(&hash.finalize());
        Self::saved(source, Some(identity))
    }
}
