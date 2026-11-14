//! Text helpers: CJK bigram tokenizer for SQLite FTS5, safe MATCH queries,
//! subject normalization, preview/snippet trimming and e-mail normalization.

/// True for characters that are written without spaces between words.
pub fn is_cjk(character: char) -> bool {
    matches!(character as u32,
        0x2E80..=0x2EFF      // CJK Radicals Supplement
        | 0x3040..=0x30FF    // Hiragana + Katakana
        | 0x3400..=0x4DBF    // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
        | 0xAC00..=0xD7AF    // Hangul Syllables
        | 0xF900..=0xFAFF    // CJK Compatibility Ideographs
        | 0x20000..=0x2FA1F  // CJK Extension B..F + Compatibility Supplement
    )
}

fn push_unique(
    tokens: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
    token: String,
) {
    if !token.is_empty() && seen.insert(token.clone()) {
        tokens.push(token);
    }
}

/// Tokenize `text` into search tokens:
/// * CJK runs become overlapping bigrams (`你好世界` -> `你好 好世 世界`),
///   a single CJK character is kept as-is,
/// * Latin/digit runs are lowercased,
/// * everything else is a separator.
pub fn cjk_bigrams(text: &str) -> String {
    let mut tokens: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let mut word = String::new();

    let flush_cjk = |run: &mut Vec<char>,
                     tokens: &mut Vec<String>,
                     seen: &mut std::collections::HashSet<String>| {
        if run.is_empty() {
            return;
        }
        if run.len() == 1 {
            push_unique(tokens, seen, run[0].to_string());
        } else {
            for pair in run.windows(2) {
                push_unique(tokens, seen, pair.iter().collect());
            }
        }
        run.clear();
    };

    for character in text.chars() {
        if is_cjk(character) {
            if !word.is_empty() {
                push_unique(&mut tokens, &mut seen, std::mem::take(&mut word));
            }
            cjk_run.push(character);
        } else if character.is_alphanumeric() {
            flush_cjk(&mut cjk_run, &mut tokens, &mut seen);
            word.extend(character.to_lowercase());
        } else {
            flush_cjk(&mut cjk_run, &mut tokens, &mut seen);
            if !word.is_empty() {
                push_unique(&mut tokens, &mut seen, std::mem::take(&mut word));
            }
        }
    }
    flush_cjk(&mut cjk_run, &mut tokens, &mut seen);
    if !word.is_empty() {
        push_unique(&mut tokens, &mut seen, word);
    }
    tokens.join(" ")
}

fn tokenize_for_query(text: &str) -> Vec<String> {
    // Same tokenization as the index, but per whitespace-delimited segment and
    // capped so a pathological query cannot build a huge MATCH expression.
    let mut tokens = Vec::new();
    let normalized = cjk_bigrams(text);
    for token in normalized.split_whitespace() {
        tokens.push(token.to_string());
        if tokens.len() >= 64 {
            break;
        }
    }
    tokens
}

/// Build a safe FTS5 `MATCH` expression from arbitrary user input.
///
/// Every token is quoted (internal quotes doubled) and tokens are joined with
/// `AND`. Returns an empty string when the input has no searchable tokens.
pub fn fts_query(user_text: &str) -> String {
    let tokens = tokenize_for_query(user_text);
    tokens
        .into_iter()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Prefixes stripped by [`normalize_subject`] (lowercased, without the colon).
const SUBJECT_PREFIXES: &[&str] = &[
    "re", "fw", "fwd", "aw", "wg", "antw", "sv", "vs", "回复", "答复", "转发", "回覆", "轉發",
];

/// Remove `Re:`, `Fwd:`, `回复：`, `Re[2]:` … prefixes (repeatedly).
pub fn normalize_subject(subject: &str) -> String {
    let mut current = subject.trim().to_string();
    while let Some(colon_index) = current
        .char_indices()
        .find(|(_, character)| *character == ':' || *character == '：')
        .map(|(index, _)| index)
    {
        let candidate = &current[..colon_index];
        // Only consider short prefixes ("Re[12]" etc.).
        if candidate.chars().count() > 12 {
            break;
        }
        let mut base = candidate.trim().to_lowercase();
        // Drop a numeric counter: `re[2]`, `re(3)`.
        if let Some(open) = base.find(['[', '(']) {
            let close = if base[open..].starts_with('[') {
                ']'
            } else {
                ')'
            };
            if base.ends_with(close)
                && base[open + 1..base.len() - 1]
                    .chars()
                    .all(|character| character.is_ascii_digit())
            {
                base.truncate(open);
            }
        }
        base = base.trim().to_string();
        if !SUBJECT_PREFIXES.contains(&base.as_str()) {
            break;
        }
        current = current[colon_index..]
            .trim_start_matches([':', '：'])
            .trim_start()
            .to_string();
        if current.is_empty() {
            break;
        }
    }
    current
}

/// Collapse whitespace, strip control characters and truncate to `max_chars`.
pub fn trim_preview(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut cleaned = String::with_capacity(text.len().min(max_chars * 4));
    let mut last_was_space = false;
    for character in text.chars() {
        if character.is_whitespace() {
            if !last_was_space && !cleaned.is_empty() {
                cleaned.push(' ');
                last_was_space = true;
            }
            continue;
        }
        if character.is_control() {
            continue;
        }
        cleaned.push(character);
        last_was_space = false;
        if cleaned.chars().count() >= max_chars {
            cleaned.push('…');
            break;
        }
    }
    cleaned.trim_end().to_string()
}

/// Short excerpt around the first occurrence of `query`.
pub fn snippet(text: &str, query: &str, max_chars: usize) -> String {
    let collapsed = trim_preview(text, usize::MAX / 4);
    if collapsed.is_empty() {
        return String::new();
    }
    let needle = query.trim().to_lowercase();
    let haystack = collapsed.to_lowercase();
    let start_char = if needle.is_empty() {
        0
    } else {
        match haystack.find(&needle) {
            Some(byte_index) => collapsed[..byte_index]
                .chars()
                .count()
                .saturating_sub(max_chars / 4),
            None => 0,
        }
    };
    let excerpt: String = collapsed.chars().skip(start_char).take(max_chars).collect();
    let mut result = String::new();
    if start_char > 0 {
        result.push('…');
    }
    result.push_str(excerpt.trim());
    if start_char + max_chars < collapsed.chars().count() {
        result.push('…');
    }
    result
}

/// Lowercase and trim an e-mail address (also strips `mailto:` and angle brackets).
pub fn normalize_email(address: &str) -> String {
    let mut value = address.trim();
    if let Some(stripped) = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
    {
        value = stripped;
    }
    let value =
        value.trim_matches(|character| character == '<' || character == '>' || character == '"');
    value.trim().to_lowercase()
}

/// Domain part of an e-mail address (lowercase).
pub fn email_domain(address: &str) -> Option<String> {
    normalize_email(address)
        .rsplit_once('@')
        .map(|(_, domain)| domain.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigrams_of_cjk_runs() {
        assert_eq!(cjk_bigrams("你好世界"), "你好 好世 世界");
        assert_eq!(cjk_bigrams("中"), "中");
        assert_eq!(
            cjk_bigrams("中华人民共和国"),
            "中华 华人 人民 民共 共和 和国"
        );
        assert_eq!(cjk_bigrams("你好 世界"), "你好 世界");
    }

    #[test]
    fn bigrams_mix_latin_and_cjk() {
        assert_eq!(cjk_bigrams("QQ邮箱 Hello"), "qq 邮箱 hello");
        assert_eq!(cjk_bigrams("Re: 项目 ABC-123"), "re 项目 abc 123");
    }

    #[test]
    fn bigrams_deduplicate() {
        assert_eq!(cjk_bigrams("你好你好"), "你好 好你");
    }

    #[test]
    fn query_quotes_and_ands_tokens() {
        assert_eq!(fts_query("你好 world"), "\"你好\" AND \"world\"");
        assert_eq!(fts_query("  "), "");
        assert_eq!(fts_query("hello-world"), "\"hello\" AND \"world\"");
        assert_eq!(
            fts_query("say \"hi there\""),
            "\"say\" AND \"hi\" AND \"there\""
        );
        assert_eq!(fts_query("a\"b"), "\"a\" AND \"b\"");
    }

    #[test]
    fn query_expression_is_safe() {
        for input in [
            "NEAR(a b)",
            "col:value",
            "foo*",
            "a OR b",
            "(",
            ")",
            "\"",
            "-term",
            "a+",
        ] {
            let expression = fts_query(input);
            if !expression.is_empty() {
                // Every token is a double quoted string literal.
                for segment in expression.split(" AND ") {
                    assert!(
                        segment.starts_with('"') && segment.ends_with('"'),
                        "{segment}"
                    );
                }
            }
        }
    }

    #[test]
    fn subject_prefixes_are_stripped() {
        assert_eq!(normalize_subject("Re: Hello"), "Hello");
        assert_eq!(normalize_subject("RE: FW: 回复：测试"), "测试");
        assert_eq!(normalize_subject("Re[2]: Hi"), "Hi");
        assert_eq!(normalize_subject("回复：你好"), "你好");
        assert_eq!(normalize_subject("Fwd: Re: Re: x"), "x");
        assert_eq!(normalize_subject("Antw: x"), "x");
        assert_eq!(normalize_subject("AW: WG: x"), "x");
        assert_eq!(normalize_subject("Regarding: x"), "Regarding: x");
        assert_eq!(normalize_subject("Re:"), "");
        assert_eq!(normalize_subject("  Hello  "), "Hello");
        assert_eq!(normalize_subject("回复 : 测试"), "测试");
    }

    #[test]
    fn preview_trimming() {
        assert_eq!(trim_preview("hello   world\n", 100), "hello world");
        assert_eq!(trim_preview("hello world", 5), "hello…");
        assert_eq!(trim_preview("\u{0}abc", 100), "abc");
    }

    #[test]
    fn snippets_center_on_match() {
        let text = "The quick brown fox jumps over the lazy dog";
        let excerpt = snippet(text, "brown", 10);
        assert!(excerpt.contains("brown"), "{excerpt}");
        assert!(excerpt.starts_with('…'), "{excerpt}");
        assert_eq!(snippet(text, "missing", 5), "The q…");
    }

    #[test]
    fn email_normalization() {
        assert_eq!(normalize_email("  Foo@Example.COM "), "foo@example.com");
        assert_eq!(normalize_email("mailto:foo@example.com"), "foo@example.com");
        assert_eq!(normalize_email("<foo@example.com>"), "foo@example.com");
        assert_eq!(
            email_domain("foo@example.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(email_domain("not-an-email"), None);
    }

    #[test]
    fn bigram_tokens_are_searchable_in_fts5() {
        // The bigram column is tokenized by unicode61; a 2-char query token must
        // survive that tokenizer unchanged.
        assert_eq!(fts_query("你好"), "\"你好\"");
        assert_eq!(cjk_bigrams("你好"), "你好");
    }
}
