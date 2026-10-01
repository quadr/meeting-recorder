use chrono::{DateTime, Local};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    Mic,
    System,
}

impl Track {
    fn suffix(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }
}

pub const SAMPLE_RATE: u32 = 16_000;

/// `2026-07-17_14-30_zoom.mic.wav`
pub fn recording_filename(started: DateTime<Local>, source: &str, track: Track) -> String {
    format!(
        "{}_{}.{}.wav",
        started.format("%Y-%m-%d_%H-%M"),
        sanitize_source(source),
        track.suffix()
    )
}

/// Максимум для очищенного имени источника в имени файла.
///
/// Полный файл — `{дата}_{время}_{источник}.{дорожка}.wav`. Дата+время+разделитель
/// занимают 17 символов (`2026-07-17_14-30_`), самый длинный суффикс дорожки с
/// расширением — `.system.wav` (11 символов). Итого служебная часть — 28 символов.
/// Предел компонента пути на NTFS — 255 символов (UTF-16 code units), так что запас
/// огромный в любом случае; 100 символов — не попытка впритык влезть в лимит NTFS, а
/// разумная граница для имени процесса как такового (реальные имена процессов даже с
/// длинными кириллическими названиями вроде «Яндекс.Телемост» на порядок короче), при
/// этом с большим запасом на конкатенацию нескольких слов через дефис.
const MAX_SOURCE_LEN: usize = 100;

/// Ширина префикса `YYYY-MM-DD_HH-MM_` — 17 символов, все ASCII.
pub const PREFIX_LEN: usize = 17;

/// Форма префикса: `d` — цифра, остальное — литерал.
const PREFIX_SHAPE: &[u8] = b"dddd-dd-dd_dd-dd_";

fn prefix_ok(b: &[u8]) -> bool {
    b.len() >= PREFIX_LEN
        && PREFIX_SHAPE
            .iter()
            .zip(b)
            .all(|(shape, c)| match shape {
                b'd' => c.is_ascii_digit(),
                lit => c == lit,
            })
}

/// Разбирает основу имени на неизменяемый префикс и редактируемый хвост.
///
/// Префикс — несущая конструкция: по нему идёт сортировка списка, склейка пары
/// дорожек и выбор месячной папки. Поэтому он фиксированной ширины и проверяется
/// по форме, а не «до последнего подчёркивания» — иначе `zoom_2` разъехался бы
/// на префикс `..._zoom_` и хвост `2`.
///
/// `None` — имя не наше (чужой файл в каталоге) или хвост пуст.
pub fn split_name(base: &str) -> Option<(String, String)> {
    if !prefix_ok(base.as_bytes()) {
        return None;
    }
    // Резать по PREFIX_LEN безопасно: первые 17 байт проверены как ASCII,
    // значит граница символа здесь совпадает с границей байта. Хвост при этом
    // может быть каким угодно юникодом.
    let (prefix, tail) = base.split_at(PREFIX_LEN);
    if tail.is_empty() {
        return None;
    }
    Some((prefix.to_string(), tail.to_string()))
}

/// Новая основа имени с заменённым хвостом.
///
/// `None` — исходное имя не разбирается или новый хвост после очистки пуст.
pub fn rename_tail(base: &str, new_tail: &str) -> Option<String> {
    let (prefix, _) = split_name(base)?;
    let tail = sanitize_tail(new_tail)?;
    Some(format!("{prefix}{tail}"))
}

/// Очистка произвольной строки под хвост имени файла. `None` — после очистки
/// ничего не осталось.
///
/// Юникодные буквы сохраняются: имена процессов на кириллице (у «Яндекс.Телемост»)
/// реальны, и человек тоже вправе назвать запись по-русски.
fn sanitize_tail(raw: &str) -> Option<String> {
    // Сначала lowercase, потом срез `.exe` — а не наоборот. При срезе ДО
    // lowercase смешанный регистр (`Zoom.EXE`) не совпадал с литералом
    // `.exe`, срез молча не срабатывал, и `.exe`/`.EXE` уезжали в имя файла
    // как обычные символы (`zoom-exe`), хотя `canonical_source`
    // (`src/detector/mod.rs`) для того же самого имени показывал «Zoom» —
    // правило здесь обязано быть тем же самым, что и там.
    let lower = raw.to_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    // схлопнуть повторы дефисов и обрезать по краям
    let mut out = String::with_capacity(cleaned.len());
    for c in cleaned.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    let out = out.trim_matches('-');
    // обрезаем по границе символа, а не байта — хвост может быть кириллицей
    let truncated: String = out.chars().take(MAX_SOURCE_LEN).collect();
    let truncated = truncated.trim_end_matches('-');
    if truncated.is_empty() {
        None
    } else {
        Some(truncated.to_string())
    }
}

/// То же, но для автоименования: пустой результат заменяется на `unknown`,
/// чтобы источник в имени файла не терялся молча.
fn sanitize_source(source: &str) -> String {
    sanitize_tail(source).unwrap_or_else(|| "unknown".to_string())
}

/// Папка записи по времени её НАЧАЛА: `<root>/2026-07`.
///
/// Именно по началу, а не по моменту закрытия файла: встреча, начатая 31 июля
/// в 23:50, должна целиком лежать в июле. Считай мы папку при финализации,
/// дорожки такой встречи могли бы разъехаться по разным месяцам.
///
/// Гранулярность месяц, а не день: при 1-3 встречах в день это 30-60 файлов на
/// папку — читаемо, тогда как папка на каждый день дала бы сотни папок по паре
/// файлов.
pub fn month_dir(root: &Path, started: DateTime<Local>) -> PathBuf {
    root.join(started.format("%Y-%m").to_string())
}

/// Инкрементальная запись 16 кГц моно 16 бит WAV.
pub struct WavSink {
    writer: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
}

impl WavSink {
    pub fn create(dir: &Path, filename: &str) -> Result<Self, hound::Error> {
        Self::create_channels(dir, filename, 1)
    }

    pub fn create_channels(dir: &Path, filename: &str, channels: u16) -> Result<Self, hound::Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(filename);
        let spec = hound::WavSpec {
            channels,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec)?;
        Ok(Self { writer, path })
    }

    pub fn write(&mut self, samples: &[i16]) -> Result<(), hound::Error> {
        for &s in samples {
            self.writer.write_sample(s)?;
        }
        Ok(())
    }

    /// Дописывает WAV-заголовок с реальной длиной. Без этого файл битый.
    pub fn finalize(self) -> Result<PathBuf, hound::Error> {
        self.writer.finalize()?;
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn момент() -> chrono::DateTime<chrono::Local> {
        chrono::Local.with_ymd_and_hms(2026, 7, 17, 14, 30, 0).unwrap()
    }

    #[test]
    fn имя_для_дорожки_микрофона() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    #[test]
    fn имя_для_системной_дорожки() {
        assert_eq!(
            recording_filename(момент(), "zoom", Track::System),
            "2026-07-17_14-30_zoom.system.wav"
        );
    }

    #[test]
    fn ручная_запись_помечается_как_manual() {
        assert_eq!(
            recording_filename(момент(), "manual", Track::Mic),
            "2026-07-17_14-30_manual.mic.wav"
        );
    }

    #[test]
    fn имя_процесса_чистится_от_exe_и_регистра() {
        assert_eq!(
            recording_filename(момент(), "Zoom.exe", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
    }

    /// Гвоздь задачи: смешанный регистр расширения (`Zoom.Exe`) раньше не
    /// совпадал с литералом `.exe` при срезе ДО lowercase — срез молча не
    /// срабатывал, и в имени файла оставалось `zoom-exe` вместо `zoom`, хотя
    /// `canonical_source` (`src/detector/mod.rs`) для того же процесса уже
    /// узнавал «Zoom». Правило теперь то же самое в обеих функциях: сначала
    /// lowercase, потом срез.
    #[test]
    fn смешанный_регистр_exe_расширения_срезается() {
        assert_eq!(
            recording_filename(момент(), "Zoom.Exe", Track::Mic),
            "2026-07-17_14-30_zoom.mic.wav"
        );
        assert_eq!(
            rename_tail("2026-07-30_13-03_chrome", "Zoom.Exe"),
            Some("2026-07-30_13-03_zoom".to_string())
        );
    }

    #[test]
    fn небезопасные_символы_в_имени_процесса_заменяются() {
        assert_eq!(
            recording_filename(момент(), "My App v2.exe", Track::Mic),
            "2026-07-17_14-30_my-app-v2.mic.wav"
        );
    }

    #[test]
    fn кириллическое_имя_процесса_не_схлопывается_в_пустоту() {
        assert_eq!(
            recording_filename(момент(), "Яндекс.Телемост.exe", Track::Mic),
            "2026-07-17_14-30_яндекс-телемост.mic.wav"
        );
    }

    #[test]
    fn один_только_exe_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), ".exe", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn источник_из_одних_дефисов_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), "---", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn пустой_источник_даёт_unknown() {
        assert_eq!(
            recording_filename(момент(), "", Track::Mic),
            "2026-07-17_14-30_unknown.mic.wav"
        );
    }

    #[test]
    fn длинный_источник_обрезается_по_лимиту_без_висящего_дефиса() {
        // 99 'a' + разделитель, который попадает ровно на границу обрезки —
        // после take(MAX_SOURCE_LEN) висящий дефис должен быть срезан отдельно.
        let long_source = format!("{}.{}", "a".repeat(99), "b".repeat(50));
        let result = sanitize_source(&long_source);
        assert_eq!(result, "a".repeat(99));
        assert!(result.len() <= MAX_SOURCE_LEN);
    }

    #[test]
    fn источник_ровно_на_лимите_не_обрезается() {
        let source = "a".repeat(MAX_SOURCE_LEN);
        assert_eq!(sanitize_source(&source), source);
    }

    #[test]
    fn имя_разбирается_на_префикс_и_хвост() {
        assert_eq!(
            split_name("2026-07-30_13-03_chrome"),
            Some(("2026-07-30_13-03_".to_string(), "chrome".to_string()))
        );
    }

    #[test]
    fn суффикс_повтора_это_часть_хвоста() {
        assert_eq!(
            split_name("2026-07-30_13-03_chrome_2"),
            Some(("2026-07-30_13-03_".to_string(), "chrome_2".to_string()))
        );
    }

    #[test]
    fn чужое_имя_не_разбирается() {
        assert_eq!(split_name("заметки"), None);
        assert_eq!(split_name("2026-07-30_13-03_"), None, "пустой хвост");
        assert_eq!(split_name("2026-07-30 13-03_chrome"), None, "пробел вместо _");
        assert_eq!(split_name("20260730_1303_chrome"), None, "нет дефисов");
        assert_eq!(split_name(""), None);
    }

    /// Первые 17 символов префикса — ASCII, но хвост может быть кириллицей.
    /// Разбор обязан резать по границе символа, а не по байту.
    #[test]
    fn кириллический_хвост_разбирается_без_паники() {
        assert_eq!(
            split_name("2026-07-30_13-03_разговор"),
            Some(("2026-07-30_13-03_".to_string(), "разговор".to_string()))
        );
    }

    #[test]
    fn переименование_меняет_только_хвост() {
        assert_eq!(
            rename_tail("2026-07-30_13-03_chrome", "Разговор с Артемом"),
            Some("2026-07-30_13-03_разговор-с-артемом".to_string())
        );
    }

    #[test]
    fn суффикс_повтора_переименованием_стирается() {
        assert_eq!(
            rename_tail("2026-07-30_13-03_chrome_2", "демо"),
            Some("2026-07-30_13-03_демо".to_string()),
            "хвост заменяется целиком, вместе с номером повтора"
        );
    }

    /// Пустой хвост — ошибка, а не подстановка `unknown`. При автоименовании
    /// источник берёт машина и подставить туда нечего; здесь поле стёр человек
    /// и обязан это увидеть.
    #[test]
    fn пустой_новый_хвост_это_ошибка() {
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", ""), None);
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", "---"), None);
        assert_eq!(rename_tail("2026-07-30_13-03_chrome", "   "), None);
    }

    #[test]
    fn слишком_длинный_хвост_обрезается() {
        let длинный = "я".repeat(200);
        let out = rename_tail("2026-07-30_13-03_chrome", &длинный).expect("хвост непустой");
        let (_, хвост) = split_name(&out).expect("результат разбирается обратно");
        assert_eq!(хвост.chars().count(), MAX_SOURCE_LEN);
    }

    #[test]
    fn чужое_имя_не_переименовывается() {
        assert_eq!(rename_tail("заметки", "новое"), None);
    }

    #[test]
    fn месячная_папка_из_даты_записи() {
        assert_eq!(
            month_dir(Path::new(r"C:\Recordings"), момент()),
            Path::new(r"C:\Recordings").join("2026-07")
        );
    }

    /// Месяц берётся из времени НАЧАЛА записи. Встреча, начатая 31 июля в 23:50
    /// и законченная 1 августа, целиком лежит в июле — иначе пара дорожек
    /// разъехалась бы по разным папкам, если бы папку считали при закрытии.
    #[test]
    fn граница_месяца_берётся_по_началу_записи() {
        let конец_июля = chrono::Local
            .with_ymd_and_hms(2026, 7, 31, 23, 50, 0)
            .unwrap();
        assert_eq!(
            month_dir(Path::new(r"C:\R"), конец_июля),
            Path::new(r"C:\R").join("2026-07")
        );
    }

    #[test]
    fn однозначный_месяц_дополняется_нулём() {
        let январь = chrono::Local.with_ymd_and_hms(2027, 1, 5, 9, 0, 0).unwrap();
        assert_eq!(
            month_dir(Path::new(r"C:\R"), январь),
            Path::new(r"C:\R").join("2027-01")
        );
    }

    /// Уникальный временный каталог без внешних зависимостей (без `tempfile`/`rand`):
    /// счётчик + pid процесса + наносекунды с эпохи. Каталог удаляется в Drop, даже
    /// если тест упал — не оставляем мусор в `%TEMP%`.
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
            let path = std::env::temp_dir().join(format!(
                "meeting-recorder-test-{tag}-{pid}-{nanos}-{n}"
            ));
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
    fn wav_sink_round_trip_читает_записанные_сэмплы() {
        let dir = ScratchDir::new("round-trip");
        let samples: Vec<i16> = vec![0, 1, -1, i16::MAX, i16::MIN, 12345, -12345];

        let mut sink = WavSink::create(&dir, "test.wav").expect("создание WavSink");
        sink.write(&samples).expect("запись сэмплов");
        let path = sink.finalize().expect("финализация WavSink");

        let mut reader = hound::WavReader::open(&path).expect("открыть WAV для чтения");
        let spec = reader.spec();
        assert_eq!(spec.sample_rate, SAMPLE_RATE);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);

        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");
        assert_eq!(read_samples, samples);
    }

    #[test]
    fn wav_sink_несколько_write_дают_непрерывный_поток_сэмплов() {
        let dir = ScratchDir::new("chunked-writes");
        let chunk1: Vec<i16> = (0..100).collect();
        let chunk2: Vec<i16> = (100..250).collect();
        let chunk3: Vec<i16> = vec![-1, -2, -3];

        let mut sink = WavSink::create(&dir, "chunked.wav").expect("создание WavSink");
        sink.write(&chunk1).expect("запись чанка 1");
        sink.write(&chunk2).expect("запись чанка 2");
        sink.write(&chunk3).expect("запись чанка 3");
        let path = sink.finalize().expect("финализация WavSink");

        let mut reader = hound::WavReader::open(&path).expect("открыть WAV для чтения");
        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");

        let mut expected = Vec::new();
        expected.extend_from_slice(&chunk1);
        expected.extend_from_slice(&chunk2);
        expected.extend_from_slice(&chunk3);
        assert_eq!(read_samples, expected);
    }

    #[test]
    fn wav_sink_дропнутый_без_finalize_всё_равно_читается() {
        // hound::WavWriter::drop() сам вызывает update_header(), если finalize()
        // не был вызван явно — подстраховка на случай раннего return без явного
        // finalize(). Тест фиксирует это поведение текущей версии hound: если
        // зависимость обновится и подстраховка пропадёт, тест упадёт.
        //
        // Важно: это НЕ защищает от жёсткого краша процесса (SIGKILL/паника с
        // abort/отключение питания) — там Drop вообще не выполняется, и в
        // заголовке останется нулевая длина данных, хотя сами сэмплы на диске
        // будут. Это и есть реальный необнаружаемый юнит-тестом риск,
        // описанный в ревью: файл существует, весит норм, но играет тишину.
        let dir = ScratchDir::new("drop-without-finalize");
        let samples: Vec<i16> = vec![10, -10, 20, -20];
        let path = dir.join("dropped.wav");
        {
            let mut sink = WavSink::create(&dir, "dropped.wav").expect("создание WavSink");
            sink.write(&samples).expect("запись сэмплов");
            // sink роняется в конце блока без явного finalize()
        }

        let mut reader = hound::WavReader::open(&path)
            .expect("файл должен читаться как валидный WAV даже без явного finalize()");
        let read_samples: Vec<i16> = reader
            .samples::<i16>()
            .collect::<Result<_, _>>()
            .expect("прочитать сэмплы");
        assert_eq!(read_samples, samples);
    }
}
