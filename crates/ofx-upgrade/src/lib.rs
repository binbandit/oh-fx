mod archive;
mod auto;
mod build_identity;
mod error;
mod lock;
mod release_source;
mod update_target;
mod upgrade;

pub use auto::{
    BACKGROUND_UPGRADE_ARGS, schedule_background_upgrade, version_change_since_last_run,
};
pub use build_identity::VERSION;
pub use error::UpgradeError;
pub use lock::UpgradeLock;
pub use release_source::release_notes_url;
pub use update_target::{is_valid_revision, is_valid_version, normalize_version};
pub use upgrade::{UpgradeOutcome, UpgradeProgress, upgrade};
