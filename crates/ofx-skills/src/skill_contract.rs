mod metadata;

pub use metadata::{
    InvalidMetadataCause, MetadataStatus, ParsedSkillFile, SkillMetadata, parse_skill_file,
    resolve_metadata,
};

pub const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
pub const MAX_NAME_BYTES: usize = 256;
