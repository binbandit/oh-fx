mod cursor;

use crate::Skill;
use ofx_contract::prepare_model_output;
use ofx_text::{LexicalDocument, PreparedQuery, is_model_safe_text, rank_intent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SkillSearchError {
    #[error("SkillSearchResultLimitTooSmall")]
    ResultLimitTooSmall,
}

#[derive(Debug)]
pub struct SkillSearchResult {
    pub items_json: String,
    pub count: usize,
    pub total_matches: usize,
}

pub fn search_skills(
    query: &PreparedQuery,
    skills: &[Skill],
    description_bytes: usize,
    max_bytes: usize,
) -> Result<SkillSearchResult, SkillSearchError> {
    let visible: Vec<_> = skills
        .iter()
        .filter(|skill| {
            is_model_safe_text(skill.name.as_bytes())
                && skill
                    .path
                    .to_str()
                    .is_some_and(|path| is_model_safe_text(path.as_bytes()))
        })
        .collect();
    let documents: Vec<_> = visible
        .iter()
        .map(|skill| LexicalDocument {
            identity: &skill.name,
            primary: &skill.name,
            secondary: &skill.description,
            stable_key: skill.path.to_str().unwrap_or_default(),
        })
        .collect();
    let matches = rank_intent(query, &documents);
    let total_matches = matches.len();
    let mut count = matches.len().min(5);
    loop {
        let items_json = matches[..count]
            .iter()
            .map(|index| {
                let skill = visible[*index];
                let description = &skill.description[..skill
                    .description
                    .floor_char_boundary(description_bytes.min(skill.description.len()))];
                format!(
                    "{{\"name\":{},\"description\":{},\"location\":{}}}",
                    quote(&skill.name),
                    quote(description),
                    quote(skill.path.to_str().unwrap_or_default())
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let next = if count < total_matches {
            quote(&cursor::render(query.raw(), &documents, count))
        } else {
            "null".to_owned()
        };
        let standalone = format!(
            "{{\"skills\":[{items_json}],\"count\":{count},\"total_matches\":{total_matches},\"more_available\":{},\"next_cursor\":{next}}}",
            count < total_matches
        );
        if prepare_model_output("capability_search", standalone.clone(), max_bytes) == standalone {
            return Ok(SkillSearchResult {
                items_json,
                count,
                total_matches,
            });
        }
        if count == 0 {
            return Err(SkillSearchError::ResultLimitTooSmall);
        }
        count -= 1;
    }
}

fn quote(value: &str) -> String {
    serde_json::Value::String(value.to_owned()).to_string()
}

#[cfg(test)]
mod tests;
