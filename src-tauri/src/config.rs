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
    /// Версия релиза, на которой человек нажал «Позже» в баннере обновления
    /// (`update.rs`). Отсутствие поля и `None` — ничего не пропускали. Гасит
    /// ровно одну версию: следующий релиз баннер покажет снова.
    pub update_skipped_version: Option<String>,
    /// Only the non-secret workspace selection is persisted. The PAT is not.
    pub callabo_workspace: Option<String>,
    /// Last successful upload choices, isolated by workspace slug. No title/PAT.
    pub callabo_upload_settings:
        std::collections::BTreeMap<String, crate::callabo::UploadPreferences>,
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
        std::fs::write(&path, json)
            .map_err(|e| format!("не удалось записать {}: {e}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_transcript_settings_are_ignored_without_losing_recording_or_callabo_preferences() {
        let config = Config::from_str(
            r#"{
            "stt_gateway_url":"https://old.example.com", "stt_api_key":"old-secret",
            "transcribe_mode":"local", "transcribe_server":"whisper_cpp", "audio_retention_days":7,
            "mic_device_id":"mic-1", "mic_device_name":"My mic", "language":"en", "theme":"dark",
            "callabo_workspace":"a", "callabo_upload_settings":{"a":{"team_ids":[11],"transcribe_language":"ko"}}
        }"#,
        );
        assert_eq!(config.mic_device_id.as_deref(), Some("mic-1"));
        assert_eq!(config.language.as_deref(), Some("en"));
        assert_eq!(config.theme.as_deref(), Some("dark"));
        assert_eq!(config.callabo_workspace.as_deref(), Some("a"));
        assert_eq!(config.callabo_upload_settings["a"].team_ids, vec![11]);
        assert_eq!(
            config.callabo_upload_settings["a"].transcribe_language,
            "ko"
        );
        let value = serde_json::to_value(&config).unwrap();
        for key in [
            "stt_gateway_url",
            "stt_api_key",
            "transcribe_mode",
            "transcribe_server",
            "audio_retention_days",
        ] {
            assert!(value.get(key).is_none());
        }
        assert!(!value.to_string().contains("old-secret"));
    }

    #[test]
    fn config_roundtrips_recording_preferences_and_isolates_callabo_workspaces() {
        let mut config = Config {
            mic_device_id: Some("mic-1".into()),
            mic_device_name: Some("Microphone".into()),
            language: Some("ru".into()),
            theme: Some("dark".into()),
            update_skipped_version: Some("0.2.1".into()),
            callabo_workspace: Some("a".into()),
            ..Default::default()
        };
        config.callabo_upload_settings.insert(
            "a".into(),
            crate::callabo::UploadPreferences {
                team_ids: vec![11],
                transcribe_language: "ko".into(),
                ..Default::default()
            },
        );
        config.callabo_upload_settings.insert(
            "b".into(),
            crate::callabo::UploadPreferences {
                team_ids: vec![22],
                transcribe_language: "en".into(),
                ..Default::default()
            },
        );
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(Config::from_str(&json), config);
        assert!(!json.contains("callabo_token"));
        assert_eq!(config.choice(), DeviceChoice::Id("mic-1".into()));
    }

    #[test]
    fn missing_or_invalid_config_preserves_safe_defaults() {
        assert_eq!(Config::from_str("{}"), Config::default());
        assert_eq!(Config::from_str("{broken"), Config::default());
        assert_eq!(Config::default().choice(), DeviceChoice::Default);
    }
}
