use super::*;

#[test]
fn conversation_language_inference_matches_script_signals() {
    assert_eq!(
        infer_conversation_language("puoi aprire la landing page", "und"),
        "und-Latn"
    );
    assert_eq!(
        infer_conversation_language("ランディングページを開いて", "und"),
        "ja"
    );
    assert_eq!(
        infer_conversation_language("افتح الصفحة الرئيسية", "und"),
        "und-Arab"
    );
    assert_eq!(
        infer_conversation_language("12345 !!!", "und-Latn"),
        "und-Latn"
    );
    for (text, tag) in [
        ("파일을 열어", "ko"),
        ("打开页面", "und-Hani"),
        ("פתח את הדף", "und-Hebr"),
        ("Открой страницу", "und-Cyrl"),
        ("Άνοιξε τη σελίδα", "und-Grek"),
        ("पेज खोलो", "und-Deva"),
        ("เปิดหน้า", "und-Thai"),
    ] {
        assert_eq!(infer_conversation_language(text, "und"), tag, "{text}");
    }
}
