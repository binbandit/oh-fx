use ofx_text::is_model_safe_text;

pub(crate) const MAX_CONVERTED_CONTENT_BYTES: usize = 10 * 1024 * 1024;
const OCTET_STREAM: &str = "application/octet-stream";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Text,
    Html,
    Binary,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Binary => "binary",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Classification {
    pub(crate) kind: Kind,
    pub(crate) mime_type: String,
}

pub(crate) fn classify(content_type: Option<&str>, body: &[u8]) -> Classification {
    if let Some(declared) = content_type {
        let mime_type = normalized_mime(declared);
        return Classification {
            kind: declared_kind(&mime_type),
            mime_type,
        };
    }
    if is_model_safe_text(body) {
        return Classification {
            kind: Kind::Text,
            mime_type: "text/plain".to_owned(),
        };
    }
    Classification {
        kind: Kind::Binary,
        mime_type: OCTET_STREAM.to_owned(),
    }
}

fn normalized_mime(content_type: &str) -> String {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches([' ', '\t', '\r', '\n']);
    if essence.is_empty() {
        return OCTET_STREAM.to_owned();
    }
    essence.to_ascii_lowercase()
}

fn declared_kind(mime: &str) -> Kind {
    if mime == "text/html" || mime == "application/xhtml+xml" {
        return Kind::Html;
    }
    let structured =
        mime.starts_with("application/") && (mime.ends_with("+json") || mime.ends_with("+xml"));
    if mime.starts_with("text/")
        || structured
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
        )
    {
        return Kind::Text;
    }
    Kind::Binary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_declared_mime_and_missing_content_type_deterministically() {
        let cases: [(Option<&str>, &[u8], Kind, &str); 12] = [
            (
                Some("Text/HTML; Charset=UTF-8"),
                b"hello",
                Kind::Html,
                "text/html",
            ),
            (Some("TEXT/PLAIN"), b"hello", Kind::Text, "text/plain"),
            (
                Some("application/json; charset=utf-8"),
                b"hello",
                Kind::Text,
                "application/json",
            ),
            (
                Some("application/activity+json"),
                b"hello",
                Kind::Text,
                "application/activity+json",
            ),
            (
                Some("application/xml"),
                b"hello",
                Kind::Text,
                "application/xml",
            ),
            (
                Some("application/rss+xml"),
                b"hello",
                Kind::Text,
                "application/rss+xml",
            ),
            (
                Some("application/javascript"),
                b"hello",
                Kind::Text,
                "application/javascript",
            ),
            (
                Some("application/x-javascript"),
                b"hello",
                Kind::Text,
                "application/x-javascript",
            ),
            (
                Some("application/pdf"),
                b"hello",
                Kind::Binary,
                "application/pdf",
            ),
            (
                Some(" ; charset=utf-8"),
                b"hello",
                Kind::Binary,
                OCTET_STREAM,
            ),
            (None, b"hello", Kind::Text, "text/plain"),
            (None, b"bad\x00text", Kind::Binary, OCTET_STREAM),
        ];
        for (content_type, body, kind, mime_type) in cases {
            assert_eq!(
                classify(content_type, body),
                Classification {
                    kind,
                    mime_type: mime_type.to_owned()
                },
                "{content_type:?}"
            );
        }
    }
}
