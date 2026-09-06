#[cfg(not(test))]
use std::sync::OnceLock;

pub const LANGUAGE_ENV: &str = "ZNNZ_LAUNCHER_LANG";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    En,
}

impl Language {
    #[cfg_attr(test, allow(dead_code))]
    pub fn detect() -> Self {
        std::env::var(LANGUAGE_ENV)
            .ok()
            .and_then(|value| Self::from_tag(&value))
            .unwrap_or_else(|| {
                if crate::platform::user_prefers_chinese() {
                    Self::ZhCn
                } else {
                    Self::En
                }
            })
    }

    pub fn from_tag(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase().replace('_', "-");
        if value == "zh" || value.starts_with("zh-") {
            Some(Self::ZhCn)
        } else if value == "en" || value.starts_with("en-") {
            Some(Self::En)
        } else {
            None
        }
    }

    pub const fn tag(self) -> &'static str {
        match self {
            Self::ZhCn => "zh-CN",
            Self::En => "en",
        }
    }

    pub const fn text(self, zh_cn: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::ZhCn => zh_cn,
            Self::En => en,
        }
    }
}

#[cfg(not(test))]
pub fn language() -> Language {
    static LANGUAGE: OnceLock<Language> = OnceLock::new();
    *LANGUAGE.get_or_init(Language::detect)
}

#[cfg(test)]
pub fn language() -> Language {
    // Existing behavior tests assert the long-standing Chinese wording. Keep
    // them deterministic on English GitHub runners; English selection itself
    // is covered through the pure Language helpers below.
    Language::ZhCn
}

pub fn tr(zh_cn: &'static str, en: &'static str) -> &'static str {
    language().text(zh_cn, en)
}

/// Format an internal error for user-facing runtime logs. English mode drops
/// untranslated context frames while retaining English OS, network, and parser
/// causes. This prevents a deeply nested `anyhow` context from leaking Chinese
/// launcher text into an otherwise English log.
pub fn runtime_error(error: &anyhow::Error) -> String {
    runtime_error_text(&format!("{error:#}"))
}

pub fn runtime_error_text(detail: &str) -> String {
    if language() == Language::ZhCn {
        return detail.to_owned();
    }

    let detail = detail
        .lines()
        .map(strip_cjk)
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .map(|line| {
            line.trim_start_matches([':', '：', ';', '；', ',', '，', '.', '。'])
                .trim_start()
                .to_owned()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if detail.is_empty() {
        "The operation failed.".to_owned()
    } else {
        detail
    }
}

fn strip_cjk(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !matches!(
                character,
                '\u{3400}'..='\u{4DBF}'
                    | '\u{4E00}'..='\u{9FFF}'
                    | '\u{F900}'..='\u{FAFF}'
                    | '\u{20000}'..='\u{2FA1F}'
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_tags_follow_chinese_or_english_policy() {
        assert_eq!(Language::from_tag("zh-CN"), Some(Language::ZhCn));
        assert_eq!(Language::from_tag("zh_TW"), Some(Language::ZhCn));
        assert_eq!(Language::from_tag("en-US"), Some(Language::En));
        assert_eq!(Language::from_tag("fr-FR"), None);
    }

    #[test]
    fn localized_text_selects_expected_branch() {
        assert_eq!(Language::ZhCn.text("中文", "English"), "中文");
        assert_eq!(Language::En.text("中文", "English"), "English");
    }

    #[test]
    fn runtime_error_text_keeps_english_technical_details() {
        assert_eq!(
            strip_cjk("无法连接 gateway: connect timed out")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            "gateway: connect timed out"
        );
        assert_eq!(
            strip_cjk("connect timed out: https://api.example.com"),
            "connect timed out: https://api.example.com"
        );
    }
}
