mod archive;
mod auto;
mod build_identity;
mod error;
mod lock;
mod release_source;
mod upgrade;

pub use auto::{claim_auto_upgrade_check, version_change_since_last_run};
pub use build_identity::VERSION;
pub use error::UpgradeError;
pub use lock::UpgradeLock;
pub use release_source::release_notes_url;
pub use upgrade::{UpgradeOutcome, UpgradeProgress, upgrade};
