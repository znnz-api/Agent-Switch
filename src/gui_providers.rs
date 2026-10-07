use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use url::Url;

pub const MANIFEST_URL: &str = "https://provider-presets.networkpe.top/providers.json";
const CACHE_FILE: &str = "provider-presets.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub icon_url: String,
    pub icon_bytes: Vec<u8>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub api_path_mode: Option<String>,
    /// Optional native protocol list used only for the conversion recommendation.
    #[serde(default)]
    pub protocols: Option<Vec<String>>,
    /// Optional local-time expiration in `YYYY/MM/DD-HH:MM` format.
    #[serde(default, alias = "expires_at", alias = "valid_until")]
    pub expires: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    schema_version: u32,
    providers: Vec<ProviderRecord>,
}
#[derive(Debug, Clone, Deserialize)]
struct ProviderRecord {
    id: String,
    name: String,
    endpoint: String,
    icon: String,
    #[serde(default)]
    website: Option<String>,
    #[serde(default)]
    lang: Option<String>,
    #[serde(default)]
    api_path_mode: Option<String>,
    #[serde(default)]
    protocols: Option<Vec<String>>,
    #[serde(default, alias = "expires_at", alias = "valid_until")]
    expires: Option<String>,
}

fn cache_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|p| p.join("AppData").join("Local"))
        })
        .map(|root| root.join("Agent-Switch").join("provider-cache"))
}

fn language_matches(lang: Option<&str>) -> bool {
    // Provider manifests must opt in to at least one UI language.  A missing
    // or empty `lang` field is therefore hidden, while `en-zh` (in either
    // order) makes the provider visible in both localized builds.
    let Some(lang) = lang.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    let requested = match crate::i18n::language() {
        crate::i18n::Language::ZhCn => "zh",
        crate::i18n::Language::En => "en",
    };
    lang.split(['-', ',', ';', '|'])
        .map(str::trim)
        .any(|value| value.eq_ignore_ascii_case(requested))
}

fn expiration_matches(expires: Option<&str>) -> bool {
    let Some(expires) = expires.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let Some(deadline) = parse_expiration(expires) else {
        return false;
    };
    deadline >= local_date_time()
}

fn parse_expiration(value: &str) -> Option<(u16, u8, u8, u8, u8)> {
    let (date, time) = value.split_once('-')?;
    let mut date_parts = date.split('/');
    let year = date_parts.next()?.parse().ok()?;
    let month = date_parts.next()?.parse().ok()?;
    let day = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse().ok()?;
    let minute = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 {
        return None;
    }
    Some((year, month, day, hour, minute))
}

#[cfg(windows)]
fn local_date_time() -> (u16, u8, u8, u8, u8) {
    #[repr(C)]
    struct SystemTime {
        year: u16,
        month: u16,
        day_of_week: u16,
        day: u16,
        hour: u16,
        minute: u16,
        second: u16,
        milliseconds: u16,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLocalTime(time: *mut SystemTime);
    }
    let mut time = SystemTime {
        year: 0,
        month: 0,
        day_of_week: 0,
        day: 0,
        hour: 0,
        minute: 0,
        second: 0,
        milliseconds: 0,
    };
    unsafe { GetLocalTime(&mut time) };
    (
        time.year,
        time.month as u8,
        time.day as u8,
        time.hour as u8,
        time.minute as u8,
    )
}

#[cfg(not(windows))]
fn local_date_time() -> (u16, u8, u8, u8, u8) {
    // Non-Windows builds are only used for development/tests. UTC is a stable
    // fallback there; release builds run on Windows and use GetLocalTime.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (seconds / 86_400) as i64;
    let (year, month, day) = civil_from_days(days);
    let day_seconds = seconds % 86_400;
    (
        year as u16,
        month,
        day,
        (day_seconds / 3_600) as u8,
        ((day_seconds % 3_600) / 60) as u8,
    )
}

#[cfg(not(windows))]
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u8, d as u8)
}

fn validate_https(value: &str, label: &str) -> Result<Url> {
    let url = Url::parse(value).with_context(|| format!("{label} URL 无效"))?;
    if url.scheme() != "https" {
        bail!("{label} 必须使用 HTTPS");
    }
    Ok(url)
}

pub async fn fetch() -> Result<Vec<Provider>> {
    let client = crate::network_proxy::configure_reqwest_builder(
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(15)),
    )?
    .build()?;
    let response = crate::network_proxy::send_with_network_retry(
        client
            .get(MANIFEST_URL)
            .header("Accept", "application/json"),
    )
    .await?;
    if !response.status().is_success() {
        bail!("提供商清单返回 HTTP {}", response.status());
    }
    let manifest: Manifest = response.json().await.context("提供商清单 JSON 无效")?;
    if manifest.schema_version != 1 {
        bail!("不支持的提供商清单版本 {}", manifest.schema_version);
    }
    let root = Url::parse(MANIFEST_URL)?;
    let cached = load_cache().unwrap_or_default();
    let mut providers = Vec::new();
    for record in manifest
        .providers
        .into_iter()
        .filter(|p| language_matches(p.lang.as_deref()))
        .filter(|p| expiration_matches(p.expires.as_deref()))
    {
        let icon_url = root
            .join(&record.icon)
            .or_else(|_| Url::parse(&record.icon))
            .context("提供商图标 URL 无效")?;
        validate_https(icon_url.as_str(), "提供商图标")?;
        let website = record
            .website
            .as_deref()
            .map(|value| validate_https(value, "提供商网址").map(|_| value.to_owned()))
            .transpose()?;
        let icon_bytes = match client.get(icon_url.clone()).send().await {
            Ok(response) if response.status().is_success() => response.bytes().await?.to_vec(),
            _ => cached
                .iter()
                .find(|old| old.icon_url == icon_url.as_str())
                .map(|old| old.icon_bytes.clone())
                .unwrap_or_default(),
        };
        providers.push(Provider {
            id: record.id,
            name: record.name,
            endpoint: record.endpoint,
            icon_url: icon_url.to_string(),
            icon_bytes,
            website,
            lang: record.lang,
            api_path_mode: record.api_path_mode,
            protocols: record.protocols,
            expires: record.expires,
        });
    }
    save_cache(&providers)?;
    Ok(providers)
}

pub fn load_cache() -> Result<Vec<Provider>> {
    let Some(root) = cache_root() else {
        return Ok(Vec::new());
    };
    let bytes = fs::read(root.join(CACHE_FILE)).context("读取提供商缓存失败")?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn save_cache(providers: &[Provider]) -> Result<()> {
    let Some(root) = cache_root() else {
        return Ok(());
    };
    fs::create_dir_all(&root)?;
    crate::backup::atomic_write(&root.join(CACHE_FILE), &serde_json::to_vec(providers)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expiration_matches, language_matches, parse_expiration};

    #[test]
    fn provider_language_requires_an_explicit_language() {
        assert!(!language_matches(None));
        assert!(!language_matches(Some("")));
        assert!(!language_matches(Some("fr")));
    }

    #[test]
    fn provider_language_accepts_single_or_bilingual_tags() {
        // Tests run with the deterministic Chinese language selected by the
        // i18n test configuration.
        assert!(language_matches(Some("zh")));
        assert!(language_matches(Some("en-zh")));
        assert!(language_matches(Some("zh-en")));
        assert!(!language_matches(Some("en")));
    }

    #[test]
    fn provider_expiration_uses_the_requested_format() {
        assert_eq!(
            parse_expiration("2026/12/03-12:00"),
            Some((2026, 12, 3, 12, 0))
        );
        assert!(parse_expiration("2026-12-03 12:00").is_none());
        assert!(parse_expiration("2026/02/30-12:00").is_none());
        assert!(expiration_matches(None));
    }
}
