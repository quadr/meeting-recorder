//! Автоматическая чистка старого аудио: звук уезжает в корзину, расшифровка
//! остаётся всегда.
//!
//! Разбор «кого трогать» отделён от файлового ввода-вывода по той же причине,
//! что `group_recordings` в `main.rs`: единственное, что здесь можно сломать
//! незаметно, — это правило «кого не трогать», а оно проверяется без диска.
//! Чтение каталога и сама отправка в корзину остаются в `main.rs`/`delete.rs`.

use chrono::{DateTime, Local, NaiveDateTime};
use std::path::{Path, PathBuf};

/// Разобрать `YYYY-MM-DD_HH-MM` из начала основы имени (см.
/// `storage::recording_filename` в ядре — формат общий на всё приложение).
/// `None` — основа не начинается с даты; такую запись чистка не трогает
/// вовсе, потому что не с чем сравнивать возраст, а не потому что «наверное,
/// старая».
fn started_at(base: &str) -> Option<DateTime<Local>> {
    let prefix = base.get(0..16)?; // "2026-07-17_14-30"
    let naive = NaiveDateTime::parse_from_str(prefix, "%Y-%m-%d_%H-%M").ok()?;
    naive.and_local_timezone(Local).single()
}

/// Одна запись с точки зрения чистки — ровно те факты, что нужны для решения
/// «пора/не пора», без самих файлов. Отдельный тип, а не переиспользование
/// `main::Recording`: тому полю приходится нести ещё имбаланс громкости и
/// длительность, которые чистке не нужны вовсе, и тащить их в чистую функцию
/// незачем.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub folder: Option<String>,
    pub base: String,
    /// Хотя бы одна дорожка ещё лежит на диске — чистить нечего, если обе уже
    /// уехали (или их не было вовсе).
    pub has_wav: bool,
    /// Расшифровка готова. Без неё чистка не трогает запись НИКОГДА: с этой
    /// стороны звук — единственное, что осталось от встречи.
    pub has_transcript: bool,
    /// Идёт прямо сейчас или расшифровывается (ждёт очереди или уже в ней).
    /// И то и другое значит «звук ещё нужен как есть», удалять нельзя.
    pub busy: bool,
}

/// Отобрать записи, у которых пора чистить звук.
///
/// Чистая функция: возраст не читается с диска (`mtime` можно подменить
/// вручную и получить враньё про дату встречи), а разбирается из имени — как
/// и вся остальная сортировка списка (см. `main.rs::group_recordings` и
/// `момент_записи` в `ui/main.js`, тот же принцип на фронтенде).
pub fn due_for_cleanup(
    list: &[Candidate],
    days: u32,
    now: DateTime<Local>,
) -> Vec<(Option<String>, String)> {
    list.iter()
        .filter(|r| r.has_wav && r.has_transcript && !r.busy)
        .filter_map(|r| {
            let started = started_at(&r.base)?;
            let age_days = now.signed_duration_since(started).num_days();
            (age_days >= i64::from(days)).then(|| (r.folder.clone(), r.base.clone()))
        })
        .collect()
}

/// Отправить в корзину только звук записи — обе дорожки, не трогая папку
/// расшифровки. Разница с `delete::delete_recording` ровно в этом: та стирает
/// запись целиком, эта — только то, что автоочистке разрешено трогать.
///
/// Не ошибка, если дорожек уже нет: значит, звук этой записи уже вычищен
/// раньше (например, ручным запуском чистки при уменьшенном сроке), и второй
/// проход — не повод жаловаться.
pub fn delete_audio(dir: &Path, base: &str) -> Result<(), String> {
    let existing: Vec<PathBuf> = [".wav", ".mic.wav", ".system.wav"]
        .iter()
        .map(|s| dir.join(format!("{base}{s}")))
        .filter(|p| p.exists())
        .collect();
    if existing.is_empty() {
        return Ok(());
    }
    trash::delete_all(&existing).map_err(|e| format!("не удалось отправить звук в корзину: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Уникальный временный каталог без внешних зависимостей. Удаляется в
    /// Drop, даже если тест упал. Тот же приём, что в `delete.rs`.
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
            let path = std::env::temp_dir().join(format!("mr-retention-{tag}-{pid}-{nanos}-{n}"));
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

    /// Гвоздь задачи «длительность переживает автоочистку»: `delete_audio`
    /// обязан забрать только дорожки, оставив файл-спутник `.meta.json`
    /// на месте — иначе длительность, которую он хранит, терялась бы вместе
    /// со звуком, хотя вся её ценность как раз в том, чтобы этого не
    /// происходило.
    ///
    /// Как и в `delete.rs`, корзина в песочнице CI недоступна — гоняется
    /// руками.
    #[test]
    #[ignore = "трогает системную корзину — гонять руками, не в CI"]
    fn чистка_забирает_только_wav_а_meta_json_остаётся() {
        let dir = ScratchDir::new("meta-survives");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");
        файл(&dir, "2026-07-30_13-03_chrome.meta.json");
        std::fs::create_dir(dir.join("2026-07-30_13-03_chrome.transcript")).unwrap();

        delete_audio(&dir, "2026-07-30_13-03_chrome").expect("чистка звука");

        assert!(!dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.system.wav").exists());
        assert!(
            dir.join("2026-07-30_13-03_chrome.meta.json").exists(),
            "файл-спутник с длительностью не должен уезжать вместе со звуком"
        );
        assert!(
            dir.join("2026-07-30_13-03_chrome.transcript").exists(),
            "расшифровка чисткой звука не трогается вовсе"
        );
    }

    /// CI-safe спутник теста выше: реальный `delete_audio`, но по ветке,
    /// которая до `trash::delete_all` не доходит вовсе (обеих дорожек уже
    /// нет) — гоняется без системной корзины и всё равно бьёт по-настоящему
    /// в исходный код функции, а не в его копию.
    #[test]
    fn delete_audio_без_дорожек_не_ошибка_и_не_трогает_корзину() {
        let dir = ScratchDir::new("nothing-to-clean");
        файл(&dir, "2026-07-30_13-03_chrome.meta.json");

        delete_audio(&dir, "2026-07-30_13-03_chrome").expect("уже вычищено — не ошибка");

        assert!(
            dir.join("2026-07-30_13-03_chrome.meta.json").exists(),
            "нечего было чистить — файл-спутник и подавно не тронут"
        );
    }

    fn момент(д: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, д, 14, 30, 0).unwrap()
    }

    fn кандидат(base: &str, has_wav: bool, has_transcript: bool, busy: bool) -> Candidate {
        Candidate { folder: Some("2026-07".into()), base: base.into(), has_wav, has_transcript, busy }
    }

    #[test]
    fn свежую_запись_чистка_не_трогает() {
        let список = vec![кандидат("2026-08-01_10-00_zoom", true, true, false)];
        // 5 дней спустя при пороге в 7 — рано.
        assert!(due_for_cleanup(&список, 7, момент(6)).is_empty());
    }

    #[test]
    fn достаточно_старую_запись_с_расшифровкой_чистка_забирает() {
        let список = vec![кандидат("2026-08-01_10-00_zoom", true, true, false)];
        let due = due_for_cleanup(&список, 7, момент(8));
        assert_eq!(due, vec![(Some("2026-07".to_string()), "2026-08-01_10-00_zoom".to_string())]);
    }

    /// Ровно на границе (7 дней и не позже) — уже пора: "старше 7 дней" в
    /// интерфейсе (`retention.d7`) читается как «от 7 дней и дальше», не как
    /// «строго больше семи целых суток».
    #[test]
    fn граница_срока_это_уже_пора() {
        let список = vec![кандидат("2026-08-01_10-00_zoom", true, true, false)];
        assert_eq!(due_for_cleanup(&список, 7, момент(8)).len(), 1);
    }

    /// Без расшифровки от встречи не осталось бы ничего вообще — чистка
    /// обязана оставить такую запись в покое, сколько бы она ни лежала.
    #[test]
    fn запись_без_расшифровки_не_трогается_никогда() {
        let список = vec![кандидат("2026-01-01_10-00_zoom", true, false, false)];
        assert!(due_for_cleanup(&список, 7, момент(30)).is_empty());
    }

    /// Идущая запись или расшифровка — звук ещё нужен как есть, независимо от
    /// возраста имени (случай маловероятный, но проверяется правило, а не
    /// вероятность).
    #[test]
    fn занятая_запись_не_трогается() {
        let список = vec![кандидат("2026-01-01_10-00_zoom", true, true, true)];
        assert!(due_for_cleanup(&список, 7, момент(30)).is_empty());
    }

    /// Звук уже вычищен раньше — второй проход чистки не должен пытаться
    /// удалить то, чего нет, и не должен отдавать её как «due» повторно.
    #[test]
    fn уже_вычищенная_запись_не_возвращается_снова() {
        let список = vec![кандидат("2026-01-01_10-00_zoom", false, true, false)];
        assert!(due_for_cleanup(&список, 7, момент(30)).is_empty());
    }

    #[test]
    fn имя_без_разборчивой_даты_пропускается_а_не_падает() {
        let список = vec![кандидат("чужой-файл-без-даты", true, true, false)];
        assert!(due_for_cleanup(&список, 0, момент(30)).is_empty());
    }
}
