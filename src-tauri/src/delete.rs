//! Удаление готовой записи: обе дорожки и папка расшифровки уезжают в
//! системную корзину, а не стираются насовсем.
//!
//! Живёт в GUI, а не в ядре — тем же аргументом, что `rename.rs`: ядро
//! отвечает за то, как запись СОЗДАЁТСЯ, а это управление уже созданным.
//! В корзину, а не `fs::remove_file`, намеренно: в приложении лежат
//! единственные копии рабочих разговоров, и безвозвратное удаление одним
//! кликом недопустимо — см. `trash` в `Cargo.toml`.

use std::path::{Path, PathBuf};

/// Что уезжает вместе с записью: обе дорожки, папка транскрипта и файл-
/// спутник с длительностью (`<основа>.meta.json`, пишет `close_sinks` в ядре
/// — см. `src/app.rs::SinkFactory::write_meta`).
///
/// `.meta.json` не входит в `rename::SUFFIXES`: переименование меняет только
/// хвост имени, а внутри самого файла хвост не хранится — переезжать вместе
/// с остальными ему незачем, обновлять нечего. Здесь же удаление — если не
/// забрать его вместе с записью, он останется на диске сиротой без единой
/// дорожки и без расшифровки рядом.
const SUFFIXES: [&str; 6] = [".wav", ".mic.wav", ".system.wav", ".transcript", ".meta.json", ".callabo.json"];

/// Отправить запись в корзину целиком: обе дорожки и папку расшифровки, если
/// она есть.
///
/// Не ошибка, если какого-то из трёх нет на диске — забирается всё, что
/// нашлось (ровно так же ведёт себя `rename_recording`). Ошибка — только если
/// не нашлось совсем ничего: удалять уже нечего, и молчаливый «успех» скрыл
/// бы, что запись, которую попросили стереть, на этом месте не жила.
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
    fn обе_дорожки_транскрипт_и_meta_json_уезжают_в_корзину() {
        let dir = ScratchDir::new("pair");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");
        файл(&dir, "2026-07-30_13-03_chrome.meta.json");
        std::fs::create_dir(dir.join("2026-07-30_13-03_chrome.transcript")).unwrap();
        файл(&dir.join("2026-07-30_13-03_chrome.transcript"), "выжимка.md");

        delete_recording(&dir, "2026-07-30_13-03_chrome").expect("удаление");

        assert!(!dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.system.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.transcript").exists());
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
