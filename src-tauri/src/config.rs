//! Настройки, переживающие перезапуск.
//!
//! Живут в GUI, а не в ядре, намеренно: `App` тестируется без файловой системы,
//! и чтение конфига внутри него отняло бы эту способность. Ядро принимает выбор
//! устройства как значение — где оно хранится, его не касается.

use meeting_recorder::capture::DeviceChoice;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
#[serde(default)]
pub struct Config {
    /// Идентификатор эндпоинта (`DeviceId` строкой). `None` — системный дефолт.
    pub mic_device_id: Option<String>,
    /// Имя на момент выбора. Только для показа: подставляется в выпадашку и в
    /// предупреждение о подмене, чтобы пользователь видел «Headset (Boss Bose)»,
    /// а не `{0.0.1.00000000}.{guid}`. Матчинг по нему не идёт нигде.
    pub mic_device_name: Option<String>,
    /// Базовый URL шлюза, например `http://localhost:8080` — без хвоста
    /// `/v1/...`, его дописывает клиент транскрипции.
    pub stt_gateway_url: Option<String>,
    pub stt_api_key: Option<String>,
    /// `"system" | "ru" | "en"`. Отсутствие поля (старый конфиг) и `None` —
    /// то же самое, что `"system"`: язык берётся из ОС. Разбор значения в
    /// действующий язык живёт в `i18n::effective_lang`, а не здесь — этот
    /// файл ничего не знает про словарь и локаль системы.
    pub language: Option<String>,
    /// `"system" | "light" | "dark"`. Отсутствие поля (старый конфиг) и
    /// `None` — то же самое, что `"system"`: тема берётся из ОС через
    /// `prefers-color-scheme`. Сам файл ничего не знает про CSS и `data-theme` —
    /// это решает фронтенд при чтении конфига.
    pub theme: Option<String>,
    /// Через сколько дней автоочистка отправляет звук записи в корзину,
    /// оставляя расшифровку на месте. Отсутствие поля (старый конфиг) и
    /// `None` — «никогда»: приложение, которое само стирает чужие записи по
    /// умолчанию, — плохой сюрприз, включать чистку должен человек, а не
    /// обновление. Разбор числа в решение «пора/не пора» живёт в
    /// `retention.rs`, а не здесь — этот файл ничего не знает про даты
    /// записей и файлы на диске.
    pub audio_retention_days: Option<u32>,
    /// `"server" | "local"`. Отсутствие поля (старый конфиг) и `None` — то
    /// же самое, что `"server"`: расшифровка идёт на шлюзе, как и раньше.
    /// Разбор значения в действующий режим живёт в `transcribe::effective_mode`,
    /// а не здесь — этот файл ничего не знает про сеть, очередь расшифровки
    /// и локальный движок.
    pub transcribe_mode: Option<String>,
    /// Версия релиза, на которой человек нажал «Позже» в баннере обновления
    /// (`update.rs`). Отсутствие поля и `None` — ничего не пропускали. Гасит
    /// ровно одну версию: следующий релиз баннер покажет снова.
    pub update_skipped_version: Option<String>,
    /// `"gateway" | "whisper_cpp"`. Отсутствие поля (старый конфиг) и `None` —
    /// то же самое, что `"gateway"`: шлюз selfhost-ai-lab, как было всегда.
    /// Разбор значения в действующий режим живёт в `transcribe::effective_mode`,
    /// а не здесь — этот файл ничего не знает про протоколы серверов.
    pub transcribe_server: Option<String>,
    /// Only the non-secret workspace selection is persisted. The PAT is not.
    pub callabo_workspace: Option<String>,
    /// Last successful upload choices, isolated by workspace slug. No title/PAT.
    pub callabo_upload_settings: std::collections::BTreeMap<String, crate::callabo::UploadPreferences>,
}

impl Config {
    pub fn choice(&self) -> DeviceChoice {
        match &self.mic_device_id {
            Some(id) => DeviceChoice::Id(id.clone()),
            None => DeviceChoice::Default,
        }
    }

    /// Разбор без единого способа упасть: битый или чужой JSON даёт дефолт.
    /// Потерять настройку неприятно, не записать встречу — хуже.
    pub fn from_str(s: &str) -> Self {
        serde_json::from_str(s).unwrap_or_default()
    }

    fn path(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_config_dir()
            .map_err(|e| format!("не найден каталог конфига: {e}"))?;
        Ok(dir.join("config.json"))
    }

    pub fn load(app: &AppHandle) -> Self {
        match Self::path(app).and_then(|p| std::fs::read_to_string(p).map_err(|e| e.to_string())) {
            Ok(s) => Self::from_str(&s),
            // Файла нет при первом запуске — это не ошибка.
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, app: &AppHandle) -> Result<(), String> {
        let path = Self::path(app)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("не удалось создать {}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callabo_workspace_roundtrips_without_a_token_field() {
        let config = Config { callabo_workspace: Some("workspace-slug".into()), ..Config::default() };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(Config::from_str(&json), config);
        assert!(!json.contains("callabo_token"));
        assert_eq!(Config::from_str("{}").callabo_workspace, None);
    }

    #[test]
    fn callabo_upload_preferences_are_isolated_per_workspace_and_exclude_title_scope() {
        let mut config = Config::default();
        let a = crate::callabo::UploadPreferences { team_ids: vec![11], transcribe_language: "ko".into(), ..Default::default() };
        let b = crate::callabo::UploadPreferences { team_ids: vec![22], transcribe_language: "en".into(), ..Default::default() };
        config.callabo_upload_settings.insert("a".into(), a.clone());
        config.callabo_upload_settings.insert("b".into(), b.clone());
        let json = serde_json::to_string(&config).unwrap();
        let restored = Config::from_str(&json);
        assert_eq!(restored.callabo_upload_settings["a"], a);
        assert_eq!(restored.callabo_upload_settings["b"], b);
        assert!(!json.contains("title") && !json.contains("scope") && !json.contains("callabo_token"));
        assert!(Config::from_str("{}").callabo_upload_settings.is_empty());
    }

    #[test]
    fn пустой_конфиг_это_системный_дефолт() {
        assert_eq!(Config::default().choice(), DeviceChoice::Default);
    }

    #[test]
    fn идентификатор_устройства_превращается_в_id() {
        let c = Config {
            mic_device_id: Some("{0.0.1.00000000}.{guid}".into()),
            mic_device_name: Some("Headset (Boss Bose)".into()),
            stt_gateway_url: None,
            stt_api_key: None,
            language: None,
            theme: None,
            audio_retention_days: None,
            transcribe_mode: None,
            update_skipped_version: None,
            transcribe_server: None,
            callabo_workspace: None,
            callabo_upload_settings: Default::default(),
        };
        assert_eq!(c.choice(), DeviceChoice::Id("{0.0.1.00000000}.{guid}".into()));
    }

    /// Имя — только для показа. Конфиг с одним именем и без идентификатора
    /// матчить нечем, и притворяться, что устройство выбрано, нельзя.
    #[test]
    fn одно_имя_без_идентификатора_это_дефолт() {
        let c = Config {
            mic_device_id: None,
            mic_device_name: Some("Headset (Boss Bose)".into()),
            stt_gateway_url: None,
            stt_api_key: None,
            language: None,
            theme: None,
            audio_retention_days: None,
            transcribe_mode: None,
            update_skipped_version: None,
            transcribe_server: None,
            callabo_workspace: None,
            callabo_upload_settings: Default::default(),
        };
        assert_eq!(c.choice(), DeviceChoice::Default);
    }

    /// Битый JSON — это потерянная настройка, а не потерянная запись.
    #[test]
    fn битый_json_даёт_дефолт_а_не_панику() {
        assert_eq!(Config::from_str("{ это не json"), Config::default());
    }

    #[test]
    fn незнакомые_поля_не_ломают_разбор() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}","что_то_новое":42}"#);
        assert_eq!(c.mic_device_id.as_deref(), Some("{id}"));
        assert_eq!(c.mic_device_name, None, "отсутствующее поле — не ошибка");
    }

    #[test]
    fn настройки_транскрипции_переживают_сериализацию() {
        let c = Config {
            mic_device_id: None,
            mic_device_name: None,
            stt_gateway_url: Some("http://localhost:8080".to_string()),
            stt_api_key: Some("test_key_xxx".to_string()),
            language: None,
            theme: None,
            audio_retention_days: None,
            transcribe_mode: None,
            update_skipped_version: None,
            transcribe_server: None,
            callabo_workspace: None,
            callabo_upload_settings: Default::default(),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    #[test]
    fn старый_конфиг_без_настроек_транскрипции_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.stt_gateway_url, None);
        assert_eq!(c.stt_api_key, None);
    }

    #[test]
    fn смена_только_микрофонных_полей_не_трогает_остальные() {
        let mut cfg = Config {
            mic_device_id: None,
            mic_device_name: None,
            stt_gateway_url: Some("http://localhost:8080".to_string()),
            stt_api_key: Some("secret".to_string()),
            language: None,
            theme: None,
            audio_retention_days: None,
            transcribe_mode: None,
            update_skipped_version: None,
            transcribe_server: None,
            callabo_workspace: None,
            callabo_upload_settings: Default::default(),
        };
        cfg.mic_device_id = Some("{new-id}".to_string());
        cfg.mic_device_name = Some("Новый микрофон".to_string());
        assert_eq!(cfg.stt_gateway_url.as_deref(), Some("http://localhost:8080"));
        assert_eq!(cfg.stt_api_key.as_deref(), Some("secret"));
    }

    /// Конфиг, записанный до появления языка, обязан читаться как есть —
    /// это и есть «системный дефолт», а не ошибка разбора.
    #[test]
    fn старый_конфиг_без_языка_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.language, None);
    }

    /// Форма — как у настоящего `config.json`, записанного до появления
    /// поля `language`; значения выдуманы намеренно. Регрессия здесь
    /// означала бы, что обновление стирает настройки у всех, кто уже
    /// пользуется приложением.
    ///
    /// Настоящие адрес шлюза и ключ в тесты не попадают: репозиторий
    /// открытый, и любой пример отсюда виден всем.
    #[test]
    fn старый_конфиг_без_языка_читается() {
        let raw = r#"{
  "mic_device_id": "coreaudio:BuiltInMicrophoneDevice",
  "mic_device_name": "MacBook Pro Microphone",
  "stt_gateway_url": "https://stt.example.com",
  "stt_api_key": "test_key_not_a_real_one"
}"#;
        let c = Config::from_str(raw);
        assert_eq!(c.mic_device_id.as_deref(), Some("coreaudio:BuiltInMicrophoneDevice"));
        assert_eq!(c.language, None);
    }

    #[test]
    fn язык_переживает_сериализацию() {
        let c = Config { language: Some("en".to_string()), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    /// Конфиг, записанный до появления темы, обязан читаться как есть — то
    /// же требование, что и для языка выше.
    #[test]
    fn старый_конфиг_без_темы_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.theme, None);
    }

    #[test]
    fn тема_переживает_сериализацию() {
        let c = Config { theme: Some("dark".to_string()), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    /// Конфиг, записанный до появления автоочистки, обязан читаться как
    /// «никогда» — то же требование, что и для языка и темы выше. Значение
    /// по умолчанию здесь особенно важно: приложение, которое само стирает
    /// звук по умолчанию, — плохой сюрприз, включать чистку должен человек.
    #[test]
    fn старый_конфиг_без_автоочистки_даёт_никогда() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.audio_retention_days, None);
    }

    #[test]
    fn срок_автоочистки_переживает_сериализацию() {
        let c = Config { audio_retention_days: Some(30), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    /// Конфиг, записанный до появления выбора «сервер/локально», обязан
    /// читаться как сервер — то же требование, что и для языка, темы и
    /// автоочистки выше: старую настройку read не ломает и не подменяет.
    #[test]
    fn старый_конфиг_без_режима_расшифровки_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.transcribe_mode, None);
    }

    #[test]
    fn режим_расшифровки_переживает_сериализацию() {
        let c = Config { transcribe_mode: Some("local".to_string()), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }

    /// Конфиг, записанный до появления типа сервера, обязан читаться как
    /// шлюз — то же требование, что и для режима расшифровки выше.
    #[test]
    fn старый_конфиг_без_типа_сервера_даёт_none() {
        let c = Config::from_str(r#"{"mic_device_id":"{id}"}"#);
        assert_eq!(c.transcribe_server, None);
    }

    #[test]
    fn тип_сервера_переживает_сериализацию() {
        let c = Config { transcribe_server: Some("whisper_cpp".to_string()), ..Config::default() };
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(Config::from_str(&json), c);
    }
}
