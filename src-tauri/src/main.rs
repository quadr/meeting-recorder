// В релизе консольного окна за GUI быть не должно. Раньше это означало, что
// eprintln! из аудио-потока в релизе улетал в никуда — единственным способом
// увидеть, что происходит, была отладочная сборка с консолью. С
// tauri-plugin-log (см. main()) лог теперь пишется в файл в любой сборке;
// консоль в отладке остаётся удобством, а не единственным источником истины.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod callabo;
mod callabo_secret;
mod config;
mod delete;
mod i18n;
use meeting_recorder::imbalance;
mod rename;
mod status;
mod tray;
mod update;
mod window;
use window::show_main_window;

use audio::Ctl;
use config::Config;
use imbalance::Cache;
use meeting_recorder::session::Event;
use serde::{Deserialize, Serialize};
use status::{Snapshot, Status};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Sender};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
use tauri_plugin_log::{Target, TargetKind};

/// Корень записей. Тот же, что у консольного бинаря (`src/main.rs`): разъедься
/// эти два пути, GUI перестал бы показывать записи, сделанные консолью, — а
/// консоль остаётся инструментом отладки того же ядра.
///
/// Конкретная запись ложится в месячную подпапку, см. `storage::month_dir`.
#[cfg(target_os = "windows")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("USERPROFILE").expect("%USERPROFILE% обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

#[cfg(target_os = "macos")]
fn recordings_root() -> PathBuf {
    let home = std::env::var("HOME").expect("$HOME обязан быть установлен");
    PathBuf::from(home).join("Recordings")
}

/// Канал в аудио-поток. `Mutex` — потому что `tauri::State` шарится между
/// потоками, а `Sender` не `Sync`.
struct Cmd(Mutex<Sender<Ctl>>);

impl Cmd {
    fn send(&self, c: Ctl) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .send(c)
            .map_err(|_| "аудио-поток не отвечает".to_string())
    }
}

/// Managed-обёртка над `audio::MonitorEpoch`: тот же счётчик, что видит
/// аудио-поток (см. его докблок), нужен и здесь — `set_monitor` обязан
/// ответить окну номером эпохи ДО того, как аудио-поток обработает команду
/// (это и есть сама причина гонки, которую эпоха лечит), поэтому команда не
/// ждёт ответа с провода, а читает общий счётчик напрямую.
struct MonitorEpochState(audio::MonitorEpoch);

/// Whether this recording is currently being captured.
fn recording_busy(status: &Status, folder: Option<&str>, base: &str) -> bool {
    status
        .snapshot()
        .current_recording
        .as_ref()
        .is_some_and(|recording| {
            Some(recording.folder.as_str()) == folder && recording.base == base
        })
}

/// Проверка обновлений: сразу после старта и дальше раз в сутки, пока
/// приложение живёт в трее. Результат кладётся в `UpdateState` — для окна,
/// которое поднимется позже, — и уходит событием `update-available` — для
/// окна, которое уже открыто. Любой отказ — в лог и молчание, см. докблок
/// `update.rs`: обновление не повод мешать записывать встречу.
fn spawn_update_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let client = match reqwest::Client::builder().build() {
            Ok(c) => c,
            Err(e) => {
                log::warn!("проверка обновлений: HTTP-клиент не создан: {e}");
                return;
            }
        };
        let current = app.package_info().version.to_string();
        loop {
            match update::fetch_latest(&client).await {
                Ok(latest) => {
                    let skipped = Config::load(&app).update_skipped_version;
                    let found = update::decide(&current, latest, skipped.as_deref());
                    app.state::<update::UpdateState>().set(found.clone());
                    if let Some(r) = found {
                        let _ = app.emit("update-available", &r);
                    }
                }
                Err(e) => log::warn!("проверка обновлений не удалась: {e}"),
            }
            tokio::time::sleep(update::CHECK_INTERVAL).await;
        }
    });
}

/// Что нашла последняя проверка — читается окном при загрузке, тем же
/// приёмом, что `get_state`: событие могло уйти до того, как webview поднялся.
#[tauri::command]
fn update_status(state: tauri::State<update::UpdateState>) -> Option<update::Release> {
    state.get()
}

/// «Позже»: запомнить пропущенную версию и погасить баннер. Гасится ровно
/// эта версия — следующий релиз баннер покажет снова (`update::decide`).
#[tauri::command]
fn update_skip(
    version: String,
    app: AppHandle,
    state: tauri::State<update::UpdateState>,
) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.update_skipped_version = Some(version);
    cfg.save(&app)?;
    state.set(None);
    Ok(())
}

/// A recording: one combined WAV or a legacy pair of tracks.
///
/// `Eq` из производных убран: появилось поле `f32`, на котором он не выводится.
/// `assert_eq!` в тестах работает и на одном `PartialEq`.
#[derive(Serialize, PartialEq, Debug)]
struct Recording {
    /// `2026-07-17_14-30_zoom` — общая основа обеих дорожек.
    name: String,
    /// Месячная папка или `None` для корня (записи до перехода на папки).
    folder: Option<String>,
    mic: bool,
    system: bool,
    /// Суммарный размер дорожек в байтах.
    size: u64,
    /// Длительность записи в секундах — по более полной из двух дорожек.
    ///
    /// Не по `size`: там сумма обеих дорожек, и оборвавшаяся дорожка сделала бы
    /// из 25 минут «40». См. `duration_sec`.
    duration_sec: u32,
    /// Микрофон владельца записал его слишком тихо: `Some(недобор в дБ)` по
    /// правилу `imbalance::quiet_mic`, считается по одной дорожке владельца.
    /// Заполняется в `list_recordings`, отдельно от группировки — та чистая и
    /// файлов не читает.
    quiet_mic_db: Option<f32>,
    mic_muted: bool,
    system_muted: bool,
    /// `true` — на эту запись прямо сейчас пишутся дорожки. Заполняется в
    /// `list_recordings` после группировки, сверкой со `Status::current_recording`
    /// — та же причина, что у `quiet_mic_db`: группировка чистая и файлов не
    /// читает, а это сверка не с диском, а с состоянием аудио-потока.
    recording_now: bool,
    /// Completed private Callabo upload, persisted in a non-secret sidecar.
    callabo_workspaces: Vec<callabo::UploadedWorkspace>,
    callabo_links: Vec<callabo::LinkedRecord>,
}

#[tauri::command]
fn send_event(name: &str, state: tauri::State<Cmd>, app: AppHandle) -> Result<(), String> {
    let ctl = match name {
        "confirm" => Ctl::Event(Event::UserConfirmed),
        "decline" => Ctl::Event(Event::UserDeclined),
        "start" => Ctl::Event(Event::ManualStart),
        "stop" => Ctl::Event(Event::ManualStop),
        // Toggle — не событие ядра: что делать, знает только машина (см. Ctl).
        "toggle" => Ctl::Toggle,
        other => return Err(format!("неизвестное событие: {other}")),
    };
    // Ошибку увидит и нажавший (она вернётся в webview), но одного этого мало:
    // кнопка в окне — не единственный вход, а причина у всех входов общая.
    // Поэтому провал send здесь — такой же фатальный случай, как в трее и на
    // хоткее, и объявляется он одинаково.
    state
        .send(ctl)
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

/// Текущее состояние целиком — для тех, кто опоздал на события.
///
/// Без этой команды webview узнаёт состояние только из `emit`, а `emit` уходит
/// лишь на изменении и только уже подписанным. Значит, страница, загрузившаяся
/// после старта аудио-потока (то есть всегда) или перезагруженная посреди
/// записи, осталась бы с захардкоженным «Ожидание встречи» из `index.html` —
/// вплоть до следующей смены состояния. См. `status` целиком.
#[tauri::command]
fn get_state(status: tauri::State<Status>) -> Snapshot {
    status.snapshot()
}

/// Сколько байт занимает секунда записи.
///
/// Формат дорожки задан в одном месте — `storage::WavSink::create`: моно,
/// `SAMPLE_RATE`, 16 бит на отсчёт. Считаем отсюда, а не константой 32000,
/// чтобы смена частоты дискретизации в ядре не оставила здесь молча врущую
/// арифметику.
const WAV_BYTES_PER_SEC: u64 = meeting_recorder::storage::SAMPLE_RATE as u64 * 2;

/// Длина заголовка WAV, который пишет `hound` для моно 16 бит.
///
/// `hound` выбирает `PCMWAVEFORMAT` для всего, что не больше двух каналов и не
/// глубже 16 бит (`WavWriter::new_with_spec_ex`), а у него заголовок ровно 44
/// байта: RIFF (12) + `fmt ` (24) + шапка `data` (8). Величина мелкая — 44
/// байта это 1,4 мс, — но вычитается честно, чтобы недописанный файл из одного
/// заголовка давал ноль, а не единицу.
const WAV_HEADER_BYTES: u64 = 44;

/// Длительность дорожки в секундах по её размеру на диске.
///
/// Читать заголовок каждого файла было бы точнее лишь на бумаге: длину данных
/// `hound` пишет туда же, откуда мы её и берём, — из размера файла, — а обход
/// каталога и так знает размер из `metadata()`, без единого открытия файла.
///
/// Обрезка вниз намеренная: «40 мин» у записи 40:59 честнее, чем «41».
/// `saturating_sub` закрывает недописанный или чужой файл короче заголовка.
fn duration_sec(track_bytes: u64) -> u32 {
    (track_bytes.saturating_sub(WAV_HEADER_BYTES) / WAV_BYTES_PER_SEC) as u32
}

/// Файл-спутник `<основа>.meta.json`, который пишет `close_sinks` в ядре
/// (`src/app.rs::SinkFactory::write_meta`) в момент финализации дорожек.
///
/// Mute flags are optional so older recordings keep their original behavior.
#[derive(Deserialize)]
struct RecordingMeta {
    #[serde(default)]
    duration_sec: u32,
    #[serde(default)]
    mic_muted: bool,
    #[serde(default)]
    system_muted: bool,
}

#[cfg(test)]
mod mute_metadata_tests {
    use super::RecordingMeta;

    #[test]
    fn old_metadata_and_optional_mute_flags_are_compatible() {
        let old: RecordingMeta = serde_json::from_str(r#"{"v":1,"duration_sec":42}"#).unwrap();
        assert_eq!(old.duration_sec, 42);
        assert!(!old.mic_muted && !old.system_muted);
        let muted: RecordingMeta = serde_json::from_str(
            r#"{"v":1,"duration_sec":42,"mic_muted":true,"system_muted":false}"#,
        )
        .unwrap();
        assert_eq!(muted.duration_sec, 42);
        assert!(muted.mic_muted && !muted.system_muted);
    }
}

/// Длительность из файла-спутника, если он лежит рядом и читается.
///
/// `None` — файла нет, он не открылся или его содержимое не разбирается как
/// JSON нужной формы: во всех трёх случаях вызывающий обязан молча откатиться
/// на расчёт по размеру `.wav` (`duration_sec` выше), а не уронить список
/// записей. Битый файл-спутник — это файл, у которого повезло меньше, чем
/// дорожкам, а не повод перестать показывать запись целиком.
fn read_duration_meta(path: &Path) -> Option<u32> {
    read_recording_meta(path).map(|m| m.duration_sec)
}

fn read_recording_meta(path: &Path) -> Option<RecordingMeta> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Group combined WAVs and legacy tracks by (base, folder), newest first.
/// Folder is part of the identity: root/month collisions must not merge.
/// Duration metadata overrides the size estimate only for an audio recording.
fn group_recordings(
    files: impl IntoIterator<Item = (Option<String>, String, u64)>,
    durations: &HashMap<(String, Option<String>), u32>,
) -> Vec<Recording> {
    let mut found: BTreeMap<(String, Option<String>), Recording> = BTreeMap::new();
    for (folder, file, size) in files {
        let combined = !file.ends_with(".mic.wav") && !file.ends_with(".system.wav");
        let (base, is_mic) = match (
            file.strip_suffix(".mic.wav"),
            file.strip_suffix(".system.wav"),
        ) {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => match file.strip_suffix(".wav") {
                Some(b) if meeting_recorder::storage::split_name(b).is_some() => {
                    (b.to_string(), true)
                }
                _ => continue,
            },
        };
        let key = (base.clone(), folder.clone());
        let rec = found.entry(key).or_insert(Recording {
            name: base,
            folder,
            mic: false,
            system: false,
            size: 0,
            duration_sec: 0,
            quiet_mic_db: None,
            mic_muted: false,
            system_muted: false,
            recording_now: false,
            callabo_workspaces: vec![],
            callabo_links: vec![],
        });
        if is_mic {
            rec.mic = true;
        } else {
            rec.system = true;
        }
        if combined {
            rec.system = true;
        }
        rec.size += size;
        // Именно max, а не сумма: дорожки пишутся параллельно, и запись длится
        // столько, сколько длится более полная из них.
        rec.duration_sec = rec
            .duration_sec
            .max(duration_sec(size) / if combined { 2 } else { 1 });
    }
    for (key, duration) in durations {
        if let Some(rec) = found.get_mut(key) {
            rec.duration_sec = *duration;
        }
    }
    found.into_values().rev().collect()
}

/// Похоже ли имя папки на месячную (`2026-07`).
fn is_month_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 7
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..].iter().all(u8::is_ascii_digit)
}

/// Audio files and duration metadata collected in one directory scan.
#[derive(Default, PartialEq, Debug)]
struct Found {
    /// `(папка, имя файла, размер)` — всё, что лежит файлами.
    files: Vec<(Option<String>, String, u64)>,
    /// `(основа, папка)` → длительность из `<основа>.meta.json`, если рядом
    /// нашёлся файл-спутник и он разобрался (см. `read_duration_meta`).
    /// Битый или отсутствующий файл просто не попадает сюда — ключа нет,
    /// `group_recordings` откатывается на расчёт по размеру `.wav`.
    durations: HashMap<(String, Option<String>), u32>,
}

/// Файлы корня плюс файлы месячных подпапок. Глубина ровно два уровня:
/// предсказуемо и не засасывает чужое дерево, если рядом окажется постороннее.
///
fn collect_files(root: &Path) -> Result<Found, String> {
    fn read(dir: &Path, folder: Option<&str>, out: &mut Found) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            match e.file_type() {
                Ok(t) if t.is_file() => {
                    if let Some(base) = name.strip_suffix(".meta.json") {
                        if let Some(dur) = read_duration_meta(&e.path()) {
                            out.durations
                                .insert((base.to_string(), folder.map(str::to_string)), dur);
                        }
                    }
                    out.files.push((
                        folder.map(str::to_string),
                        name,
                        e.metadata().map(|m| m.len()).unwrap_or(0),
                    ));
                }
                _ => {}
            }
        }
    }

    let mut out = Found::default();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        // Каталога нет — записей просто ещё не было. Это не ошибка.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::default()),
        Err(e) => return Err(format!("не удалось прочитать {}: {e}", root.display())),
    };
    let mut months: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        match e.file_type() {
            Ok(t) if t.is_dir() && is_month_folder(&name) => months.push(e.path()),
            Ok(t) if t.is_file() => {
                if let Some(base) = name.strip_suffix(".meta.json") {
                    if let Some(dur) = read_duration_meta(&e.path()) {
                        out.durations.insert((base.to_string(), None), dur);
                    }
                }
                out.files
                    .push((None, name, e.metadata().map(|m| m.len()).unwrap_or(0)));
            }
            _ => {}
        }
    }
    for m in months {
        let folder = m.file_name().map(|n| n.to_string_lossy().into_owned());
        read(&m, folder.as_deref(), &mut out);
    }
    Ok(out)
}

/// Список записей для окна: обход каталога, склейка дорожек в пары и разметка
/// тихого микрофона.
///
/// Три шага одной команды, а не три отдельных, и это не одно и то же с точки
/// зрения того, где что тестируется. Обход (`collect_files`) и склейка
/// (`group_recordings`) разделены сознательно — см. их докблоки: это
/// единственная логика здесь, которую можно сломать незаметно, и она чистая,
/// без файлового ввода-вывода, значит проверяется без диска. Разметка
/// тихого микрофона, наоборот, СОБРАНА прямо тут, а не вынесена рядом: она читает
/// содержимое файлов через `Cache::levels`, и утаскивать чтение файлов в чистую
/// функцию значило бы отнять у неё главное свойство — тестируемость без диска.
///
/// Пометка считается только для полных пар (`r.mic && r.system`): одинокая
/// дорожка уже помечена как неполная в UI, второе предупреждение поверх неё
/// ничего не добавит, а чтение файла стоит времени зря.
///
/// `recording_now` заполняется той же сверкой, что и тихий микрофон, но без
/// условия на полную пару: запись без системного звука тоже может идти.
#[tauri::command]
fn list_recordings(
    cache: tauri::State<Cache>,
    status: tauri::State<Status>,
) -> Result<Vec<Recording>, String> {
    let root = recordings_root();
    let found = collect_files(&root)?;
    let mut list = group_recordings(found.files, &found.durations);
    let current = status.snapshot().current_recording;
    for r in &mut list {
        let dir = match &r.folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        let combined_path = dir.join(format!("{}.wav", r.name));
        r.callabo_workspaces = callabo::completed_workspaces(&dir, &r.name);
        r.callabo_links = callabo::completed_links(&dir, &r.name);
        if let Some(meta) = read_recording_meta(&dir.join(format!("{}.meta.json", r.name))) {
            r.mic_muted = meta.mic_muted;
            r.system_muted = meta.system_muted;
        }
        if let Ok(reader) = hound::WavReader::open(&combined_path) {
            r.mic = true;
            r.system = reader.spec().channels >= 2;
            if !found
                .durations
                .contains_key(&(r.name.clone(), r.folder.clone()))
            {
                r.duration_sec = reader.duration() / reader.spec().sample_rate;
            }
        }
        r.recording_now = current
            .as_ref()
            .is_some_and(|c| Some(c.folder.as_str()) == r.folder.as_deref() && c.base == r.name);
        // Пометка имеет смысл только для полной пары: одинокая дорожка уже
        // помечена как неполная, и второе предупреждение о ней ничего не добавит.
        if !(r.mic && r.system) || r.mic_muted || r.recording_now {
            continue;
        }
        // Только дорожка владельца: системная в решении не участвует — её
        // громкость зависит от колонок собеседника, а не от микрофона
        // (см. докблок `imbalance`).
        let mic_path = if combined_path.exists() {
            combined_path
        } else {
            dir.join(format!("{}.mic.wav", r.name))
        };
        if let Some(m) = cache.levels(&mic_path) {
            r.quiet_mic_db = imbalance::quiet_mic(m);
        }
    }
    Ok(list)
}

/// Показать каталог в проводнике/Finder.
///
/// Через `explorer.exe` напрямую, без `tauri-plugin-opener`: плагин ради одной
/// строчки тянул бы за собой ещё и права в capabilities.
///
/// Код возврата не проверяется намеренно: `explorer.exe` возвращает 1 даже когда
/// окно успешно открылось. Проверять здесь нечего — либо папка открылась, либо
/// пользователь это увидит сам.
///
/// Общая для обеих команд открытия, чтобы способ открытия и это объяснение
/// жили в одном месте: разъехавшись, они разъедутся молча.
fn reveal(dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(dir);
    cmd.spawn()
        .map_err(|e| format!("не удалось открыть Finder/проводник: {e}"))?;
    Ok(())
}

/// Открыть каталог записей целиком.
#[tauri::command]
fn open_folder() -> Result<(), String> {
    let dir = recordings_root();
    // Иначе explorer откроет «Документы» вместо пустого несуществующего пути.
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
    reveal(&dir)
}

/// Имя, пришедшее из окна, должно быть ровно одним шагом пути.
///
/// Разбором на компоненты, а не поиском `..` подстрокой: подстрока пропустила
/// бы `x/../../y`, а разбор — нет. Ровно один `Component::Normal` означает
/// сразу всё нужное: имя не абсолютное, не корень, не диск (`C:` на Windows),
/// не `.` и не `..`, и разделителей внутри нет.
///
/// Сравнение с исходной строкой нужно потому, что `Path` нормализует на лету:
/// у `"файл/"` компонент один и он `Normal`, но само имя уже с хвостом, и
/// пропускать такое незачем.
fn one_segment(name: &str) -> Result<(), String> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(std::path::Component::Normal(n)), None) if n == std::ffi::OsStr::new(name) => Ok(()),
        _ => Err(format!("«{name}» — не имя внутри каталога записей")),
    }
}

/// Build and validate a recording directory from frontend identifiers.
///
/// Путь собирается ЗДЕСЬ, из корня записей, и ни один его кусок не приходит
/// готовым: из окна прилетают только имя месячной папки и основа имени записи,
/// а основу человек мог поменять сам — и через «Переименовать», и руками в
/// Finder, где на неё нет вообще никаких правил.
///
/// Папка проверяется не «на плохие символы», а на то, что она месячная
/// (`2026-07`): другого места для записей нет — `collect_files` заходит ровно в
/// такие подпапки и больше никуда, — а семь цифр с дефисом не могут вывести за
/// пределы корня в принципе. Это строже любого чёрного списка и короче.
///
/// Отделено от команды, чтобы проверяться без диска: сборка пути — это ровно
/// то, что здесь можно сломать незаметно, и файлы ей не нужны.
fn recording_dir(root: &Path, folder: Option<&str>, base: &str) -> Result<PathBuf, String> {
    // Основа проверяется всегда, а не только когда из неё строят подпапку:
    // правило «всё, что пришло из окна, проверено» держится в голове, а
    // «проверено в одной ветке из двух» — нет.
    one_segment(base)?;
    let mut dir = root.to_path_buf();
    if let Some(f) = folder {
        if !is_month_folder(f) {
            return Err(format!("«{f}» — не месячная папка записей"));
        }
        dir.push(f);
    }
    Ok(dir)
}

/// Reveal an existing recording directory without creating missing folders.
#[tauri::command]
fn open_recording_folder(folder: Option<String>, base: String) -> Result<(), String> {
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base)?;
    if !dir.is_dir() {
        return Err(format!("папки {} нет", dir.display()));
    }
    reveal(&dir)
}

/// Открыть раздел настроек, где выдают разрешение на захват системного звука.
///
/// Тем же способом, что `open_folder`, и по той же причине: одна строка вместо
/// плагина с правами в capabilities.
///
/// Раздел — «Запись экрана и звука» (`Privacy_ScreenCapture`): Process Tap
/// живёт именно там, хотя usage description у него свой
/// (`NSAudioCaptureUsageDescription`). Отдельного якоря под захват звука в
/// схеме `x-apple.systempreferences` нет.
///
/// Кнопка нужна не для красоты: путь до этого переключателя человек по памяти
/// не наберёт, а предупреждение, которое говорит «разрешите в настройках» и не
/// показывает где, перекладывает поиск на того, кто и так уже споткнулся.
#[cfg(target_os = "macos")]
#[tauri::command]
fn open_privacy_settings() -> Result<(), String> {
    std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn()
        .map_err(|e| format!("не удалось открыть Системные настройки: {e}"))?;
    Ok(())
}

/// На Windows этой кнопки нет — как нет и разрешения, которое она открывает:
/// WASAPI loopback его не требует. Команда существует только затем, чтобы
/// `invoke` из общего `main.js` не падал в ненайденную команду.
#[cfg(not(target_os = "macos"))]
#[tauri::command]
fn open_privacy_settings() -> Result<(), String> {
    Ok(())
}

/// Открыть страницу репозитория в браузере по умолчанию.
///
/// Тем же способом, что `open_folder`: `explorer.exe`/`open` открывают не
/// только пути, но и произвольный URL — заводить `tauri-plugin-opener` ради
/// одной ссылки в подвале окна незачем. Ссылка нужна не для красоты: человек,
/// которому переслали голый `.exe`/`.dmg` без сопроводительного текста, иначе
/// не узнает, откуда взять новую версию или куда написать про баг.
const REPOSITORY_URL: &str = "https://github.com/quadr/meeting-recorder";

#[tauri::command]
fn open_repository() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(REPOSITORY_URL);
    cmd.spawn()
        .map_err(|e| format!("не удалось открыть браузер: {e}"))?;
    Ok(())
}

/// Открыть произвольную внешнюю ссылку — тем же способом, что `open_repository`,
/// но параметризованно: экран «о разработчиках» ведёт на несколько разных
/// адресов (почта, LinkedIn, личный сайт, GitHub), заводить отдельную команду
/// на каждый незачем.
///
/// Схема ограничена явным списком (`http`/`https`/`mailto`) — это ровно то,
/// что нужно ссылкам на этих экранах; разрешить фронтенду открыть что угодно
/// значило бы доверять ему больше, чем он того заслуживает.
#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    let разрешена =
        url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:");
    if !разрешена {
        return Err(format!("недопустимая ссылка: {url}"));
    }
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(&url);
    cmd.spawn()
        .map_err(|e| format!("не удалось открыть браузер: {e}"))?;
    Ok(())
}

/// Доступные микрофоны для выпадашки: идентификатор и что показать.
///
/// `InputDevice` уже `Serialize`? Нет — он в ядре, где serde не подключён.
/// Поэтому здесь своя DTO: тащить serde в ядро ради одной структуры значило бы
/// расширить его зависимости под нужду GUI.
#[derive(serde::Serialize)]
struct MicDevice {
    id: String,
    name: String,
}

#[tauri::command]
fn list_mic_devices() -> Result<Vec<MicDevice>, String> {
    Ok(meeting_recorder::capture::list_input_devices()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|d| MicDevice {
            id: d.id,
            name: d.name,
        })
        .collect())
}

#[tauri::command]
fn get_config(app: AppHandle) -> Config {
    Config::load(&app)
}

/// Переименовать запись. `folder` — месячная папка или `None` для корня.
#[tauri::command]
fn rename_recording(
    folder: Option<String>,
    base: String,
    new_tail: String,
    uploads: tauri::State<callabo::Uploads>,
) -> Result<String, String> {
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base)?;
    uploads.while_idle(folder.as_deref(), &base, || {
        rename::rename_recording(&dir, &base, &new_tail)
    })
}

/// Move idle recording audio and metadata to the Recycle Bin.
/// Legacy transcript directories are not part of the recording anymore.
#[tauri::command]
fn delete_recording(
    folder: Option<String>,
    base: String,
    status: tauri::State<Status>,
    uploads: tauri::State<callabo::Uploads>,
) -> Result<(), String> {
    if recording_busy(&status, folder.as_deref(), &base) {
        return Err(format!("«{base}» сейчас занята — идёт запись"));
    }
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base)?;
    uploads.while_idle(folder.as_deref(), &base, || {
        delete::delete_recording(&dir, &base)
    })
}

/// `id: None` — вернуться на системный дефолт.
///
/// Имя приходит вместе с идентификатором и сохраняется рядом: когда устройства
/// не окажется в системе, показать пользователю будет нечего, кроме него.
#[tauri::command]
fn set_mic_device(
    id: Option<String>,
    name: Option<String>,
    state: tauri::State<Cmd>,
    app: AppHandle,
) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.mic_device_id = id;
    cfg.mic_device_name = name;
    cfg.save(&app)?;
    state
        .send(Ctl::SetMicDevice(cfg.choice()))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))
}

/// `language: "system" | "ru" | "en"`. Сохраняет выбор, переключает
/// действующий язык на процесс и перерисовывает трей — единственную часть
/// GUI, которую webview не умеет перекрасить сам.
///
/// Остального интерфейса эта команда не касается: перерисовать окно —
/// работа фронтенда (см. `ui/main.js`), у него для этого есть свежий словарь
/// и `applyStatic()`.
#[tauri::command]
fn set_language(language: Option<String>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.language = language;
    cfg.save(&app)?;
    let lang = i18n::effective_lang(cfg.language.as_deref());
    app.state::<i18n::ActiveLang>().set(lang);
    tray::refresh_texts(&app);
    Ok(())
}

/// `theme: "system" | "light" | "dark"`. Только сохраняет выбор — в отличие
/// от `set_language`, тему не нужно ни разбирать в действующее значение (это
/// решает CSS через `data-theme`/`prefers-color-scheme`), ни трогать трей:
/// это выбор темы приложения, а иконка трея шаблонная и красится системой
/// сама (см. `tray.rs`) — они независимы намеренно.
#[tauri::command]
fn set_theme(theme: Option<String>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.theme = theme;
    cfg.save(&app)
}

/// Включить/выключить проверку микрофона.
///
/// Отвечает СРАЗУ, не дожидаясь, пока аудио-поток на следующем тике разберёт
/// канал (см. докблок `audio::MonitorEpoch`) — иначе кнопка на секунды
/// зависала бы disabled. Возвращает номер эпохи, который получится ПОСЛЕ
/// применения этой команды: `epoch` читается уже после отправки в канал.
/// Возвращённое значение — нижняя граница будущего результата.
/// `ui/main.js` запоминает это число и игнорирует поле `monitoring` у
/// событий `levels` со эпохой меньше него.
#[tauri::command]
fn set_monitor(
    on: bool,
    state: tauri::State<Cmd>,
    epoch: tauri::State<MonitorEpochState>,
    app: AppHandle,
) -> Result<u64, String> {
    state
        .send(Ctl::Monitor(on))
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))?;
    Ok(epoch.0.load(Ordering::SeqCst) + 1)
}

#[tauri::command]
async fn set_mute(
    source: String,
    muted: bool,
    state: tauri::State<'_, Cmd>,
    app: AppHandle,
) -> Result<status::MuteSnapshot, String> {
    let mic = match source.as_str() {
        "mic" => true,
        "system" => false,
        _ => return Err("Unknown audio source".into()),
    };
    let (reply, receive) = tokio::sync::oneshot::channel();
    state
        .send(Ctl::SetMute { mic, muted, reply })
        .inspect_err(|_| status::fatal(&app, status::DEAD.to_string()))?;
    receive
        .await
        .map_err(|_| "Audio thread stopped before applying mute".to_string())?
}

fn main() {
    let (tx, rx) = channel::<Ctl>();
    let tray_tx = tx.clone();
    // Один счётчик на весь процесс: команда и аудио-поток обязаны видеть одно
    // и то же число, иначе эпоха ничего не различает. См. докблок
    // `audio::MonitorEpoch`.
    let monitor_epoch: audio::MonitorEpoch =
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let monitor_epoch_audio = monitor_epoch.clone();
    let hotkey_tx = tx.clone();

    tauri::Builder::default()
        // Must precede every other plugin: the second process exits before
        // registering hotkeys, creating a tray icon or starting audio workers.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main_window(app);
        }))
        // После single-instance — до .manage(Status::default()), у которого свой докблок
        // «аудио-поток пишет сюда с первой же строки»: если сбой случится
        // раньше, чем плагин поднимется, он снова уйдёт в никуда, ровно как
        // раньше уходил eprintln! из GUI без консоли.
        .plugin(
            tauri_plugin_log::Builder::new()
                .targets([
                    Target::new(TargetKind::LogDir { file_name: None }),
                    Target::new(TargetKind::Stdout),
                ])
                .level(log::LevelFilter::Info)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .manage(Cmd(Mutex::new(tx)))
        .manage(MonitorEpochState(monitor_epoch))
        // Заводится до setup(): аудио-поток пишет сюда с первой же строки, а
        // фатальная ошибка там случается раньше, чем webview успеет подписаться.
        .manage(Status::default())
        .manage(Cache::default())
        .manage(update::UpdateState::default())
        .manage(callabo::Uploads::default())
        .invoke_handler(tauri::generate_handler![
            send_event,
            get_state,
            list_recordings,
            open_folder,
            open_privacy_settings,
            open_repository,
            open_url,
            list_mic_devices,
            get_config,
            callabo::callabo_workspaces,
            callabo::callabo_auth_status,
            callabo::callabo_forget_token,
            callabo::callabo_dialog_data,
            callabo::set_callabo_workspace,
            callabo::callabo_upload,
            callabo::linked::callabo_sync_record,
            set_mic_device,
            set_language,
            set_theme,
            set_monitor,
            set_mute,
            rename_recording,
            delete_recording,
            open_recording_folder,
            update_status,
            update_skip
        ])
        .setup(move |app| {
            // Приложение живёт в Dock как обычное (`ActivationPolicy::Regular`),
            // а не только в строке меню. Раньше здесь стояла `Accessory`: окно
            // стартовало скрытым, крестик его прятал, и иконка в Dock без
            // видимого окна и без обработчика повторного открытия только вводила
            // бы в заблуждение. Обе причины отпали: окно стартует видимым
            // (`"visible": true` в `tauri.conf.json`), а клик по иконке теперь
            // обрабатывает `RunEvent::Reopen` в `main()` — показывает и
            // фокусирует главное окно, если видимых окон нет.
            //
            // Парная половина этого решения раньше была в `LSUIElement`
            // (`src-tauri/Info.plist`) — ключ снят вместе со сменой политики,
            // Dock теперь должен быть виден с самого запуска.
            //
            // На показ окна из `status::fatal` это не влияет: `set_focus()` в
            // tao — это `makeKeyAndOrderFront` + `activateIgnoringOtherApps`, то
            // есть явная активация, которая работает одинаково у обеих политик.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Regular);

            let handle = app.handle().clone();

            // Действующий язык нужен ДО tray::build(): трей рисует свои
            // тексты один раз при сборке меню, и решать на каком языке позже
            // уже нечем — второй сборки меню не будет, только точечные правки
            // текста (см. set_language).
            let lang = i18n::effective_lang(Config::load(&handle).language.as_deref());
            app.manage(i18n::ActiveLang::new(lang));

            tray::build(&handle, tray_tx)?;

            // Ctrl+Shift+R — toggle. Что именно делать, решает аудио-поток по
            // состоянию машины: хоткей обязан работать и с закрытым окном, а
            // спрашивать состояние у webview, которого может не быть на экране,
            // — способ однажды не остановить запись.
            let shortcut = Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            app.global_shortcut()
                .on_shortcut(shortcut, move |app, _sc, event| {
                    // Только на нажатие: без этого один хоткей даёт две команды
                    // (нажатие + отпускание), то есть старт и мгновенный стоп.
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    // Хоткей — самый молчаливый из входов: нажатие вслепую, без
                    // окна и без меню. Проглотить здесь ошибку значит оставить
                    // пользователя уверенным, что запись идёт.
                    if hotkey_tx.send(Ctl::Toggle).is_err() {
                        status::fatal(app, status::DEAD.to_string());
                    }
                })?;

            // Аудио-поток. Всё !Send рождается ВНУТРИ него.
            let mic = Config::load(&handle).choice();
            std::thread::spawn(move || {
                audio::run(handle, rx, recordings_root(), mic, monitor_epoch_audio)
            });

            spawn_update_worker(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Крестик прячет окно, а не выходит: это трей-приложение, детект
            // обязан продолжать работать. Выход — только через меню трея, где он
            // проходит через финализацию записи.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("не удалось запустить приложение")
        .run(|app_handle, event| {
            // Клик по иконке в Dock у приложения без видимых окон присылает
            // `Reopen` — без этого обработчика повторится ровно та проблема,
            // из-за которой Dock когда-то убирали (см. докблок в `setup()`):
            // иконка есть, а клик по ней ничего не делает.
            //
            // Вариант существует только на macOS (`tauri` 2.11.5, `app.rs`,
            // `enum RunEvent::Reopen`), поэтому и обработка — только под
            // `cfg(target_os = "macos")`, а не веткой `match`.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen {
                has_visible_windows,
                ..
            } = event
            {
                if !has_visible_windows {
                    show_main_window(app_handle);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Каталог записей обязан выводиться из домашнего каталога ТЕКУЩЕГО
    /// пользователя, а не быть прибитым к чьему-то конкретному профилю.
    /// Раньше под Windows здесь стоял литерал `C:\Users\<username>\Recordings`,
    /// и на чужой машине приложение писало в чужой домашний каталог.
    #[cfg(target_os = "windows")]
    #[test]
    fn каталог_записей_выводится_из_профиля_пользователя() {
        let profile = std::env::var("USERPROFILE").expect("%USERPROFILE%");
        assert_eq!(recordings_root(), PathBuf::from(profile).join("Recordings"));
    }

    /// Симметричный сторож для macOS: ветки двух систем должны оставаться
    /// одинаковыми по смыслу, и если кто-то починит одну, вторая не должна
    /// тихо разъехаться.
    #[cfg(target_os = "macos")]
    #[test]
    fn каталог_записей_выводится_из_домашнего_каталога() {
        let home = std::env::var("HOME").expect("$HOME");
        assert_eq!(recordings_root(), PathBuf::from(home).join("Recordings"));
    }

    /// исчисляются десятками байт, то есть меньше секунды звука.
    fn rec(name: &str, folder: Option<&str>, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            mic,
            system,
            size,
            duration_sec: 0,
            quiet_mic_db: None,
            mic_muted: false,
            system_muted: false,
            recording_now: false,
            callabo_workspaces: vec![],
            callabo_links: vec![],
        }
    }

    fn group(files: &[(Option<&str>, &str, u64)]) -> Vec<Recording> {
        group_full(files, &[])
    }

    fn group_full(
        files: &[(Option<&str>, &str, u64)],
        durations: &[((&str, Option<&str>), u32)],
    ) -> Vec<Recording> {
        let durations = durations
            .iter()
            .map(|((base, folder), seconds)| {
                ((base.to_string(), folder.map(str::to_string)), *seconds)
            })
            .collect();
        group_recordings(
            files
                .iter()
                .map(|(folder, name, size)| (folder.map(str::to_string), name.to_string(), *size)),
            &durations,
        )
    }

    fn wav_bytes(sec: u64) -> u64 {
        WAV_HEADER_BYTES + sec * WAV_BYTES_PER_SEC
    }

    #[test]
    fn пара_дорожек_склеивается_в_одну_запись() {
        assert_eq!(
            group(&[
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
                (None, "2026-07-17_14-45_zoom.system.wav", 20),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, true, 120)],
            "размер записи — сумма дорожек, имя — общая основа"
        );
    }

    #[test]
    fn two_channel_wav_is_one_complete_recording() {
        let size = WAV_HEADER_BYTES + 600 * WAV_BYTES_PER_SEC * 2;
        let list = group(&[(Some("2026-07"), "2026-07-17_14-45_zoom.wav", size)]);
        assert_eq!(list.len(), 1);
        assert!(list[0].mic && list[0].system);
        assert_eq!(list[0].name, "2026-07-17_14-45_zoom");
        assert_eq!(list[0].size, size);
        assert_eq!(list[0].duration_sec, 600);
    }

    /// Отсутствие дорожки — не косметика: пара mic+system и есть запись, и UI
    /// показывает неполную пару предупреждением.
    #[test]
    fn одинокая_дорожка_видна_как_неполная() {
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.mic.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
        assert_eq!(
            group(&[(None, "2026-07-17_14-45_zoom.system.wav", 100)]),
            vec![rec("2026-07-17_14-45_zoom", None, false, true, 100)]
        );
    }

    #[test]
    fn чужие_файлы_в_каталоге_не_наше_дело() {
        assert_eq!(
            group(&[
                (None, "заметки.txt", 10),
                (None, "foreign_zoom.wav", 10),
                (None, "mic.wav", 10),
                (None, ".mic.wav.bak", 10),
                (None, "2026-07-17_14-45_zoom.mic.wav", 100),
            ]),
            vec![rec("2026-07-17_14-45_zoom", None, true, false, 100)]
        );
    }

    /// Основа начинается с `YYYY-MM-DD_HH-MM`, поэтому лексикографический
    /// порядок BTreeMap и есть хронологический, а `.rev()` даёт «свежее сверху».
    #[test]
    fn свежее_сверху_независимо_от_порядка_обхода() {
        let list = group(&[
            (None, "2026-07-17_09-00_meet.mic.wav", 1),
            (None, "2026-07-18_10-00_zoom.mic.wav", 1),
            (None, "2026-07-16_23-59_teams.mic.wav", 1),
        ]);
        let names: Vec<&str> = list.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "2026-07-18_10-00_zoom",
                "2026-07-17_09-00_meet",
                "2026-07-16_23-59_teams"
            ]
        );
    }

    /// `_N` у повтора в ту же минуту — часть основы (см. `app::with_seq`),
    /// значит это отдельная запись, а не вторая дорожка первой.
    #[test]
    fn повтор_в_ту_же_минуту_это_отдельная_запись() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.mic.wav", 1),
            (None, "2026-07-17_14-45_zoom.system.wav", 1),
            (None, "2026-07-17_14-45_zoom_2.mic.wav", 5),
            (None, "2026-07-17_14-45_zoom_2.system.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-17_14-45_zoom_2", None, true, true, 10),
                rec("2026-07-17_14-45_zoom", None, true, true, 2),
            ]
        );
    }

    #[test]
    fn пустой_каталог_это_пустой_список_а_не_ошибка() {
        assert_eq!(group(&[]), vec![]);
    }

    #[test]
    fn записи_из_подпапки_и_из_корня_живут_в_одном_списке() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (Some("2026-07"), "2026-07-30_13-03_chrome.system.wav", 10),
            (None, "2026-06-01_10-00_zoom.mic.wav", 5),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, true, 20),
                rec("2026-06-01_10-00_zoom", None, true, false, 5),
            ],
            "порядок хронологический независимо от папки"
        );
    }

    #[test]
    fn папка_записи_запоминается() {
        let list = group(&[(Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 1)]);
        assert_eq!(list[0].folder.as_deref(), Some("2026-07"));
    }

    /// БЛОКЕР ревью: одна и та же основа в двух разных папках — сценарий
    /// прерванной миграции (`.mic.wav` не успел переехать, `.system.wav` уже в
    /// `2026-07/`) или коллизии имён между корнем и месячной папкой
    /// (`free_name_pair` не видит файлы корня). Раньше ключом группировки была
    /// только основа, и обе половинки схлопывались в одну «полную» запись, чья
    /// `folder` бралась от первого встреченного файла, — «Переименовать»
    /// переименовало бы только эту половину, вторая дорожка молча осталась бы
    /// под старым именем. Ключ `(base, folder)` обязан показать это честно:
    /// ДВЕ неполные записи, каждая под своей папкой, а не одна целая.
    #[test]
    fn одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую() {
        let list = group(&[
            (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
            (None, "2026-07-30_13-03_chrome.system.wav", 20),
        ]);
        assert_eq!(
            list,
            vec![
                rec("2026-07-30_13-03_chrome", Some("2026-07"), true, false, 10),
                rec("2026-07-30_13-03_chrome", None, false, true, 20),
            ],
            "половинки одной основы из разных папок не имеют права слиться в одну запись"
        );
    }

    #[test]
    fn месячной_папкой_считается_только_yyyy_mm() {
        assert!(is_month_folder("2026-07"));
        assert!(!is_month_folder("2026-7"));
        assert!(!is_month_folder("2026-07-30"));
        assert!(!is_month_folder("архив"));
        assert!(!is_month_folder(""));
    }

    // ---- длительность --------------------------------------------------------

    /// Караул при правке формата записи: `duration_sec` считается по размеру
    /// файла, и смена частоты дискретизации или разрядности тихо сделает из неё
    /// вранье в разы.
    #[test]
    fn секунда_дорожки_весит_32000_байт() {
        assert_eq!(
            WAV_BYTES_PER_SEC, 32_000,
            "\n\
             Формат дорожки изменился: моно 16 бит при 16000 Гц — это 32000 байт\n\
             в секунду (src/storage.rs, WavSink::create).\n\
             \n\
             Длительность записи считается по размеру файла, а не по заголовку,\n\
             поэтому новый формат обязан приехать сюда вместе с правкой ядра —\n\
             иначе окно покажет «40 мин» там, где записано 20.\n"
        );
    }

    /// Гвоздь задачи: `size` — сумма дорожек, и делить её пополам нельзя.
    /// Ровно тот случай, который окно и так помечает предупреждением: system
    /// оборвалась на 25-й минуте, mic писался все 40. Оценка по сумме дала бы
    /// 32 минуты — не длительность ни одной из дорожек.
    #[test]
    fn длительность_считается_по_более_полной_дорожке_а_не_по_сумме() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400)),
            (None, "2026-07-17_14-45_zoom.system.wav", wav_bytes(1500)),
        ]);
        assert_eq!(list[0].duration_sec, 2400, "40 минут, а не 32 и не 65");
        assert_eq!(
            list[0].size,
            wav_bytes(2400) + wav_bytes(1500),
            "size остаётся суммой — на нём держится показ занятого места"
        );
    }

    #[test]
    fn порядок_обхода_дорожек_на_длительность_не_влияет() {
        let list = group(&[
            (None, "2026-07-17_14-45_zoom.system.wav", wav_bytes(1500)),
            (None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400)),
        ]);
        assert_eq!(list[0].duration_sec, 2400);
    }

    #[test]
    fn у_одинокой_дорожки_длительность_её_собственная() {
        let list = group(&[(None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(600))]);
        assert_eq!(list[0].duration_sec, 600);
    }

    /// Показать «10 мин» у записи 9:59 — соврать в большую сторону. Обрезаем вниз.
    #[test]
    fn неполная_секунда_обрезается_вниз() {
        assert_eq!(duration_sec(wav_bytes(599) + WAV_BYTES_PER_SEC - 1), 599);
    }

    /// Файл, у которого есть заголовок и нет данных, остаётся после падения
    /// посреди записи. Нулевая длительность честнее единицы.
    #[test]
    fn файл_без_звука_или_короче_заголовка_даёт_ноль() {
        assert_eq!(
            duration_sec(wav_bytes(0)),
            0,
            "один заголовок — ноль секунд"
        );
        assert_eq!(duration_sec(10), 0, "обрезанный файл не уходит в минус");
        assert_eq!(duration_sec(0), 0);
    }

    // ---- meta.json: exact recording duration ---------------------------

    /// Гвоздь задачи: файл-спутник знает точную длительность и обязан
    /// побеждать оценку по размеру `.wav`, даже когда сам `.wav` цел и
    /// расчёт по нему тоже возможен.
    #[test]
    fn meta_json_побеждает_расчёт_по_размеру_даже_когда_wav_на_месте() {
        let list = group_full(
            &[(None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400))],
            &[(("2026-07-17_14-45_zoom", None), 3120)],
        );
        assert_eq!(
            list[0].duration_sec, 3120,
            "meta.json важнее расчёта по размеру, а не наоборот"
        );
    }

    /// Без файла-спутника ничего не меняется — старые записи, для которых он
    /// никогда не создавался, по-прежнему считаются по размеру.
    #[test]
    fn без_meta_json_расчёт_остаётся_по_размеру_как_раньше() {
        let list = group(&[(None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(600))]);
        assert_eq!(list[0].duration_sec, 600);
    }

    /// `read_duration_meta` не роняет обход каталога: битый или пустой
    /// файл-спутник просто не даёт ключа, а не паникует и не топит остальные
    /// записи в `collect_files`.
    #[test]
    fn битый_meta_json_не_читается_и_не_роняет_список() {
        let dir = std::env::temp_dir().join(format!(
            "mr-meta-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("создать временный каталог");

        std::fs::write(dir.join("битый.meta.json"), b"{ not json").unwrap();
        assert_eq!(read_duration_meta(&dir.join("битый.meta.json")), None);

        std::fs::write(dir.join("пустой.meta.json"), b"").unwrap();
        assert_eq!(read_duration_meta(&dir.join("пустой.meta.json")), None);

        assert_eq!(read_duration_meta(&dir.join("нет-такого.meta.json")), None);

        std::fs::write(dir.join("живой.meta.json"), br#"{"v":1,"duration_sec":42}"#).unwrap();
        assert_eq!(read_duration_meta(&dir.join("живой.meta.json")), Some(42));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_ignores_and_preserves_legacy_transcript_folders() {
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = Scratch(std::env::temp_dir().join(format!(
            "mr-audio-list-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
        let base = "2026-10-07_10-00_meeting";
        let month = dir.0.join("2026-10");
        let legacy = month.join(format!("{base}.transcript"));
        let root_legacy = dir.0.join(format!("{base}.transcript"));
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::create_dir_all(&root_legacy).unwrap();
        std::fs::write(legacy.join("summary.md"), b"old transcript").unwrap();
        // Even audio nested inside a legacy folder must not become a recording.
        std::fs::write(root_legacy.join(format!("{base}.wav")), b"nested audio").unwrap();
        std::fs::write(
            month.join(format!("{base}.meta.json")),
            br#"{"duration_sec":42}"#,
        )
        .unwrap();
        let found = collect_files(&dir.0).unwrap();
        assert!(group_recordings(found.files, &found.durations).is_empty());
        std::fs::write(month.join(format!("{base}.wav")), b"audio").unwrap();
        let found = collect_files(&dir.0).unwrap();
        let listed = group_recordings(found.files, &found.durations);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, base);
        assert_eq!(listed[0].folder.as_deref(), Some("2026-10"));
        assert_eq!(listed[0].duration_sec, 42);
        assert_eq!(
            std::fs::read(legacy.join("summary.md")).unwrap(),
            b"old transcript"
        );
        assert_eq!(
            std::fs::read(root_legacy.join(format!("{base}.wav"))).unwrap(),
            b"nested audio"
        );
    }

    #[test]
    fn recording_busy_only_tracks_the_matching_capture() {
        let status = Status::default();
        assert!(!recording_busy(
            &status,
            Some("2026-07"),
            "2026-07-17_14-30_zoom"
        ));
        status.set_current_recording(Some(("2026-07".into(), "2026-07-17_14-30_zoom".into())));
        assert!(recording_busy(
            &status,
            Some("2026-07"),
            "2026-07-17_14-30_zoom"
        ));
        assert!(!recording_busy(&status, Some("2026-07"), "other"));
        assert!(!recording_busy(&status, None, "2026-07-17_14-30_zoom"));
    }

    fn путь(folder: Option<&str>, base: &str) -> Result<PathBuf, String> {
        recording_dir(Path::new("/записи"), folder, base)
    }

    #[test]
    fn запись_из_корня_показывается_самим_корнем() {
        assert_eq!(
            путь(None, "2026-07-17_14-30_zoom"),
            Ok(PathBuf::from("/записи"))
        );
    }

    #[test]
    fn запись_из_месячной_папки_показывается_этой_папкой() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_zoom"),
            Ok(PathBuf::from("/записи/2026-07"))
        );
    }

    /// Из окна приходит имя папки, а не путь. Всё, что не `YYYY-MM`, — не наша
    /// подпапка: `collect_files` в другие и не заходит.
    #[test]
    fn папкой_может_быть_только_месячная() {
        for чужое in [
            "..",
            ".",
            "/",
            "2026-7",
            "2026-07/..",
            "../2026-07",
            "чужое",
        ] {
            assert!(
                путь(Some(чужое), "2026-07-17_14-30_zoom").is_err(),
                "«{чужое}» не месячная папка и открываться не должна"
            );
        }
    }

    /// Основу человек меняет сам — и «Переименовать», и руками в Finder, где
    /// правил нет вообще. Уйти по ней вверх из каталога записей нельзя.
    #[test]
    fn основа_имени_не_выводит_за_каталог_записей() {
        for чужое in [
            "..",
            ".",
            "",
            "../секреты",
            "a/../../b",
            "/etc/passwd",
            "запись/",
        ] {
            assert!(
                путь(Some("2026-07"), чужое).is_err(),
                "«{чужое}» не имя записи и открываться не должно"
            );
        }
    }

    /// Проверка основы не зависит от того, в подпапку идём или нет: правило
    /// «всё, что пришло из окна, проверено» не должно держаться на ветке.
    #[test]
    fn чужая_основа_отвергается_в_месячной_папке() {
        assert!(путь(Some("2026-07"), "../секреты").is_err());
    }

    /// Кириллица, точки и пробелы внутри имени — обычное дело после
    /// переименования; отвергать их незачем.
    #[test]
    fn обычное_переименованное_имя_проходит() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_созвон с артёмом v1.2"),
            Ok(PathBuf::from("/записи/2026-07"))
        );
    }

    // ---- tauri.conf.json ----------------------------------------------------

    /// Караул при значении, которое выглядит опечаткой и ею не является.
    ///
    /// `bundle.macOS.minimumSystemVersion` стоит `11.0`, хотя приложению нужна
    /// macOS 14.4, — иначе отказ на старой системе не дойдёт до человека:
    /// запуск перехватит Finder и откажет своими словами. Рассуждение записано
    /// в нескольких местах (докблок `MIN_MACOS` в `src/capture/macos.rs`,
    /// design-документ, план, README, `scripts/check-tap-lazy-bind.sh`), и ни
    /// одно из них не лежит внутри `src-tauri/` — то есть там, куда смотрит
    /// человек, решивший «привести в соответствие». JSON комментариев не держит;
    /// этот тест — единственный комментарий, который правка не сможет не
    /// заметить.
    ///
    /// Файл берётся `include_str!`, а не чтением с диска: так тест не зависит
    /// ни от рабочего каталога, ни от платформы, а расхождение всплывает уже
    /// при компиляции, если файл вообще исчезнет. `cfg` на нём нет намеренно —
    /// ключ правят чаще всего как раз не с macOS.
    #[test]
    fn минимальная_версия_macos_в_бандле_осталась_11_0() {
        const CONF: &str = include_str!("../tauri.conf.json");
        let conf: serde_json::Value =
            serde_json::from_str(CONF).expect("src-tauri/tauri.conf.json — не валидный JSON");

        assert_eq!(
            conf["bundle"]["macOS"]["minimumSystemVersion"].as_str(),
            Some("11.0"),
            "\n\
             bundle.macOS.minimumSystemVersion обязан остаться \"11.0\".\n\
             \n\
             Расхождение с настоящим требованием (macOS 14.4) выглядит \
             недосмотром, но им не является.\n\
             \n\
             ЗАЧЕМ. Этот ключ — LSMinimumSystemVersion в Info.plist, то есть гейт \
             Finder'а.\n\
             При 11.0 приложение на старой системе запускается, доходит до main, \
             зовёт\n\
             unsupported_reason() и объясняет человеку, что нужна 14.4 и почему. \
             При 14.4\n\
             запуск перехватит сама macOS и откажет своими словами — пользователь \
             не узнает,\n\
             чего именно не хватает, а мы не узнаем, что он вообще пытался.\n\
             \n\
             ЧЕГО ЭТОТ КЛЮЧ БОЛЬШЕ НЕ ДЕЛАЕТ. До 2026-08-17 он же держал \
             живучесть процесса:\n\
             в Tauri 2 он задаёт и MACOSX_DEPLOYMENT_TARGET, а ld при 11.x \
             связывал символы\n\
             тапа лениво. Опора оказалась зависящей от версии линкера — на \
             ld-1053.12\n\
             связывание жадное уже при 11.0. Теперь живучесть держит слабая \
             линковка\n\
             (-Wl,-weak_framework,CoreAudio в обоих build.rs), и её стерегут \
             отдельные тесты:\n\
             корневой_крейт_линкует_coreaudio_слабо (src/lib.rs) и \
             gui_крейт_линкует_coreaudio_слабо.\n\
             \n\
             Замеры и рассуждение целиком — докблок MIN_MACOS в \
             src/capture/macos.rs.\n\
             Проверка на собранном бандле: npm run check-tap-lazy-bind\n"
        );
    }

    // ---- build.rs -----------------------------------------------------------

    /// То же, что `корневой_крейт_линкует_coreaudio_слабо` в ядре, но для этого
    /// крейта: `cargo:rustc-link-arg` между крейтами не наследуется, линк у
    /// GUI-бинаря свой, и флаг ему нужен свой.
    ///
    /// Два почти одинаковых теста вместо одного общего — потому что забыть флаг
    /// можно в каждом файле по отдельности, и падать должен тот тест, который
    /// назовёт нужный файл.
    #[test]
    fn gui_крейт_линкует_coreaudio_слабо() {
        const BUILD_RS: &str = include_str!("../build.rs");
        assert!(
            BUILD_RS.contains("-Wl,-weak_framework,CoreAudio"),
            "\n\
             В src-tauri/build.rs пропал флаг слабой линковки CoreAudio:\n\
             \x20   println!(\"cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio\");\n\
             \n\
             Без него на macOS старее 14.4 dyld убивает GUI с \"Symbol not found:\n\
             _AudioHardwareCreateProcessTap\" ДО main: ни окна, ни тоста, ни \
             объяснения.\n\
             \n\
             Докблок MIN_MACOS в src/capture/macos.rs, проверка на бандле:\n\
             \x20   npm run check-tap-lazy-bind\n"
        );
    }

    /// `trash` на Windows требует явно выбранную модель COM: в его
    /// `windows.rs` стоит НАМЕРЕННАЯ ошибка компиляции, если не задана ни
    /// `coinit_multithreaded`, ни `coinit_apartmentthreaded`. Обе входят в
    /// `default` крейта, а у нас `default-features = false` — и вместе с
    /// умолчаниями срезается COM.
    ///
    /// Тест читает манифест, а не полагается на сборку: ошибка спрятана за
    /// `cfg(windows)`, поэтому ни `cargo test` на macOS, ни ревью диффа её не
    /// увидят — красным станет только Windows-раннер, и через девять минут
    /// компиляции. Здесь она падает сразу и на любой платформе.
    #[test]
    fn trash_объявлен_с_моделью_com_для_windows() {
        const CARGO_TOML: &str = include_str!("../Cargo.toml");
        let строка = CARGO_TOML
            .lines()
            .find(|l| l.trim_start().starts_with("trash"))
            .expect("зависимость `trash` пропала из src-tauri/Cargo.toml");
        assert!(
            строка.contains("coinit_apartmentthreaded") || строка.contains("coinit_multithreaded"),
            "\n\
             В src-tauri/Cargo.toml у `trash` не осталось модели COM:\n\
             \x20   {строка}\n\
             \n\
             На Windows это не предупреждение, а отказ сборки:\n\
             \x20   error[E0070]: invalid left-hand side of assignment\n\
             \x20   trash-5.2.6/src/windows.rs:278\n\
             \n\
             Вернуть одну из фич (крейт по умолчанию берёт первую):\n\
             \x20   features = [\"coinit_apartmentthreaded\"]\n"
        );
    }

    /// `productName` в `tauri.conf.json` задаёт имя собранного бандла, а
    /// `scripts/check-tap-lazy-bind.sh` ищет бинарь внутри него по этому пути.
    /// Переименование приложения ломает скрипт молча: он просто не найдёт файл
    /// — причём только на macOS и только когда кто-то решит его запустить, а
    /// README предлагает эту команду постороннему человеку.
    #[test]
    fn скрипт_проверки_бандла_знает_текущее_имя_приложения() {
        const CONF: &str = include_str!("../tauri.conf.json");
        const SCRIPT: &str = include_str!("../../scripts/check-tap-lazy-bind.sh");
        let conf: serde_json::Value =
            serde_json::from_str(CONF).expect("tauri.conf.json обязан быть валидным JSON");
        let имя = conf["productName"]
            .as_str()
            .expect("productName в tauri.conf.json");
        assert!(
            SCRIPT.contains(&format!("{имя}.app")),
            "\n\
             scripts/check-tap-lazy-bind.sh ищет бандл не под тем именем.\n\
             \n\
             productName в tauri.conf.json: {имя}\n\
             значит собирается:              {имя}.app\n\
             \n\
             Поправить DEFAULT_BIN в скрипте — иначе `npm run check-tap-lazy-bind`\n\
             из README не найдёт файл и упадёт на постороннем человеке.\n"
        );
    }
}
