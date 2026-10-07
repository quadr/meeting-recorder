//! Локализация Rust-стороны GUI.
//!
//! Единственный источник текстов — `ui/i18n/strings.json`: его же читает
//! фронтенд через `fetch`. Здесь файл вшивается в бинарь `include_str!`ом и
//! разбирается один раз, чтобы у Rust не завёлся собственный список фраз,
//! который однажды разъедется с JS-словарём.
//!
//! Кто пользуется этим модулем: трей (нативное меню и подсказка иконки в
//! строке меню — их не отрисовать через JS) и нативные уведомления в
//! `status.rs`. Всё, что рисует webview, переводит фронтенд сам через
//! `window.i18n.t`, получая от Rust только ключ — см. докблок `ActiveLang`.

use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Manager};

const DICT_JSON: &str = include_str!("../../ui/i18n/strings.json");

fn dict() -> &'static Value {
    static DICT: OnceLock<Value> = OnceLock::new();
    DICT.get_or_init(|| {
        serde_json::from_str(DICT_JSON).expect("ui/i18n/strings.json обязан быть валидным JSON")
    })
}

/// Достать строку по ключу `"раздел.ключ"` и языку (`"ru"` | `"en"`), подставив
/// параметры в `{имя}`.
///
/// Ключа нет в словаре — возвращаем сам ключ, как и одноимённая функция в
/// `ui/i18n.js`: отсутствующая строка должна быть видна (это баг, не пропажа
/// текста для пользователя), а падать GUI из-за неё не должен.
pub fn t(key: &str, lang: &str) -> String {
    t_params(key, lang, &[])
}

pub fn t_params(key: &str, lang: &str, params: &[(&str, &str)]) -> String {
    let mut parts = key.splitn(2, '.');
    let section = parts.next().unwrap_or("");
    let name = parts.next().unwrap_or("");
    let raw = dict()
        .get(section)
        .and_then(|s| s.get(name))
        .and_then(|entry| entry.get(lang))
        .and_then(Value::as_str)
        .unwrap_or(key);
    substitute(raw, params)
}

fn substitute(s: &str, params: &[(&str, &str)]) -> String {
    if params.is_empty() {
        return s.to_string();
    }
    let mut out = s.to_string();
    for (name, value) in params {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Действующий язык из значения поля `Config::language` (`None` — то же, что
/// `"system"`): `"ru"`/`"en"` берутся как есть, всё остальное — по языку
/// системы. `ru*` (`ru`, `ru-RU`, `ru_RU`…) — русский, всё прочее — английский:
/// у нас только два языка в словаре, третьего дефолта нет.
pub fn effective_lang(cfg_language: Option<&str>) -> String {
    match cfg_language {
        Some("ru") => "ru".to_string(),
        Some("en") => "en".to_string(),
        _ => {
            let sys = sys_locale::get_locale().unwrap_or_default();
            if sys.to_lowercase().starts_with("ru") {
                "ru".to_string()
            } else {
                "en".to_string()
            }
        }
    }
}

/// Действующий язык на момент вызова — тот, кто хранит его на процесс.
///
/// `Mutex<String>`, а не `AtomicU8`-подобное перечисление: языков всего два
/// сейчас, но кодировать их в биты ради экономии двух байт не стоит того,
/// что придётся тащить по всем местам чтения третье соответствие.
pub struct ActiveLang(Mutex<String>);

impl ActiveLang {
    pub fn new(lang: String) -> Self {
        Self(Mutex::new(lang))
    }

    pub fn get(&self) -> String {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, lang: String) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = lang;
    }
}

/// Короткий путь для мест, у которых есть `AppHandle`, но нет своей ссылки на
/// `ActiveLang` (трей, уведомления).
pub fn active_lang(app: &AppHandle) -> String {
    app.state::<ActiveLang>().get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn достаёт_строку_по_ключу_и_языку() {
        assert_eq!(t("tray.start", "ru"), "Начать запись");
        assert_eq!(t("tray.start", "en"), "Start recording");
    }

    #[test]
    fn незнакомый_ключ_возвращается_как_есть() {
        assert_eq!(t("нет.такого", "ru"), "нет.такого");
    }

    /// Произвольный технический текст без раздела/ключа (например, диагностика
    /// из `audio.rs`) обязан пройти насквозь неизменным, а не потеряться из-за
    /// точек внутри предложения.
    #[test]
    fn произвольный_текст_без_ключа_не_ломается() {
        let raw = "детектор не поднялся: unsupported version.";
        assert_eq!(t(raw, "ru"), raw);
    }

    #[test]
    fn подставляет_параметры() {
        assert_eq!(
            t_params("callabo.history", "en", &[("workspaces", "A, B")]),
            "Callabo: A, B"
        );
    }

    #[test]
    fn системный_язык_ru_даёт_ru() {
        assert_eq!(effective_lang(Some("ru")), "ru");
    }

    #[test]
    fn системный_язык_en_даёт_en() {
        assert_eq!(effective_lang(Some("en")), "en");
    }

    /// `"system"`, отсутствие значения и мусор ведут по одному пути — на
    /// язык ОС, а не считаются тремя разными случаями.
    #[test]
    fn system_и_none_и_мусор_идут_по_системному_языку() {
        let a = effective_lang(Some("system"));
        let b = effective_lang(None);
        let c = effective_lang(Some("fr"));
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert!(a == "ru" || a == "en");
    }

    #[test]
    fn active_lang_хранит_последнее_установленное() {
        let al = ActiveLang::new("ru".to_string());
        assert_eq!(al.get(), "ru");
        al.set("en".to_string());
        assert_eq!(al.get(), "en");
    }
}
