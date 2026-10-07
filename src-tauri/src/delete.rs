//! Move recording audio and metadata to the system Recycle Bin.
//! Never permanently delete conversations or touch legacy transcript folders.

use std::path::{Path, PathBuf};

/// Audio formats and metadata managed as one recording.
const SUFFIXES: [&str; 5] = [
    ".wav",
    ".mic.wav",
    ".system.wav",
    ".meta.json",
    ".callabo.json",
];

/// Trash the existing recording members; missing members are fine.
/// Return an error when no managed recording files exist.
pub fn delete_recording(dir: &Path, base: &str) -> Result<(), String> {
    let existing: Vec<PathBuf> = SUFFIXES
        .iter()
        .map(|s| dir.join(format!("{base}{s}")))
        .filter(|p| p.exists())
        .collect();
    if existing.is_empty() {
        return Err(format!("запись «{base}» не найдена в {}", dir.display()));
    }
    trash::delete_all(&existing).map_err(|e| format!("не удалось отправить в корзину: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Уникальный временный каталог без внешних зависимостей. Удаляется в
    /// Drop, даже если тест упал. Тот же приём, что в `rename.rs`.
    struct ScratchDir(PathBuf);

    impl ScratchDir {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!("mr-delete-{tag}-{pid}-{nanos}-{n}"));
            std::fs::create_dir_all(&path).expect("создать временный каталог");
            Self(path)
        }
    }

    impl std::ops::Deref for ScratchDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn transcript_only_folder_is_not_a_deletable_recording() {
        let dir = ScratchDir::new("legacy-only");
        let base = "2026-10-07_10-00_meeting";
        let legacy = dir.join(format!("{base}.transcript"));
        std::fs::create_dir(&legacy).unwrap();
        std::fs::write(legacy.join("summary.md"), b"old transcript").unwrap();
        assert!(delete_recording(&dir, base).is_err());
        assert_eq!(
            std::fs::read(legacy.join("summary.md")).unwrap(),
            b"old transcript"
        );
    }

    fn файл(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), b"x").expect("создать файл");
    }

    /// Корзина в тестовой песочнице CI недоступна (нет ни macOS Trash, ни
    /// Windows Recycle Bin в контейнере) — эти тесты гоняются только там, где
    /// `trash::delete_all` действительно умеет отработать. Логика подбора
    /// путей (какие файлы существуют и что считать «ничего не найдено»)
    /// проверяется отдельно, без обращения к самой корзине, — см. тесты ниже.
    #[test]
    #[ignore = "трогает системную корзину — гонять руками, не в CI"]
    fn audio_and_metadata_move_to_trash_without_touching_legacy_transcripts() {
        let dir = ScratchDir::new("pair");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");
        файл(&dir, "2026-07-30_13-03_chrome.meta.json");
        std::fs::create_dir(dir.join("2026-07-30_13-03_chrome.transcript")).unwrap();
        файл(
            &dir.join("2026-07-30_13-03_chrome.transcript"),
            "выжимка.md",
        );

        delete_recording(&dir, "2026-07-30_13-03_chrome").expect("удаление");

        assert!(!dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.system.wav").exists());
        assert!(dir.join("2026-07-30_13-03_chrome.transcript").exists());
        assert!(
            !dir.join("2026-07-30_13-03_chrome.meta.json").exists(),
            "файл-спутник обязан уехать вместе со всей остальной записью, \
             иначе после удаления в папке остаётся сирота"
        );
    }

    #[test]
    fn отсутствие_всех_четырёх_это_ошибка() {
        let dir = ScratchDir::new("empty");
        let err = delete_recording(&dir, "2026-07-30_13-03_chrome").unwrap_err();
        assert!(err.contains("не найдена"), "текст ошибки: {err}");
    }
}
