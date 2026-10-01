//! Слышит ли микрофон владельца — по его дорожке готовой записи.
//!
//! Существует потому, что выбор устройства не доказывает, что устройство слышит
//! владельца. Отказ здесь маскируется под успех: файл есть, весит сколько
//! положено, открывается и играет — просто голоса в нём почти нет, и обнаружится
//! это только на транскрипте, через день.
//!
//! Считается по ОДНОЙ дорожке, без сравнения с системной. Две попытки сравнивать
//! (RMS файлов, затем уровни речи) ставили пометку на нормальную запись Ани
//! 08.09: её микрофон честно на 25 дБ тише лупбека звонка — потому что громкость
//! лупбека зависит от колонок собеседника, а не от её микрофона. Речь при этом
//! стояла на 18 дБ над собственным шумом и расшифровывалась без потерь. Вопрос
//! пометки — «слышно ли голос», и ответ на него даёт сама дорожка: уровень речи
//! и её отрыв от шумовой полки.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// Пол уровня. Всё, что тише, считается этим значением.
///
/// Пол, а не `-inf`, чтобы арифметика сравнения оставалась обычной: с
/// бесконечностями пришлось бы отдельно разбирать случай «мик молчал совсем»,
/// который как раз и есть самый важный.
pub const FLOOR_DBFS: f32 = -120.0;

/// Ниже какого уровня речь считается слишком тихой, dBFS.
///
/// −50: у Ани 08.09 речь на −43 (слышно нормально), у мёртвого микрофона 30.07
/// весь файл лежал около −60. Whisper нормализует громкость сам, так что
/// граница стоит не там, где «плохо слышно человеку», а там, где голос уже
/// тонет в шуме любого тракта.
pub const MIN_SPEECH_DBFS: f32 = -50.0;

/// Насколько речь должна выделяться над шумовой полкой, дБ.
///
/// 6 дБ — вдвое по амплитуде. Меньше — голос неотличим от шипения, и это
/// «микрофон слышит комнату, но не человека»: воздуховод, чужой вход, кабель
/// без сигнала. У живой записи отрыв 15–25 дБ.
pub const MIN_CONTRAST_DB: f32 = 6.0;

/// Сколько сэмплов в одном окне выборки — четверть секунды при 16 кГц.
///
/// Окно короче слова не нужно, окно длиннее фразы смазало бы паузу с речью и
/// вернуло бы ту самую ошибку, ради которой всё это переписано.
const WINDOW: usize = 4096;

/// Какая доля окон должна быть тише «уровня речи».
///
/// 0.9 — берём громкие 10% окон: у владельца речь занимает 20–30% встречи,
/// и в верхние 10% попадает только она, паузы в оценку не входят. Меньше (0.5)
/// затянуло бы в оценку тишину молчаливого владельца; больше (0.99) —
/// единичные щелчки и пики вместо голоса.
pub const SPEECH_PERCENTILE: f64 = 0.9;

/// Какая доля окон считается «шумовой полкой» — тихие 10%.
///
/// Не медиана: владелец, говорящий большую часть встречи, сдвинул бы медиану
/// в речь, и контраст речи с «полкой» вышел бы нулевым на нормальной записи.
/// Паузы длиннее 10% времени есть в любом разговоре.
pub const NOISE_PERCENTILE: f64 = 0.1;

/// RMS одного окна в dBFS. Пустое окно и цифровой ноль дают [`FLOOR_DBFS`].
fn window_dbfs(sum_sq: f64, counted: usize) -> f32 {
    if counted == 0 {
        return FLOOR_DBFS;
    }
    let rms = (sum_sq / counted as f64).sqrt() / i16::MAX as f64;
    if rms <= 0.0 {
        return FLOOR_DBFS;
    }
    (20.0 * rms.log10()).max(FLOOR_DBFS as f64) as f32
}

/// Дочитать до `take` сэмплов из текущей позиции `reader` и вернуть их
/// уровень одним окном.
fn read_window(
    reader: &mut hound::WavReader<std::io::BufReader<std::fs::File>>,
    take: usize,
) -> Result<f32, hound::Error> {
    let mut sum_sq = 0f64;
    let mut counted = 0usize;
    let channels = reader.spec().channels as usize;
    for (i, s) in reader.samples::<i16>().take(take * channels).enumerate() {
        let v = s? as f64;
        if i % channels != 0 {
            continue;
        }
        sum_sq += v * v;
        counted += 1;
    }
    Ok(window_dbfs(sum_sq, counted))
}

/// Уровни окон файла в dBFS по разреженной выборке.
///
/// Читается `windows` окон по [`WINDOW`] сэмплов, равномерно по файлу: для
/// 38-минутной записи это ~1.6 МБ вместо 73 МБ. Файл короче суммарной выборки
/// читается целиком подряд идущими окнами — выборка вырождается в полный
/// проход, а не в ошибку. Пустой файл даёт пустой список.
pub fn window_levels_dbfs(path: &Path, windows: usize) -> Result<Vec<f32>, hound::Error> {
    let mut reader = hound::WavReader::open(path)?;
    let total = reader.duration() as usize;
    if total == 0 {
        return Ok(Vec::new());
    }

    let mut levels = Vec::with_capacity(windows.max(1));
    if windows == 0 || total <= windows * WINDOW {
        let mut left = total;
        while left > 0 {
            let take = left.min(WINDOW);
            levels.push(read_window(&mut reader, take)?);
            left -= take;
        }
    } else {
        let stride = total / windows;
        for i in 0..windows {
            reader.seek((i * stride) as u32)?;
            levels.push(read_window(&mut reader, WINDOW)?);
        }
    }
    Ok(levels)
}

/// Перцентиль `p` списка уровней. Пустой список — [`FLOOR_DBFS`]: окон нет,
/// значит тишина.
pub fn percentile(levels: &[f32], p: f64) -> f32 {
    if levels.is_empty() {
        return FLOOR_DBFS;
    }
    let mut sorted = levels.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// «Так звучит речь на этой дорожке» — [`SPEECH_PERCENTILE`].
pub fn speech_level(levels: &[f32]) -> f32 {
    percentile(levels, SPEECH_PERCENTILE)
}

/// «Так звучит эта дорожка, когда никто не говорит» — [`NOISE_PERCENTILE`].
pub fn noise_level(levels: &[f32]) -> f32 {
    percentile(levels, NOISE_PERCENTILE)
}

/// Уровень речи файла в dBFS — не RMS всего файла.
///
/// RMS всего файла взвешен долей пауз: владелец молчит большую часть встречи,
/// и его дорожка выходит на 15–25 дБ «тише» ещё до всякой оценки. Здесь —
/// то, как звучит голос, когда он звучит: перцентиль уровней окон.
pub fn speech_level_dbfs(path: &Path, windows: usize) -> Result<f32, hound::Error> {
    Ok(speech_level(&window_levels_dbfs(path, windows)?))
}

/// Два числа, по которым решается пометка: речь и шумовая полка, dBFS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicLevels {
    pub speech: f32,
    pub noise: f32,
}

pub fn mic_levels(levels: &[f32]) -> MicLevels {
    MicLevels { speech: speech_level(levels), noise: noise_level(levels) }
}

/// Слишком тихий микрофон: `Some(недобор в дБ)` до ближайшего из двух порогов
/// — [`MIN_SPEECH_DBFS`] по уровню речи и [`MIN_CONTRAST_DB`] по её отрыву от
/// полки. `None` — голос слышен. Сравнение строгое: ровно порог не считается,
/// чтобы граничное значение не мигало пометкой между пересчётами.
///
/// Число нужно не интерфейсу (он показывает один и тот же текст), а логу и
/// тестам: по нему видно, какой из двух признаков сработал и насколько.
pub fn quiet_mic(m: MicLevels) -> Option<f32> {
    let недобор_уровня = MIN_SPEECH_DBFS - m.speech;
    let недобор_контраста = MIN_CONTRAST_DB - (m.speech - m.noise);
    let недобор = недобор_уровня.max(недобор_контраста);
    (недобор > 0.0).then_some(недобор)
}

/// Кеш посчитанных уровней.
///
/// Ключ включает размер и время модификации, поэтому заменённый или дописанный
/// файл пересчитывается сам. Без кеша список, обновляющийся на каждый фокус
/// окна, перечитывал бы диск заново.
#[derive(Default)]
pub struct Cache(Mutex<HashMap<(PathBuf, u64, SystemTime), MicLevels>>);

impl Cache {
    /// `None` — файла нет или он не читается как WAV. Это не ошибка: пометки
    /// просто не будет.
    pub fn levels(&self, path: &Path) -> Option<MicLevels> {
        let meta = std::fs::metadata(path).ok()?;
        let key = (path.to_path_buf(), meta.len(), meta.modified().ok()?);
        if let Ok(g) = self.0.lock() {
            if let Some(v) = g.get(&key) {
                return Some(*v);
            }
        }
        let v = mic_levels(&window_levels_dbfs(path, 200).ok()?);
        if let Ok(mut g) = self.0.lock() {
            g.insert(key, v);
        }
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_system_audio_does_not_mask_a_silent_microphone() {
        let path = std::env::temp_dir().join(format!("meetrec-quiet-stereo-{}.wav", std::process::id()));
        let spec = hound::WavSpec { channels: 2, sample_rate: 16_000, bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..16_000 {
            writer.write_sample(0i16).unwrap();
            writer.write_sample(20_000i16).unwrap();
        }
        writer.finalize().unwrap();
        let levels = window_levels_dbfs(&path, 2).unwrap();
        assert_eq!(levels, [FLOOR_DBFS, FLOOR_DBFS]);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rms_синуса_половинной_амплитуды_около_минус_девяти_дб() {
        let dir = std::env::temp_dir().join(format!("mr-imb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sine.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16_000 {
            let v = (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
            w.write_sample((v * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();

        // RMS синуса = амплитуда / sqrt(2) = 0.3536 → 20*log10(0.3536) ≈ -9.03 дБ
        let db = speech_level_dbfs(&path, 200).unwrap();
        assert!((db + 9.03).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn файл_короче_выборки_читается_целиком_а_не_падает() {
        let dir = std::env::temp_dir().join(format!("mr-imb-short-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("short.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..100 {
            w.write_sample(i16::MAX / 2).unwrap();
        }
        w.finalize().unwrap();

        let db = speech_level_dbfs(&path, 200).unwrap();
        assert!((db + 6.02).abs() < 0.5, "получили {db} дБ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn пустой_файл_даёт_пол_а_не_ошибку() {
        let dir = std::env::temp_dir().join(format!("mr-imb-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("empty.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        hound::WavWriter::create(&path, spec).unwrap().finalize().unwrap();

        assert_eq!(speech_level_dbfs(&path, 200).unwrap(), FLOOR_DBFS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Единственный тест, реально уходящий в ветку `seek`/`windows > 1`: оба
    /// теста выше укладываются в порог полного чтения
    /// (`total <= windows * WINDOW`) и никогда не зовут `reader.seek`.
    ///
    /// Файл здесь специально длиннее `windows * WINDOW` (200 * 4096 =
    /// 819 200 сэмплов), и, чтобы поймать не только «упало / не упало», а
    /// осмысленность результата, первые 10% файла — цифровая тишина, а
    /// остальные 90% — синус известной амплитуды. Страйд (`total / windows`)
    /// подобран так, что каждое из 200 окон целиком попадает либо в тишину,
    /// либо в синус, и пропорция окон (20 тишина / 180 синус) в точности
    /// повторяет пропорцию файла (10% / 90%) — поэтому у разреженной выборки
    /// и у полного чтения должен получиться один и тот же уровень речи: 90-й
    /// перцентиль окон, то есть чистый синус, тишина в него не попадает.
    /// Если бы `seek` не двигался (или считал не туда), выборка выродилась бы
    /// в 200 перечтений первого окна — чистую тишину — и результат провалился
    /// бы к `FLOOR_DBFS`, что и ловит вторая проверка ниже.
    #[test]
    fn разреженная_выборка_с_seek_совпадает_с_полным_чтением() {
        let dir = std::env::temp_dir().join(format!("mr-imb-sparse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sparse.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        const TOTAL: usize = 850_000;
        const SILENT: usize = 85_000; // ровно 10% — тишина в начале файла
        const WINDOWS: usize = 200;
        // 200 * 4096 = 819 200 — порог полного чтения в window_levels_dbfs.
        // TOTAL заведомо больше, значит функция обязана пойти в ветку seek.
        assert!(
            TOTAL > WINDOWS * WINDOW,
            "файл должен быть длиннее порога, иначе тест не проверяет seek"
        );

        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..TOTAL {
            let v = if i < SILENT {
                0.0
            } else {
                (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5
            };
            w.write_sample((v * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();

        // Ожидаемый уровень речи — синус амплитудой 0.5 (RMS = 0.5/√2), как
        // если бы тишины в файле не было вовсе: она занимает 10% окон, а
        // перцентиль 0.9 отсекает ровно их. Считаем формулой, а не константой.
        let expected_db = (20.0 * (0.5 / std::f64::consts::SQRT_2).log10()) as f32;

        let full = speech_level_dbfs(&path, 0).unwrap();
        assert!(
            (full - expected_db).abs() < 0.5,
            "полное чтение: получили {full} дБ, ожидали {expected_db} дБ"
        );

        let sparse = speech_level_dbfs(&path, WINDOWS).unwrap();
        assert!(
            (sparse - expected_db).abs() < 0.5,
            "разреженная выборка: получили {sparse} дБ, ожидали {expected_db} дБ"
        );
        // Если бы seek не сработал, выборка читала бы только первое окно —
        // сплошную тишину — и провалилась бы к полу, разойдясь с ожиданием на
        // добрых 90+ дБ. Проверяем этот контраст явно, а не только близость к
        // expected_db, чтобы намерение теста было видно и без чтения комментария.
        assert!(
            (sparse - FLOOR_DBFS).abs() > 30.0,
            "выборка подозрительно близка к полу — похоже, seek не сдвинулся: {sparse} дБ"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Файл заданной длины: речь (синус амплитуды `amp`) занимает долю `duty`
    /// каждого «предложения» из `period` сэмплов, а всё остальное — шумовая
    /// полка амплитуды `noise` (равномерный шум, простой LCG). Так выглядит
    /// живой микрофон: тишина в нём не цифровой ноль, а шипение.
    fn wav_с_речью_и_шумом(path: &Path, total: usize, period: usize, duty: f64, amp: f32, noise: f32) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 16_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let mut seed: u32 = 12345;
        for i in 0..total {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = ((seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * noise;
            let в_речи = (i % period) as f64 / period as f64 <= duty;
            let v = if в_речи {
                (i as f32 / 16_000.0 * 440.0 * std::f32::consts::TAU).sin() * amp + n
            } else {
                n
            };
            w.write_sample((v.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).unwrap();
        }
        w.finalize().unwrap();
    }

    fn временный(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mr-quiet-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("mic.wav")
    }

    const ПЯТЬ_МИНУТ: usize = 5 * 60 * 16_000;
    const ПРЕДЛОЖЕНИЕ: usize = 8 * 16_000;
    /// Синус амплитуды 0.010 — RMS ≈ −43 dBFS, как у речи Ани 08.09.
    const РЕЧЬ_АНИ: f32 = 0.010;
    /// Равномерный шум амплитуды 0.0017 — RMS ≈ −60 dBFS, полка её микрофона.
    const ШУМ_АНИ: f32 = 0.0017;

    /// Аня, 08.09: речь на −43 dBFS при полке −60, говорит 20% времени, слышно
    /// нормально. Системная дорожка при этом на 27 дБ громче — и это не
    /// повод для пометки: громкость лупбека зависит от колонок, а не от микрофона.
    #[test]
    fn нормальный_микрофон_при_громких_собеседниках_не_помечается() {
        let path = временный("anya");
        wav_с_речью_и_шумом(&path, ПЯТЬ_МИНУТ, ПРЕДЛОЖЕНИЕ, 0.2, РЕЧЬ_АНИ, ШУМ_АНИ);
        let m = mic_levels(&window_levels_dbfs(&path, 200).unwrap());
        assert!((m.speech + 43.0).abs() < 2.0, "речь: {}", m.speech);
        assert!((m.noise + 60.0).abs() < 2.0, "шум: {}", m.noise);
        assert_eq!(quiet_mic(m), None);
    }

    /// Владелец говорит почти всю встречу: медиана попала бы в речь и дала бы
    /// ложный «нет контраста». Полка берётся по 10-му перцентилю — в паузах.
    #[test]
    fn монолог_владельца_не_помечается() {
        let path = временный("monolog");
        wav_с_речью_и_шумом(&path, ПЯТЬ_МИНУТ, ПРЕДЛОЖЕНИЕ, 0.85, РЕЧЬ_АНИ, ШУМ_АНИ);
        let m = mic_levels(&window_levels_dbfs(&path, 200).unwrap());
        assert_eq!(quiet_mic(m), None, "речь {} шум {}", m.speech, m.noise);
    }

    /// Мёртвый микрофон (30.07): цифровые нули. Худший случай, обязан ловиться.
    #[test]
    fn мёртвый_микрофон_помечается() {
        let path = временный("dead");
        wav_с_речью_и_шумом(&path, ПЯТЬ_МИНУТ, ПРЕДЛОЖЕНИЕ, 0.2, 0.0, 0.0);
        let m = mic_levels(&window_levels_dbfs(&path, 200).unwrap());
        assert!(quiet_mic(m).is_some(), "речь {} шум {}", m.speech, m.noise);
    }

    /// Микрофон слышит комнату, но не человека: ровный шум без речи.
    #[test]
    fn шум_без_речи_помечается() {
        let path = временный("noise");
        wav_с_речью_и_шумом(&path, ПЯТЬ_МИНУТ, ПРЕДЛОЖЕНИЕ, 0.2, 0.0, 0.0097);
        let m = mic_levels(&window_levels_dbfs(&path, 200).unwrap());
        assert!((m.speech - m.noise) < MIN_CONTRAST_DB, "речь {} шум {}", m.speech, m.noise);
        assert!(quiet_mic(m).is_some());
    }

    /// Речь есть, но на −58 dBFS: чётко над полкой, и всё равно слишком тихо,
    /// чтобы на неё полагаться.
    #[test]
    fn слишком_тихая_речь_помечается() {
        let path = временный("faint");
        wav_с_речью_и_шумом(&path, ПЯТЬ_МИНУТ, ПРЕДЛОЖЕНИЕ, 0.2, 0.0025, 0.0003);
        let m = mic_levels(&window_levels_dbfs(&path, 200).unwrap());
        assert!(m.speech < MIN_SPEECH_DBFS, "речь {}", m.speech);
        assert!(quiet_mic(m).is_some());
    }

    #[test]
    fn пороги_пометки_по_уровню_и_по_контрасту() {
        let ok = MicLevels { speech: -49.0, noise: -70.0 };
        assert_eq!(quiet_mic(ok), None);
        let тихо = MicLevels { speech: -51.0, noise: -70.0 };
        assert!((quiet_mic(тихо).unwrap() - 1.0).abs() < 0.01, "недобор 1 дБ до −50");
        let без_контраста = MicLevels { speech: -30.0, noise: -27.0 };
        assert!((quiet_mic(без_контраста).unwrap() - 9.0).abs() < 0.01, "недобор 9 дБ до контраста 6");
        // Ровно порог не считается: сравнение строгое, чтобы граница не мигала.
        assert_eq!(quiet_mic(MicLevels { speech: -50.0, noise: -70.0 }), None);
        assert_eq!(quiet_mic(MicLevels { speech: -30.0, noise: -36.0 }), None);
    }

    /// Обе дорожки в тишине — раньше это «нечего сравнивать». Теперь системная
    /// дорожка не участвует вовсе, а молчащий микрофон — пометка.
    #[test]
    fn пустой_список_окон_это_молчащий_микрофон() {
        let m = mic_levels(&[]);
        assert_eq!(m, MicLevels { speech: FLOOR_DBFS, noise: FLOOR_DBFS });
        assert!(quiet_mic(m).is_some());
    }
}
