//! Переименование готовой записи на диске.
//!
//! Живёт в GUI, а не в ядре: ядро отвечает за то, как запись СОЗДАЁТСЯ, а это
//! управление уже созданным. Чистая арифметика имён при этом в ядре
//! (`storage::rename_tail`) — она нужна обоим и тестируется без файлов.

use meeting_recorder::storage::rename_tail;
use std::path::{Path, PathBuf};

/// Audio and metadata move together to preserve duration and Callabo history.
/// Legacy transcript folders remain at their original location.
const SUFFIXES: [&str; 5] = [
    ".wav",
    ".mic.wav",
    ".system.wav",
    ".meta.json",
    ".callabo.json",
];

/// Переименовывает запись целиком. Возвращает новую основу имени.
///
/// Сначала проверяются ВСЕ целевые имена, и только потом что-либо двигается:
/// иначе отказ на второй дорожке оставил бы половину записи переименованной, а
/// половину нет — состояние, из которого пользователь не выберется руками, не
/// зная правил склейки.
///
/// Переименование не меняет месячную папку: дата в префиксе неизменяема, а
/// именно она эту папку и задаёт.
pub fn rename_recording(dir: &Path, base: &str, new_tail: &str) -> Result<String, String> {
    let new_base = rename_tail(base, new_tail).ok_or_else(|| {
        format!("не удалось построить имя из «{new_tail}»: пустое или имя записи чужое")
    })?;
    if new_base == base {
        return Ok(new_base);
    }

    // Что существует и куда поедет.
    let pairs: Vec<(PathBuf, PathBuf)> = SUFFIXES
        .iter()
        .map(|s| {
            (
                dir.join(format!("{base}{s}")),
                dir.join(format!("{new_base}{s}")),
            )
        })
        .filter(|(from, _)| from.exists())
        .collect();

    if pairs.is_empty() {
        return Err(format!("запись «{base}» не найдена в {}", dir.display()));
    }
    for (_, to) in &pairs {
        if to.exists() {
            return Err(format!(
                "имя «{new_base}» занято: {} уже существует",
                to.display()
            ));
        }
    }

    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in &pairs {
        match std::fs::rename(from, to) {
            Ok(()) => done.push((from.clone(), to.clone())),
            Err(e) => {
                // Откат: вернуть уже переименованное. Ошибку самого отката
                // здесь МОЛЧА не проглатываем: если она есть, наверх должна
                // уйти не только первая причина, но и факт, что откат не
                // довёл дело до конца — иначе человек прочитает «не удалось
                // переименовать» и решит, что диск не тронут, хотя часть
                // дорожек могла остаться висеть под новым именем.
                let rollback_failures: Vec<String> = done
                    .iter()
                    .rev()
                    .filter_map(|(from_done, to_done)| {
                        std::fs::rename(to_done, from_done).err().map(|re| {
                            format!("{} → {}: {re}", to_done.display(), from_done.display())
                        })
                    })
                    .collect();
                let reason = format!("не удалось переименовать {}: {e}", from.display());
                return Err(if rollback_failures.is_empty() {
                    reason
                } else {
                    format!(
                        "{reason}; ОТКАТ НЕ УДАЛСЯ, на диске остался частично переименованный набор: {}",
                        rollback_failures.join("; ")
                    )
                });
            }
        }
    }
    Ok(new_base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callabo_receipt_moves_with_audio() {
        let dir = ScratchDir::new("callabo");
        файл(&dir, "2026-07-30_13-03_chrome.wav");
        файл(&dir, "2026-07-30_13-03_chrome.callabo.json");
        rename_recording(&dir, "2026-07-30_13-03_chrome", "meeting").unwrap();
        assert!(dir.join("2026-07-30_13-03_meeting.callabo.json").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.callabo.json").exists());
    }
    use std::path::PathBuf;

    /// Уникальный временный каталог без внешних зависимостей. Удаляется в Drop,
    /// даже если тест упал.
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
            let path = std::env::temp_dir().join(format!("mr-rename-{tag}-{pid}-{nanos}-{n}"));
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

    #[test]
    fn переименовываются_обе_дорожки() {
        let dir = ScratchDir::new("pair");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");

        let новое =
            rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").expect("переименование");

        assert_eq!(новое, "2026-07-30_13-03_артем");
        assert!(dir.join("2026-07-30_13-03_артем.mic.wav").exists());
        assert!(dir.join("2026-07-30_13-03_артем.system.wav").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }

    #[test]
    fn single_multichannel_wav_moves_with_metadata() {
        let dir = ScratchDir::new("multichannel");
        файл(&dir, "2026-07-30_13-03_chrome.wav");
        файл(&dir, "2026-07-30_13-03_chrome.meta.json");
        rename_recording(&dir, "2026-07-30_13-03_chrome", "meeting").unwrap();
        assert!(dir.join("2026-07-30_13-03_meeting.wav").exists());
        assert!(dir.join("2026-07-30_13-03_meeting.meta.json").exists());
        assert!(!dir.join("2026-07-30_13-03_chrome.wav").exists());
    }

    #[test]
    fn legacy_transcript_is_left_untouched_when_audio_is_renamed() {
        let dir = ScratchDir::new("transcript");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");
        std::fs::create_dir(dir.join("2026-07-30_13-03_chrome.transcript")).unwrap();
        файл(
            &dir.join("2026-07-30_13-03_chrome.transcript"),
            "выжимка.md",
        );

        rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").expect("переименование");

        assert!(dir
            .join("2026-07-30_13-03_chrome.transcript")
            .join("выжимка.md")
            .exists());
    }

    #[test]
    fn отсутствие_транскрипта_не_ломает_переименование() {
        let dir = ScratchDir::new("no-transcript");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert!(rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").is_ok());
        assert!(dir.join("2026-07-30_13-03_артем.mic.wav").exists());
    }

    /// Автоподбор `_2` уместен там, где имя выбирает машина. Здесь его выбрал
    /// человек, и подмена за спиной означала бы, что запись потом ищут не там.
    #[test]
    fn занятое_имя_это_ошибка_а_не_автоподбор() {
        let dir = ScratchDir::new("taken");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_артем.mic.wav");

        let err = rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").unwrap_err();

        assert!(err.contains("занято"), "текст ошибки: {err}");
        assert!(
            dir.join("2026-07-30_13-03_chrome.mic.wav").exists(),
            "при отказе на диске ничего не меняется"
        );
        assert!(!dir.join("2026-07-30_13-03_артем.system.wav").exists());
    }

    #[test]
    fn пустой_хвост_отвергается_до_касания_диска() {
        let dir = ScratchDir::new("empty");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert!(rename_recording(&dir, "2026-07-30_13-03_chrome", "  ").is_err());
        assert!(dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }

    #[test]
    fn переименование_в_то_же_имя_это_не_ошибка() {
        let dir = ScratchDir::new("same");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        assert_eq!(
            rename_recording(&dir, "2026-07-30_13-03_chrome", "chrome").expect("то же имя"),
            "2026-07-30_13-03_chrome"
        );
        assert!(dir.join("2026-07-30_13-03_chrome.mic.wav").exists());
    }

    /// Ревью Task 10, находка 2: все прежние тесты падали на upfront-проверке
    /// `to.exists()`, ни один не доходил до настоящего `fs::rename`, значит
    /// ветку отката не проверял никто. Здесь первая дорожка (`.mic.wav`)
    /// переименовывается по-настоящему, а вторая (`.system.wav`) держится
    /// заблокированной хендлом без `FILE_SHARE_DELETE`: обычный
    /// `std::fs::File::open` эту дорожку не заблокировал бы — Rust на Windows
    /// включает `FILE_SHARE_DELETE` в шаринг по умолчанию именно для того,
    /// чтобы файл можно было переименовать/удалить, пока где-то открыт его
    /// хендл. Здесь шаринг сознательно урезан до одного лишь чтения, и
    /// `MoveFileExW` внутри `fs::rename` откажет с sharing violation уже
    /// ПОСЛЕ того, как первая дорожка успешно переехала — ровно та ветка,
    /// которую нужно было проверить.
    ///
    /// Тест Windows-only не по недосмотру, а по устройству: он строится на
    /// том, что `MoveFileExW` откажет по sharing violation. У POSIX такого
    /// понятия нет вовсе — открытый хендл там переименованию не мешает,
    /// `fs::rename` спокойно бы отработал, и ветка отката снова осталась бы
    /// непройденной, только теперь ещё и с зелёным тестом. Воспроизвести её на
    /// macOS можно разве что правами на каталог, и это была бы уже другая
    /// проверка; заводить её — отдельная задача, а не побочный эффект порта.
    #[cfg(target_os = "windows")]
    #[test]
    fn отказ_второй_дорожки_откатывает_первую() {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let dir = ScratchDir::new("rollback");
        файл(&dir, "2026-07-30_13-03_chrome.mic.wav");
        файл(&dir, "2026-07-30_13-03_chrome.system.wav");

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ) // ни FILE_SHARE_WRITE, ни FILE_SHARE_DELETE
            .open(dir.join("2026-07-30_13-03_chrome.system.wav"))
            .expect("открыть system-дорожку с эксклюзивной (без delete) блокировкой");

        let err = rename_recording(&dir, "2026-07-30_13-03_chrome", "артем").unwrap_err();
        drop(lock); // снять блокировку сразу — дальше идёт только чтение файловой системы

        assert!(
            err.contains("не удалось переименовать"),
            "текст ошибки: {err}"
        );
        assert!(
            dir.join("2026-07-30_13-03_chrome.mic.wav").exists(),
            "первая дорожка обязана откатиться на старое имя"
        );
        assert!(
            !dir.join("2026-07-30_13-03_артем.mic.wav").exists(),
            "новое имя первой дорожки не должно остаться висеть после отката"
        );
        assert!(
            dir.join("2026-07-30_13-03_chrome.system.wav").exists(),
            "вторая дорожка так и не переехала — она и была заблокирована"
        );
    }
}
