use super::*;

#[test]
fn language_script_profile_preserves_session_inference_semantics() {
    assert_eq!(dominant_script("a"), Some(Script::Latin));
    assert_eq!(dominant_script("資料を確認"), Some(Script::Japanese));
    assert_eq!(dominant_script("파일 A"), Some(Script::Hangul));
    assert_eq!(dominant_script("aя"), None);
    assert_eq!(dominant_script("1234"), None);
}

#[test]
fn each_script_wins_by_its_letter_count() {
    for (text, script) in [
        ("Сначала проверим файл. Then", Script::Cyrillic),
        ("افتح الصفحة الرئيسية", Script::Arabic),
        ("שלום עולם", Script::Hebrew),
        ("नमस्ते दुनिया", Script::Devanagari),
        ("สวัสดีชาวโลก", Script::Thai),
        ("Καλημέρα κόσμε", Script::Greek),
        ("接下来我将检查项目中的锁文件 lock", Script::Han),
        (
            "English `日本語 code` (quoted 中文) \"Русский\" prose.",
            Script::Latin,
        ),
        ("ｶﾀｶﾅ", Script::Japanese),
        ("ㄱㄴ", Script::Hangul),
        ("ÀÉÎõü ḀẞỲ", Script::Latin),
    ] {
        assert_eq!(dominant_script(text), Some(script), "{text}");
    }
}

#[test]
fn kana_and_hangul_outrank_any_other_count_and_ties_have_no_script() {
    assert_eq!(
        dominant_script("a long english sentence の"),
        Some(Script::Japanese)
    );
    assert_eq!(
        dominant_script("a long english sentence 한"),
        Some(Script::Hangul)
    );
    assert_eq!(dominant_script("ab中文"), None);
    assert_eq!(dominant_script(""), None);
}
