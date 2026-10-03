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

fn profile(script: Option<Script>, letters: usize, dominant_letters: usize) -> Profile {
    Profile {
        script,
        letters,
        dominant_letters,
    }
}

#[test]
fn paired_prose_profiles_count_every_script_and_the_non_latin_view_drops_latin() {
    assert_eq!(
        prose_profiles("abc中文"),
        ProseProfiles {
            all: profile(Some(Script::Latin), 5, 3),
            non_latin: profile(Some(Script::Han), 2, 2),
        }
    );
    assert_eq!(
        prose_profiles(""),
        ProseProfiles {
            all: profile(None, 0, 0),
            non_latin: profile(None, 0, 0),
        }
    );
    assert_eq!(
        prose_profiles("次にロックファイルを確認します。 English identifiers.").all,
        profile(Some(Script::Japanese), 33, 15)
    );
    assert_eq!(
        prose_profiles("한국어 응답 and English text.").non_latin,
        profile(Some(Script::Hangul), 5, 5)
    );
}

#[test]
fn language_script_prose_profile_excludes_code_identifiers_and_quoted_data() {
    let chinese_with_packages = "接下来我将检查项目中的锁文件（如 package-lock.json、Cargo.lock、Pipfile.lock 等）及其对应的清单文件。";
    assert_eq!(
        prose_profiles(chinese_with_packages).all.script,
        Some(Script::Han)
    );

    let english_with_quote = "The command failed with the quoted message ‘锁文件已损坏’, so I will inspect the lockfile.";
    assert_eq!(
        prose_profiles(english_with_quote).all.script,
        Some(Script::Latin)
    );

    let english_after_code =
        "```zig\nconst причина = true;\n```\nI will inspect the lockfile next.";
    assert_eq!(
        prose_profiles(english_after_code).all.script,
        Some(Script::Latin)
    );

    let chinese_with_identifiers = "我将检查 lockfile 和 dependency manifest，查找损坏问题。";
    let non_latin = prose_profiles(chinese_with_identifiers).non_latin;
    assert_eq!(non_latin.script, Some(Script::Han));
    assert!(non_latin.dominant_letters >= 8);
}

#[test]
fn prose_skips_code_spans_quotes_and_nested_delimiters() {
    let text = "English `日本語 code` (quoted 中文) \"Русский\" prose.";
    assert_eq!(
        prose_profiles(text).all,
        profile(Some(Script::Latin), 12, 12)
    );
    let nested = "English “中文” {日本語 [nested]} prose.";
    assert_eq!(
        prose_profiles(nested).all,
        profile(Some(Script::Latin), 12, 12)
    );
    assert_eq!(
        prose_profiles("（全角括号）中文").all,
        profile(Some(Script::Han), 2, 2)
    );
    assert_eq!(
        prose_profiles("【标签】「引用」中文").all,
        profile(Some(Script::Han), 2, 2)
    );
    assert_eq!(
        prose_profiles("an `unclosed span with 中文").all,
        profile(Some(Script::Latin), 2, 2)
    );
    assert_eq!(
        prose_profiles("`````x````` 中文").all,
        profile(Some(Script::Han), 2, 2)
    );
}

#[test]
fn whole_text_profiles_keep_the_session_language_counts() {
    assert_eq!(dominant_script("aя"), None);
    assert_eq!(prose_profiles("ab中文").all, profile(None, 4, 2));
}
