//! Проверка обновлений: есть ли на GitHub релиз новее того, что запущен.
//!
//! Никакого встроенного установщика: Tauri updater требует ключей подписи и
//! манифеста, а на macOS ad-hoc сборку он всё равно не проведёт мимо
//! Gatekeeper. Здесь ровно одно: узнать номер последнего релиза, сравнить с
//! версией из `tauri.conf.json` и показать баннер «Скачать / Позже». «Скачать»
//! открывает страницу релиза в браузере (`open_url` в `main.rs`), «Позже»
//! запоминает пропущенную версию в конфиге — баннер вернётся только со
//! следующим релизом.
//!
//! Пока репозиторий приватный, API отдаёт 404 без токена, и проверка молча
//! ничего не находит: зашивать токен в приложение нельзя. Сеть, парсинг и
//! любой другой отказ — тоже молча, в лог: обновление — не повод мешать
//! человеку записывать встречу.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Duration;

/// Откуда берётся последний релиз. Черновики и pre-release сюда не попадают —
/// `latest` у GitHub это только полноценные релизы.
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/quadr/meeting-recorder/releases/latest";

/// Как часто спрашивать, пока приложение живёт в трее. Сутки: релизы выходят
/// не чаще, а лимит анонимных запросов к API — 60 в час на адрес.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Предел одного запроса. Ответ — одна страница JSON; без предела запрос на
/// протухшей сети висел бы, а вместе с ним и вся задача проверки.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Что показать человеку: номер без `v` и ссылка на страницу релиза.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Release {
    pub version: String,
    pub url: String,
}

/// Ровно те поля ответа `releases/latest`, что нужны. `draft`/`prerelease`
/// на `latest` не приходят в виде `true` никогда, но проверить дешевле, чем
/// полагаться на это.
#[derive(Deserialize)]
struct LatestPayload {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// `v0.3.1` / `0.3.1` → `(0, 3, 1)`. Всё, что не три числа через точку, —
/// `None`: сравнивать такое не с чем, а угадывать опасно (можно предложить
/// «обновиться» на старую версию).
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
    let mut parts = s.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next()??;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Релиз новее запущенного. Неразобранная версия с любой стороны — `false`:
/// лучше промолчать, чем предложить откат.
pub fn is_newer(current: &str, latest: &str) -> bool {
    match (parse_version(current), parse_version(latest)) {
        (Some(c), Some(l)) => l > c,
        _ => false,
    }
}

/// Разобрать ответ GitHub в `Release`. Черновик, pre-release и мусор — `None`.
pub fn parse_latest(json: &str) -> Option<Release> {
    let p: LatestPayload = serde_json::from_str(json).ok()?;
    if p.draft || p.prerelease {
        return None;
    }
    let version = p.tag_name.trim().strip_prefix('v').unwrap_or(p.tag_name.trim()).to_string();
    parse_version(&version)?;
    Some(Release { version, url: p.html_url })
}

/// Показывать ли баннер: релиз новее запущенной версии и человек не нажимал
/// «Позже» именно на нём. «Позже» на 0.3.0 не гасит 0.3.1.
pub fn decide(current: &str, latest: Option<Release>, skipped: Option<&str>) -> Option<Release> {
    let r = latest?;
    if !is_newer(current, &r.version) {
        return None;
    }
    if skipped.map(|s| s.trim().strip_prefix('v').unwrap_or(s.trim()) == r.version) == Some(true) {
        return None;
    }
    Some(r)
}

/// Что нашла последняя проверка. Webview может подняться позже события
/// `update-available` (события Tauri не буферизуются — см. `status.rs`),
/// поэтому результат лежит здесь и читается командой `update_status` при
/// загрузке окна, тем же приёмом, что `get_state`.
#[derive(Default)]
pub struct UpdateState(Mutex<Option<Release>>);

impl UpdateState {
    pub fn set(&self, r: Option<Release>) {
        if let Ok(mut g) = self.0.lock() {
            *g = r;
        }
    }
    pub fn get(&self) -> Option<Release> {
        self.0.lock().ok().and_then(|g| g.clone())
    }
}

/// Спросить GitHub. `Ok(None)` — релиза нет или он не разобрался (404 на
/// приватном репозитории сюда же). `Err` — сеть/таймаут, в лог.
pub async fn fetch_latest(client: &reqwest::Client) -> Result<Option<Release>, reqwest::Error> {
    let resp = client
        .get(LATEST_RELEASE_URL)
        // GitHub отвечает 403 на запрос без User-Agent.
        .header(reqwest::header::USER_AGENT, concat!("MeetRec/", env!("CARGO_PKG_VERSION")))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await?;
    if !resp.status().is_success() {
        return Ok(None);
    }
    let body = resp.text().await?;
    Ok(parse_latest(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(v: &str) -> Release {
        Release { version: v.into(), url: format!("https://example.test/{v}") }
    }

    #[test]
    fn версия_с_буквой_v_и_без_разбирается_одинаково() {
        assert_eq!(parse_version("v0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version("0.3.1"), Some((0, 3, 1)));
        assert_eq!(parse_version(" v1.0.0 "), Some((1, 0, 0)));
    }

    #[test]
    fn не_три_числа_это_не_версия() {
        assert_eq!(parse_version("0.3"), None);
        assert_eq!(parse_version("0.3.1.4"), None);
        assert_eq!(parse_version("latest"), None);
        assert_eq!(parse_version("0.3.1-beta"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn новее_сравнивается_по_числам_а_не_по_строкам() {
        assert!(is_newer("0.2.0", "0.3.0"));
        assert!(is_newer("0.2.0", "v0.2.1"));
        assert!(is_newer("0.9.0", "0.10.0"), "10 > 9, хотя строка «0.10.0» < «0.9.0»");
        assert!(!is_newer("0.3.0", "0.3.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
    }

    /// Неразобранная версия — молчание, а не предложение «обновиться».
    #[test]
    fn мусор_в_версии_не_даёт_обновления() {
        assert!(!is_newer("0.2.0", "latest"));
        assert!(!is_newer("dev", "0.3.0"));
    }

    #[test]
    fn ответ_github_разбирается_в_версию_и_ссылку() {
        let json = r#"{"tag_name":"v0.3.0","html_url":"https://github.com/x/y/releases/tag/v0.3.0","draft":false,"prerelease":false,"name":"MeetRec v0.3.0"}"#;
        assert_eq!(
            parse_latest(json),
            Some(Release {
                version: "0.3.0".into(),
                url: "https://github.com/x/y/releases/tag/v0.3.0".into()
            })
        );
    }

    #[test]
    fn черновик_и_prerelease_не_считаются_релизом() {
        let draft = r#"{"tag_name":"v0.3.0","html_url":"u","draft":true}"#;
        let pre = r#"{"tag_name":"v0.3.0","html_url":"u","prerelease":true}"#;
        assert_eq!(parse_latest(draft), None);
        assert_eq!(parse_latest(pre), None);
    }

    #[test]
    fn мусор_вместо_json_не_паникует() {
        assert_eq!(parse_latest("Not Found"), None);
        assert_eq!(parse_latest(r#"{"message":"Not Found"}"#), None);
        assert_eq!(parse_latest(r#"{"tag_name":"nightly","html_url":"u"}"#), None);
    }

    #[test]
    fn баннер_только_на_версию_новее_и_не_пропущенную() {
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), None), Some(release("0.3.0")));
        assert_eq!(decide("0.3.0", Some(release("0.3.0")), None), None);
        assert_eq!(decide("0.2.0", None, None), None);
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), Some("0.3.0")), None, "нажали «Позже»");
        assert_eq!(decide("0.2.0", Some(release("0.3.0")), Some("v0.3.0")), None, "«Позже» с буквой v");
    }

    /// «Позже» гасит один релиз, а не все будущие.
    #[test]
    fn позже_не_гасит_следующий_релиз() {
        assert_eq!(decide("0.2.0", Some(release("0.3.1")), Some("0.3.0")), Some(release("0.3.1")));
    }

    #[test]
    fn состояние_хранит_последний_результат() {
        let s = UpdateState::default();
        assert_eq!(s.get(), None);
        s.set(Some(release("0.3.0")));
        assert_eq!(s.get(), Some(release("0.3.0")));
        s.set(None);
        assert_eq!(s.get(), None);
    }
}
