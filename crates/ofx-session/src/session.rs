use ofx_text::{Script, dominant_script};

pub(crate) fn infer_conversation_language<'a>(text: &str, fallback: &'a str) -> &'a str {
    let Some(script) = dominant_script(text) else {
        return fallback;
    };
    match script {
        Script::Japanese => "ja",
        Script::Hangul => "ko",
        Script::Han => "und-Hani",
        Script::Arabic => "und-Arab",
        Script::Hebrew => "und-Hebr",
        Script::Cyrillic => "und-Cyrl",
        Script::Greek => "und-Grek",
        Script::Devanagari => "und-Deva",
        Script::Thai => "und-Thai",
        Script::Latin => "und-Latn",
    }
}

#[cfg(test)]
mod tests;
