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
mod local;
mod recording;
mod rename;
mod retention;
mod status;
mod transcribe;
mod update;
mod whisper_cpp;
mod tray;
mod window;
use window::show_main_window;

use audio::Ctl;
use config::Config;
use imbalance::Cache;
use meeting_recorder::session::Event;
use serde::{Deserialize, Serialize};
use status::{Snapshot, Status};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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

/// Одна ждущая или обрабатываемая транскрипция.
#[derive(Clone, PartialEq, Debug)]
struct QueueItem {
    folder: Option<String>,
    base: String,
}

/// Что стало с записью, которую попросили отменить.
///
/// Три исхода, а не «получилось / не получилось», потому что снаружи они
/// требуют разного: снятую из хвоста очереди никто больше не тронет, и сказать
/// об этом обязана сама команда; прерванную на ходу объявляет воркер, когда
/// расшифровка действительно остановится; а неизвестную объявлять некому и
/// нечего.
#[derive(PartialEq, Debug)]
enum Cancelled {
    /// Записи нет ни в очереди, ни в работе — отменять нечего. Не ошибка:
    /// расшифровка могла закончиться ровно между показом меню и нажатием.
    Unknown,
    /// Стояла в очереди и снята, не начавшись. Внутри — кому и какую позицию
    /// сообщить заново; идущая запись сюда НЕ попадает (см. `cancel`).
    Dropped(Vec<(usize, QueueItem)>),
    /// Обрабатывалась прямо сейчас: воркеру послан сигнал остановиться.
    /// Внутри — id задач шлюза, накопленных для неё (см. поле `jobs`),
    /// заодно отобранные из-под того же лока, что принял решение
    /// «идущая, гасим». Отдельный вызов за теми же id уже ПОСЛЕ этого не
    /// годится: между возвратом `cancel()` и следующим захватом лока воркер,
    /// разбуженный нашим же `oneshot`, успевает дойти до `finish_front()` и
    /// опустошить `jobs` первым — и тогда `DELETE` на шлюз просто не улетает.
    Stopped(Vec<String>),
}

/// Изменяемая часть очереди — под одним локом целиком.
///
/// Разложить эти три поля по трём мьютексам значило бы завести гонку на ровном
/// месте: отмена решает, снимать запись из списка или прерывать её на ходу,
/// ровно по тому, начал ли воркер `items[0]`. Читайся `items` и `running`
/// порознь, отмена успела бы застать «ещё не начал» между `recv` воркера и
/// подъёмом флага — и вычеркнула бы из списка запись, которая уже пошла в
/// работу и всё равно дошла бы до конца.
#[derive(Default)]
struct Pending {
    items: VecDeque<QueueItem>,
    /// Поднят на всё время обработки `items[0]`, отдельно от `cancel`: послать
    /// в `oneshot` можно ровно один раз, поэтому после первой отмены `cancel`
    /// пуст — и без этого флага повторное нажатие приняло бы идущую запись за
    /// ещё не начатую и вычеркнуло бы её из `items`, а воркер потом снял бы с
    /// фронта уже чужую.
    running: bool,
    /// Куда сказать идущей расшифровке «хватит». `Some` ровно тогда, когда
    /// `running` поднят и отмены ещё не было.
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    /// Идентификаторы задач шлюза, уже отправленных для `items[0]`.
    ///
    /// Живут под тем же локом, что `running` и `cancel`, по той же причине:
    /// отмена решает «снять из очереди или прервать на ходу» и «что гасить на
    /// шлюзе» одним снимком. Читайся они порознь, отмена успела бы взять id
    /// уже следующей записи.
    jobs: Vec<String>,
}

/// Очередь транскрипций на всё приложение: `items[0]` обрабатывается прямо
/// сейчас (или вот-вот начнёт), `items[1..]` ждут своей очереди в порядке
/// постановки.
///
/// Один воркер (см. `spawn_transcribe_worker`) читает `rx` строго
/// последовательно — это и есть очередь, а не просто «не начинать вторую,
/// пока не кончится первая», как было раньше: там второй клик отвечал
/// ошибкой и требовал повторного клика вручную после первой.
///
/// `items` и канал меняются под одним и тем же локом (`enqueue`), поэтому
/// порядок в `items` всегда совпадает с порядком, в котором воркер реально
/// получит записи — иначе позиции, которые видит UI, могли бы разойтись с
/// тем, что происходит на самом деле.
///
/// **Отмена ломает равенство «канал = очередь», но не порядок.** Забрать
/// запись из середины `tokio::mpsc` нельзя, поэтому `cancel` вычёркивает её
/// только из `items` — в канале остаётся мёртвая запись. Уцелевшее свойство:
/// `items` всегда ПОДПОСЛЕДОВАТЕЛЬНОСТЬ того, что ещё лежит в канале. Значит,
/// пришедшая воркеру запись, не совпавшая с текущим фронтом, — это в точности
/// отменённая, и её надо пропустить; проверку делает `start_front`.
struct TranscribeQueue {
    pending: Mutex<Pending>,
    tx: tokio::sync::mpsc::UnboundedSender<QueueItem>,
}

impl TranscribeQueue {
    fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<QueueItem>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { pending: Mutex::new(Pending::default()), tx }, rx)
    }

    /// Ставит запись в очередь, если её там ещё нет — двойной клик по кнопке
    /// не создаёт вторую копию, а просто отдаёт ту же позицию, что и первый.
    /// Позиция 1-индексирована: 1 — обрабатывается прямо сейчас, 2 —
    /// следующая, и так далее.
    fn enqueue(&self, item: QueueItem) -> Result<usize, String> {
        let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
        if let Some(pos) = pending.items.iter().position(|i| *i == item) {
            return Ok(pos + 1);
        }
        pending.items.push_back(item.clone());
        let position = pending.items.len();
        self.tx.send(item).map_err(|_| "воркер транскрипции недоступен".to_string())?;
        Ok(position)
    }

    /// Воркер получил запись из канала и спрашивает разрешения начать.
    ///
    /// `None` — запись отменили, пока она ждала: в `items` её больше нет, а в
    /// канале осталась мёртвая копия (см. докблок типа). Пропустить её здесь
    /// обязательно: иначе расшифровка пошла бы после отмены, а `finish_front`
    /// снял бы с фронта чужую запись.
    ///
    /// `Some(rx)` — можно работать, а по этому каналу придёт отмена.
    fn start_front(&self, item: &QueueItem) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        if pending.items.front() != Some(item) {
            return None;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        pending.running = true;
        pending.cancel = Some(tx);
        Some(rx)
    }

    /// Запомнить отправленную задачу шлюза. Зовётся из `run_transcription`
    /// сразу после `submit`, до того как начнётся ожидание.
    fn note_job(&self, job_id: String) {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        pending.jobs.push(job_id);
    }

    /// Убирает обработанную запись с фронта и отдаёт тех, кто остался — под
    /// тем же локом, что и `enqueue`, чтобы снимок для пересчёта позиций не
    /// мог оказаться устаревшим уже в момент чтения.
    fn finish_front(&self) -> Vec<QueueItem> {
        let mut pending = self.pending.lock().expect("лок очереди транскрипции");
        pending.running = false;
        pending.cancel = None;
        pending.jobs.clear();
        pending.items.pop_front();
        pending.items.iter().cloned().collect()
    }

    /// Снимает запись с очереди или останавливает её на ходу.
    ///
    /// Идущая запись из `items` НЕ вычёркивается: её снимет с фронта
    /// `finish_front`, когда воркер действительно остановится. Вычеркнуть её
    /// здесь значило бы сдвинуть фронт под работающим воркером, и тот снял бы
    /// потом следующую, ни разу не начатую.
    ///
    /// В `Dropped` едут только ждущие и только с новыми позициями: идущей
    /// записи `queued:1` слать нельзя — на экране у неё стадия («отправка»,
    /// «расшифровка»), и позиция поверх стадии выглядела бы откатом назад.
    fn cancel(&self, item: &QueueItem) -> Result<Cancelled, String> {
        let mut pending = self.pending.lock().map_err(|e| e.to_string())?;
        let Some(pos) = pending.items.iter().position(|i| i == item) else {
            return Ok(Cancelled::Unknown);
        };
        if pos == 0 && pending.running {
            // `take` — потому что послать в oneshot можно единожды; повторное
            // нажатие попадёт сюда же по флагу `running` и просто ничего не
            // сделает.
            if let Some(tx) = pending.cancel.take() {
                let _ = tx.send(());
            }
            // Забираем id ИЗ ТОГО ЖЕ лока, а не отдельным вызовом снаружи:
            // `tx.send(())` выше будит воркер немедленно, и он может успеть
            // дойти до `finish_front()` (который чистит `jobs`) раньше, чем
            // вызывающий код возьмёт лок ещё раз. Отдельный метод, забирающий
            // `jobs` вторым вызовом уже после `cancel()`, — гонка, которая
            // молча теряет id и оставляет задачу висеть на шлюзе.
            let jobs = std::mem::take(&mut pending.jobs);
            return Ok(Cancelled::Stopped(jobs));
        }
        pending.items.remove(pos);
        let skip = usize::from(pending.running);
        let moved = pending
            .items
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(i, q)| (i + 1, q.clone()))
            .collect();
        Ok(Cancelled::Dropped(moved))
    }

    /// Есть ли запись в очереди — ждёт своей позиции или обрабатывается прямо
    /// сейчас. Не различает эти два случая: обеим нельзя мешать одинаково —
    /// трогать файлы под расшифровкой, которая вот-вот начнётся, так же
    /// плохо, как под той, что уже идёт (см. `recording_busy`).
    fn contains(&self, item: &QueueItem) -> bool {
        self.pending
            .lock()
            .expect("лок очереди транскрипции")
            .items
            .contains(item)
    }
}

/// Занята ли запись прямо сейчас — идёт запись или расшифровка (в очереди или
/// уже в работе). Общая для `delete_recording` и автоочистки: у обеих один и
/// тот же список исключений, и разъехаться этому списку в двух местах нельзя.
fn recording_busy(status: &Status, queue: &TranscribeQueue, folder: Option<&str>, base: &str) -> bool {
    let recording = status
        .snapshot()
        .current_recording
        .is_some_and(|c| Some(c.folder.as_str()) == folder && c.base == base);
    let transcribing = queue.contains(&QueueItem { folder: folder.map(str::to_string), base: base.to_string() });
    recording || transcribing
}

/// Воркер очереди: читает канал строго по одной записи за раз, поэтому
/// параллельных транскрипций не бывает в принципе — не только по логике
/// `enqueue`, но и потому, что второй `.recv()` физически не начнётся, пока
/// первый `await` внутри цикла не вернётся.
///
/// Ошибку `run_transcription` не пробрасывает и не логирует отдельно: она уже
/// ушла тому, кто умеет её показать, через `emit_transcribe_error` внутри
/// самой функции — здесь важно только то, что очередь обязана двигаться
/// дальше независимо от того, чем кончилась предыдущая запись.
///
/// Отмена идущей записи — это `select!`, который бросает саму расшифровку
/// недоделанной. Бросить её безопасно ровно потому, что все точки ожидания у
/// неё сетевые: файлы пишутся сплошным куском в самом конце, между ними нет ни
/// одного `await`, и оборваться посередине набора `.md`/`.txt` расшифровка не
/// может.
///
/// Задание на стороне шлюза при этом НЕ остаётся просто висеть — но гасится
/// не отсюда. `cancel_transcription` (см. её докблок) шлёт `DELETE` по id,
/// отобранным из `Pending::jobs` тем же локом, что разбудил этот `select!`; а
/// если до отмены не дошло, но связь со шлюзом пропала совсем — ту же задачу
/// гасит сам `transcribe::poll_until_done`, вернув `PollLost`. Незагашенными
/// остаются только два случая, и оба осознанно вне объёма: выход из
/// приложения посреди расшифровки (это ближе к персистентной очереди) и
/// отмена во время ещё не завершённого `submit` — id тогда ещё не существует,
/// гасить нечего.
fn spawn_transcribe_worker(
    app: AppHandle,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<QueueItem>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(item) = rx.recv().await {
            let Some(cancel) = app.state::<TranscribeQueue>().start_front(&item) else {
                // Отменена, пока ждала: об этом уже сказала сама команда.
                continue;
            };
            let stopped = tokio::select! {
                // `biased` — чтобы уже дошедшая до конца расшифровка считалась
                // завершённой, а не отменённой: нажатие, опоздавшее на доли
                // секунды, не должно превращать готовую расшифровку в
                // «отменено» при том, что файлы на диске уже лежат.
                biased;
                _ = run_transcription(item.folder.clone(), item.base.clone(), app.clone()) => false,
                _ = cancel => true,
            };
            if stopped {
                emit_transcribe_cancelled(&app, &item.folder, &item.base);
            }
            let remaining = app.state::<TranscribeQueue>().finish_front();
            for (i, next) in remaining.iter().enumerate() {
                emit_transcribe_progress(&app, &next.folder, &next.base, &format!("queued:{}", i + 1));
            }
        }
    });
}

/// Через сколько чистка старого аудио повторяет обход каталога. Раз в сутки,
/// а не чаще: чистка ходит по файловой системе, и гонять её каждую минуту —
/// работа без пользы. Смену конфига между тиками эта задача не пропускает: у
/// неё нет своего кеша срока, `run_retention_cleanup` читает `Config::load`
/// заново на каждом проходе.
const RETENTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Завести фоновую чистку: один проход сразу после старта и затем по одному
/// на каждые сутки, пока приложение работает.
///
/// `spawn_blocking` вокруг тела, а не голый цикл на async-задаче: в отличие
/// от команд `invoke_handler`, которые Tauri сам разгружает на пул потоков,
/// задача, поднятая напрямую через `async_runtime::spawn`, крутится на общем
/// рантайме — синхронный обход каталога внутри неё держал бы этот рантайм
/// занятым, пока не дочитает диск.
fn spawn_retention_worker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            let handle = app.clone();
            let _ = tauri::async_runtime::spawn_blocking(move || run_retention_cleanup(&handle)).await;
            tokio::time::sleep(RETENTION_INTERVAL).await;
        }
    });
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

/// Один проход чистки: какие записи набежали за срок — у тех звук уезжает в
/// корзину. Расшифровка не трогается никогда (см. `retention::delete_audio`).
///
/// Отказ на конфиге (нет срока — то есть «никогда», см. докблок поля в
/// `config.rs`) и отказ на чтении каталога останавливают весь проход — без
/// каталога нечего перебирать. Отказ же на отдельной записи (файл занят,
/// нет прав) — нет: он идёт в лог и не мешает остальным записям этого же
/// прохода.
fn run_retention_cleanup(app: &AppHandle) {
    let Some(days) = Config::load(app).audio_retention_days else {
        return;
    };
    let root = recordings_root();
    let found = match collect_files(&root) {
        Ok(f) => f,
        Err(e) => {
            log::warn!("автоочистка звука: не удалось прочитать {}: {e}", root.display());
            return;
        }
    };
    let list = group_recordings(found.files, &found.transcripts, &found.durations);
    let status = app.state::<Status>();
    let queue = app.state::<TranscribeQueue>();
    let candidates: Vec<retention::Candidate> = list
        .iter()
        .map(|r| retention::Candidate {
            folder: r.folder.clone(),
            base: r.name.clone(),
            has_wav: r.mic || r.system,
            has_transcript: r.transcript,
            busy: recording_busy(&status, &queue, r.folder.as_deref(), &r.name)
                || app.state::<callabo::Uploads>().busy(r.folder.as_deref(), &r.name),
        })
        .collect();
    let now = chrono::Local::now();
    for (folder, base) in retention::due_for_cleanup(&candidates, days, now) {
        let dir = match &folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        if let Err(e) = app.state::<callabo::Uploads>().while_idle(folder.as_deref(), &base, || retention::delete_audio(&dir, &base)) {
            log::warn!("автоочистка звука «{base}»: {e}");
        }
    }
}

/// Одна запись: пара дорожек под общим именем.
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
    /// Рядом с дорожками лежит папка `<name>.transcript` с готовой расшифровкой.
    ///
    /// Только «да/нет»: разбирать содержимое папки незачем — окну нужно лишь
    /// решить, предлагать ли «Открыть расшифровку» вместо «Расшифровать».
    transcript: bool,
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
    /// `true` — на эту запись прямо сейчас пишутся дорожки. Заполняется в
    /// `list_recordings` после группировки, сверкой со `Status::current_recording`
    /// — та же причина, что у `quiet_mic_db`: группировка чистая и файлов не
    /// читает, а это сверка не с диском, а с состоянием аудио-потока.
    recording_now: bool,
    /// Completed private Callabo upload, persisted in a non-secret sidecar.
    callabo_workspaces: Vec<callabo::UploadedWorkspace>,
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
/// Только `duration_sec` разбирается — `v` в поле не заведён специально:
/// формат сегодня один-единственный, и место под будущую несовместимую
/// правку не то же самое, что код, который её уже умеет читать. Незнакомые
/// поля `serde` тихо игнорирует сам по себе.
#[derive(Deserialize)]
struct RecordingMeta {
    #[serde(default)]
    duration_sec: u32,
}

/// Длительность из файла-спутника, если он лежит рядом и читается.
///
/// `None` — файла нет, он не открылся или его содержимое не разбирается как
/// JSON нужной формы: во всех трёх случаях вызывающий обязан молча откатиться
/// на расчёт по размеру `.wav` (`duration_sec` выше), а не уронить список
/// записей. Битый файл-спутник — это файл, у которого повезло меньше, чем
/// дорожкам, а не повод перестать показывать запись целиком.
fn read_duration_meta(path: &Path) -> Option<u32> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<RecordingMeta>(&content)
        .ok()
        .map(|m| m.duration_sec)
}

/// Склеить дорожки в записи по основе имени И папке.
///
/// Имя дорожки — `{основа}.{mic|system}.wav`, где основа это
/// `YYYY-MM-DD_HH-MM_источник` с необязательным `_N` у повторов в ту же минуту
/// (см. `app::free_name_pair`). Группируем срезанием суффикса дорожки: пара
/// склеивается обратно ровно тем же правилом, которым её разложили.
///
/// **Ключ — пара `(base, folder)`, а не одна `base`.** Прежняя версия считала
/// «дата в основе однозначно задаёт месячную папку, поэтому одна запись не
/// может лежать в двух папках сразу» — эта посылка ложна в двух достижимых
/// сценариях:
///
/// 1. Прерванная миграция: `scripts/migrate-to-month-folders.sh` переносит
///    ПОФАЙЛОВО (`for entry in *`, один `mv` на файл) и на конфликте выходит с
///    кодом 1, не откатывая уже перенесённые члены тройки. После такого
///    прогона `.mic.wav` может остаться в корне, а `.system.wav` — уже уехать
///    в `2026-07/`.
/// 2. Коллизия имён между корнем и месячной папкой: `free_name_pair`
///    (`src/app.rs`) проверяет занятость имени только внутри целевой месячной
///    папки — список файлов корня в неё не попадает. Пока миграция не
///    прогнана, новая запись в ту же минуту с тем же источником, что и старая
///    корневая запись, получит имя БЕЗ суффикса `_2` и совпадёт с ней по основе.
///
/// Ключ только по `base` в обоих случаях схлопнул бы две половинки в одну
/// «полную» запись, чья `folder` бралась бы от первого встреченного файла:
/// «Переименовать» переименовало бы только эту половину, вторая дорожка (и,
/// возможно, `.transcript`) осталась бы под старым именем молча — `rename_
/// recording` вернул бы `Ok`, ничего не сообщив о разрыве. Ключ `(base,
/// folder)` вместо этого честно показывает такую ситуацию как ДВЕ неполные
/// записи — ровно то, чем она и является на диске.
///
/// `BTreeMap` по-прежнему сортирует в первую очередь по `base` — оно первый
/// компонент кортежа, `folder` работает только тайбрейком при совпадении
/// основы. Основа начинается с `YYYY-MM-DD_HH-MM`, так что лексикографический
/// порядок и есть хронологический. Наверх список отдаётся перевёрнутым:
/// свежее сверху.
///
/// `transcripts` — ключи `(основа, папка)` найденных рядом папок
/// `<основа>.transcript`, тем же ключом, что и группировка.
///
/// Папка расшифровки без единой дорожки рядом — не выдумка, а прямое
/// следствие автоочистки старого аудио (см. `retention.rs`): та трогает
/// только `.wav`, `.transcript` не касается никогда, и после неё на диске
/// закономерно остаётся именно такая пара. Раньше это считалось «мусором» и
/// в список не попадало вовсе — теперь это легальное состояние записи, и
/// вторым проходом ниже для него заводится запись с `mic: false, system:
/// false`: показать это честно (в UI — `retention.cleared`, см. `ui/main.js`)
/// можно только если запись вообще есть в списке, а открыть расшифровку и
/// удалить то, что от встречи осталось, — только если у неё есть на что жать.
///
/// Отделено от обхода каталога намеренно: правило склейки — это единственное
/// здесь, что можно сломать незаметно (отсутствие дорожки в паре UI показывает
/// предупреждением, и ошибка в группировке выглядела бы как испорченная запись).
/// Проверять его через `read_dir` значило бы держать в тесте настоящие файлы
/// ради логики, которой файлы не нужны.
///
/// `durations` — длительности из файлов-спутников `<основа>.meta.json`
/// (см. `read_duration_meta`), тем же ключом `(основа, папка)`, что и
/// `transcripts`. Разбор их содержимого сюда не спущен намеренно, по той же
/// причине, что и обход каталога — это чтение файлов, а группировка обязана
/// оставаться чистой функцией, проверяемой без диска.
///
/// Файл-спутник побеждает расчёт по размеру `.wav`, даже когда сам `.wav` на
/// месте: он знает точную длительность записи (секунды от старта до стопа),
/// расчёт по размеру — это только оценка, округлённая вниз до целой секунды и
/// зависящая от того, что дописал `hound` в заголовок. Отсутствие или порча
/// файла-спутника (ключа нет в `durations`) не меняет ничего — остаётся
/// прежний расчёт по размеру, как до этой задачи.
fn group_recordings(
    files: impl IntoIterator<Item = (Option<String>, String, u64)>,
    transcripts: &HashSet<(String, Option<String>)>,
    durations: &HashMap<(String, Option<String>), u32>,
) -> Vec<Recording> {
    let mut found: BTreeMap<(String, Option<String>), Recording> = BTreeMap::new();
    for (folder, file, size) in files {
        let combined = !file.ends_with(".mic.wav") && !file.ends_with(".system.wav");
        let (base, is_mic) = match (file.strip_suffix(".mic.wav"), file.strip_suffix(".system.wav"))
        {
            (Some(b), _) => (b.to_string(), true),
            (_, Some(b)) => (b.to_string(), false),
            // Не наша дорожка — чужой файл в каталоге, не наше дело.
            _ => match file.strip_suffix(".wav") {
                Some(b) if meeting_recorder::storage::split_name(b).is_some() => (b.to_string(), true),
                _ => continue,
            },
        };
        let key = (base.clone(), folder.clone());
        let transcript = transcripts.contains(&key);
        let rec = found.entry(key).or_insert(Recording {
            name: base,
            folder,
            mic: false,
            system: false,
            size: 0,
            transcript,
            duration_sec: 0,
            quiet_mic_db: None,
            recording_now: false,
            callabo_workspaces: vec![],
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
        rec.duration_sec = rec.duration_sec.max(duration_sec(size) / if combined { 2 } else { 1 });
    }
    // Второй проход: расшифровки, у которых обеих дорожек уже нет (см. докблок
    // выше). Только `or_insert` — запись с хотя бы одной дорожкой уже создана
    // первым проходом и трогать её здесь незачем.
    for key in transcripts {
        let (base, folder) = key;
        found.entry(key.clone()).or_insert(Recording {
            name: base.clone(),
            folder: folder.clone(),
            mic: false,
            system: false,
            size: 0,
            transcript: true,
            duration_sec: 0,
            quiet_mic_db: None,
            recording_now: false,
            callabo_workspaces: vec![],
        });
    }
    // Третий проход: файл-спутник побеждает расчёт по размеру — но только для
    // записи, которая уже есть в `found` (хотя бы дорожка или расшифровка).
    // Файл-спутник без единого следа рядом на диске не заводит запись сам —
    // это не его роль, он только уточняет длительность уже существующей.
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

/// Что нашлось в каталоге записей за один обход.
///
/// Дорожки и папки расшифровок собираются вместе, потому что берутся из одного
/// и того же `read_dir`: второй проход по тому же дереву стоил бы столько же,
/// сколько первый, и мог бы застать каталог уже изменившимся.
#[derive(Default, PartialEq, Debug)]
struct Found {
    /// `(папка, имя файла, размер)` — всё, что лежит файлами.
    files: Vec<(Option<String>, String, u64)>,
    /// `(основа, папка)` записей, у которых рядом есть `<основа>.transcript`.
    transcripts: HashSet<(String, Option<String>)>,
    /// `(основа, папка)` → длительность из `<основа>.meta.json`, если рядом
    /// нашёлся файл-спутник и он разобрался (см. `read_duration_meta`).
    /// Битый или отсутствующий файл просто не попадает сюда — ключа нет,
    /// `group_recordings` откатывается на расчёт по размеру `.wav`.
    durations: HashMap<(String, Option<String>), u32>,
}

/// Файлы корня плюс файлы месячных подпапок. Глубина ровно два уровня:
/// предсказуемо и не засасывает чужое дерево, если рядом окажется постороннее.
///
/// Каталоги не пропускаются целиком, как раньше: `<основа>.transcript` — это
/// папка (внутри `.md` и `.txt`, см. `run_transcription`), и другого признака
/// готовой расшифровки на диске нет. Внутрь мы не заходим — имени папки
/// достаточно, чтобы ответить «расшифровка есть».
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
                Ok(t) if t.is_dir() => {
                    if let Some(base) = name.strip_suffix(".transcript") {
                        out.transcripts
                            .insert((base.to_string(), folder.map(str::to_string)));
                    }
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
            // Расшифровка записи, которая осталась в корне и не переехала в
            // месячную папку, лежит тоже в корне — рядом со своими дорожками.
            Ok(t) if t.is_dir() => {
                if let Some(base) = name.strip_suffix(".transcript") {
                    out.transcripts.insert((base.to_string(), None));
                }
            }
            Ok(t) if t.is_file() => {
                if let Some(base) = name.strip_suffix(".meta.json") {
                    if let Some(dur) = read_duration_meta(&e.path()) {
                        out.durations.insert((base.to_string(), None), dur);
                    }
                }
                out.files.push((
                    None,
                    name,
                    e.metadata().map(|m| m.len()).unwrap_or(0),
                ));
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
/// условия на полную пару: запись без системного звука тоже может идти
/// прямо сейчас, и её тоже нельзя ни удалить, ни расшифровать заново.
#[tauri::command]
fn list_recordings(cache: tauri::State<Cache>, status: tauri::State<Status>) -> Result<Vec<Recording>, String> {
    let root = recordings_root();
    let found = collect_files(&root)?;
    let mut list = group_recordings(found.files, &found.transcripts, &found.durations);
    let current = status.snapshot().current_recording;
    for r in &mut list {
        let dir = match &r.folder {
            Some(f) => root.join(f),
            None => root.clone(),
        };
        let combined_path = dir.join(format!("{}.wav", r.name));
        r.callabo_workspaces = callabo::completed_workspaces(&dir, &r.name);
        if let Ok(reader) = hound::WavReader::open(&combined_path) {
            r.mic = true;
            r.system = reader.spec().channels >= 2;
            if !found.durations.contains_key(&(r.name.clone(), r.folder.clone())) {
                r.duration_sec = reader.duration() / reader.spec().sample_rate;
            }
        }
        r.recording_now = current
            .as_ref()
            .is_some_and(|c| Some(c.folder.as_str()) == r.folder.as_deref() && c.base == r.name);
        // Пометка имеет смысл только для полной пары: одинокая дорожка уже
        // помечена как неполная, и второе предупреждение о ней ничего не добавит.
        if !(r.mic && r.system) {
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
    cmd.spawn().map_err(|e| format!("не удалось открыть Finder/проводник: {e}"))?;
    Ok(())
}

/// Открыть каталог записей целиком.
#[tauri::command]
fn open_folder() -> Result<(), String> {
    let dir = recordings_root();
    // Иначе explorer откроет «Документы» вместо пустого несуществующего пути.
    std::fs::create_dir_all(&dir).map_err(|e| format!("не удалось создать {}: {e}", dir.display()))?;
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

/// Куда ведут пункты «Показать файлы» и «Открыть расшифровку».
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
fn recording_dir(
    root: &Path,
    folder: Option<&str>,
    base: &str,
    transcript: bool,
) -> Result<PathBuf, String> {
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
    if transcript {
        dir.push(format!("{base}.transcript"));
    }
    Ok(dir)
}

/// Открыть папку конкретной записи: месячную с дорожками или её расшифровку.
///
/// Несуществующую папку не создаём, в отличие от `open_folder`: пустой корень
/// значит «записей ещё не было», а пустая `<имя>.transcript` — враньё, будто
/// расшифровка есть. Честнее сказать, что открывать нечего.
#[tauri::command]
fn open_recording_folder(
    folder: Option<String>,
    base: String,
    transcript: bool,
) -> Result<(), String> {
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base, transcript)?;
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
const REPOSITORY_URL: &str = "https://github.com/mmaximov97/meeting-recorder";

#[tauri::command]
fn open_repository() -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(REPOSITORY_URL);
    cmd.spawn().map_err(|e| format!("не удалось открыть браузер: {e}"))?;
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
    let разрешена = url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:");
    if !разрешена {
        return Err(format!("недопустимая ссылка: {url}"));
    }
    #[cfg(target_os = "windows")]
    let mut cmd = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    cmd.arg(&url);
    cmd.spawn().map_err(|e| format!("не удалось открыть браузер: {e}"))?;
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
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base, false)?;
    uploads.while_idle(folder.as_deref(), &base, || rename::rename_recording(&dir, &base, &new_tail))
}

/// Удалить запись целиком: обе дорожки и папку расшифровки — в корзину, не
/// насовсем (см. `delete.rs`).
///
/// Путь собирается через `recording_dir`, а не прямым `join`, как у
/// `rename_recording`: удаление необратимее переименования (пусть и с
/// корзиной как страховкой), и лишняя проверка «`folder` — действительно
/// месячная папка, `base` — действительно один сегмент пути» здесь дешевле,
/// чем в rename.
///
/// Отказ на занятой записи — раньше проверки существования на диске: идущую
/// запись или расшифровку нельзя трогать, даже если бы файлы уже как-то
/// пропали.
#[tauri::command]
fn delete_recording(
    folder: Option<String>,
    base: String,
    status: tauri::State<Status>,
    queue: tauri::State<TranscribeQueue>,
    uploads: tauri::State<callabo::Uploads>,
) -> Result<(), String> {
    if recording_busy(&status, &queue, folder.as_deref(), &base) {
        return Err(format!("«{base}» сейчас занята — идёт запись или расшифровка"));
    }
    let dir = recording_dir(&recordings_root(), folder.as_deref(), &base, false)?;
    uploads.while_idle(folder.as_deref(), &base, || delete::delete_recording(&dir, &base))
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

/// `days: None` — никогда не чистить (см. докблок поля в `config.rs`). Только
/// сохраняет выбор: расписание уже крутится своим циклом (`spawn_retention_worker`)
/// и на следующем тике сам перечитает конфиг — второй запуск отсюда не нужен,
/// а был бы вторым источником «когда чистить в следующий раз».
#[tauri::command]
fn set_audio_retention(days: Option<u32>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.audio_retention_days = days;
    cfg.save(&app)
}

#[tauri::command]
fn set_transcribe_config(
    gateway_url: Option<String>,
    api_key: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.stt_gateway_url = gateway_url;
    cfg.stt_api_key = api_key;
    cfg.save(&app)
}

/// `mode: "server" | "local"`. Только сохраняет выбор — тем же принципом,
/// что и `set_theme`: разбор строки в действующий режим решает
/// `transcribe::effective_mode`, а не эта команда.
#[tauri::command]
fn set_transcribe_mode(mode: Option<String>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.transcribe_mode = mode;
    cfg.save(&app)
}

/// Тип сервера расшифровки: `"gateway"` — шлюз selfhost-ai-lab, `"whisper_cpp"`
/// — whisper-server. Только сохраняет выбор: развилка читает конфиг при
/// каждой расшифровке (`run_transcription`), второй источник правды не нужен.
#[tauri::command]
fn set_transcribe_server(kind: Option<String>, app: AppHandle) -> Result<(), String> {
    let mut cfg = Config::load(&app);
    cfg.transcribe_server = kind;
    cfg.save(&app)
}

/// Состояние модели локальной расшифровки — по факту на диске и на диске
/// свободного места, не по памяти между вызовами: окно настроек могли
/// закрыть и открыть заново, скачивание могли прервать снаружи.
#[tauri::command]
fn local_model_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let dir = local::resolve_models_dir(&app)?;
    if local::is_downloaded(&dir) {
        return Ok(serde_json::json!({ "state": "ready", "size": local::MODEL.size_bytes }));
    }
    match local::check_space(&dir) {
        Some(local::SpaceCheck::NotEnough { need, free }) => {
            Ok(serde_json::json!({ "state": "no_space", "need": need, "free": free }))
        }
        _ => Ok(serde_json::json!({ "state": "missing", "size": local::MODEL.size_bytes })),
    }
}

/// Ставит скачивание в фон и возвращается сразу — ход дела приходит
/// событиями `local-model-progress`/`local-model-done`/`local-model-error`
/// (см. `local::download_model`), тем же приёмом, что расшифровка на шлюзе
/// шлёт `transcribe-progress`/`transcribe-done`/`transcribe-error`.
#[tauri::command]
fn local_model_download(app: AppHandle) -> Result<(), String> {
    let dir = local::resolve_models_dir(&app)?;
    tauri::async_runtime::spawn(async move {
        if let Err(e) = local::download_model(&app, &dir).await {
            let _ = app.emit("local-model-error", local::error_payload(&e));
        } else {
            let _ = app.emit("local-model-done", ());
        }
    });
    Ok(())
}

#[tauri::command]
fn local_model_remove(app: AppHandle) -> Result<(), String> {
    let dir = local::resolve_models_dir(&app)?;
    local::remove_model(&dir).map_err(|e| format!("не удалось удалить модель: {e}"))
}

/// Включить/выключить проверку микрофона.
///
/// Отвечает СРАЗУ, не дожидаясь, пока аудио-поток на следующем тике разберёт
/// канал (см. докблок `audio::MonitorEpoch`) — иначе кнопка на секунды
/// зависала бы disabled. Возвращает номер эпохи, который получится ПОСЛЕ
/// применения этой команды: `epoch` читается уже после отправки в канал, а
/// применяет её обработчик `Ctl::Monitor` в `audio::drain_ctl` строго по
/// очереди следом за уже применёнными — поэтому «текущее значение + 1» и есть
/// нижняя граница будущего результата, не завышенная ни при каких раскладах.
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

fn emit_transcribe_progress(app: &AppHandle, folder: &Option<String>, base: &str, stage: &str) {
    let _ = app.emit(
        "transcribe-progress",
        serde_json::json!({ "folder": folder, "base": base, "stage": stage }),
    );
}

fn emit_transcribe_done(app: &AppHandle, folder: &Option<String>, base: &str) {
    let _ = app.emit("transcribe-done", serde_json::json!({ "folder": folder, "base": base }));
}

fn emit_transcribe_error(app: &AppHandle, folder: &Option<String>, base: &str, message: &str) {
    let _ = app.emit(
        "transcribe-error",
        serde_json::json!({ "folder": folder, "base": base, "message": message }),
    );
}

/// Отмена — отдельное событие, а не `transcribe-error`.
///
/// Ошибка и отмена выглядят на экране по-разному и должны выглядеть
/// по-разному: ошибку человек не просил, её показывают красным и предлагают
/// повторить, а отмену он только что нажал сам — извиняться за неё не за что.
fn emit_transcribe_cancelled(app: &AppHandle, folder: &Option<String>, base: &str) {
    let _ = app.emit("transcribe-cancelled", serde_json::json!({ "folder": folder, "base": base }));
}

/// Какая из двух дорожек сейчас в работе — на шлюзе или на whisper-сервере.
///
/// Отдельным событием, а не полем в `stage`: строка стадии уже перегружена
/// форматом `queued:N`, и второй раз этого делать не стоит — разбор в
/// `ui/main.js` пришлось бы усложнять ради того, что к стадии отношения не имеет.
fn emit_transcribe_track(app: &AppHandle, folder: &Option<String>, base: &str, track: &str) {
    let _ = app.emit(
        "transcribe-track",
        serde_json::json!({ "folder": folder, "base": base, "track": track }),
    );
}

/// Ставит запись в очередь и возвращается сразу — саму транскрипцию проводит
/// `spawn_transcribe_worker`. Позиция > 1 значит «уже что-то обрабатывается
/// или ждёт впереди» — шлём её в UI тем же событием `transcribe-progress`,
/// которым `run_transcription` шлёт стадии, чтобы фронтенду не нужен был
/// отдельный тип состояния под «в очереди» и «обрабатывается».
#[tauri::command]
fn transcribe_recording(
    folder: Option<String>,
    base: String,
    app: AppHandle,
    queue: tauri::State<'_, TranscribeQueue>,
) -> Result<(), String> {
    let position = queue.enqueue(QueueItem { folder: folder.clone(), base: base.clone() })?;
    if position > 1 {
        emit_transcribe_progress(&app, &folder, &base, &format!("queued:{position}"));
    }
    Ok(())
}

/// Снять запись с расшифровки: и стоящую в очереди, и идущую прямо сейчас.
///
/// Отсутствие записи в очереди — не ошибка: между тем, как человек открыл
/// меню, и тем, как нажал «Отменить», расшифровка могла спокойно закончиться.
/// Вернуть здесь `Err` значило бы показать красное сообщение о том, что всё в
/// порядке.
#[tauri::command]
fn cancel_transcription(
    folder: Option<String>,
    base: String,
    app: AppHandle,
    queue: tauri::State<'_, TranscribeQueue>,
) -> Result<(), String> {
    let item = QueueItem { folder: folder.clone(), base: base.clone() };
    match queue.cancel(&item)? {
        // Об идущей объявит воркер, когда она действительно остановится:
        // скажи мы это отсюда, «отменено» появилось бы на экране раньше, чем
        // расшифровка перестала писать файлы.
        Cancelled::Stopped(jobs) => {
            // Задача на шлюзе живёт своей жизнью и держит GPU: воркер там
            // работает с concurrency: 1, и пока брошенная задача не погашена,
            // следующая в НАШЕЙ очереди не двинется. Гасим её явно.
            //
            // `jobs` пришли вместе с исходом `cancel()`, а не отдельным
            // вызовом следом: `cancel()` уже разбудил воркер отправкой в
            // `oneshot`, и раздельный второй захват лока мог бы опоздать за
            // `finish_front()`, которая тот же `jobs` чистит.
            if !jobs.is_empty() {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let cfg = Config::load(&app);
                    let (Some(url), Some(key)) = (cfg.stt_gateway_url, cfg.stt_api_key) else {
                        return;
                    };
                    let url = url.trim().trim_end_matches('/').to_string();
                    let Ok(client) = reqwest::Client::builder().build() else {
                        return;
                    };
                    for job in jobs {
                        // Неудача не всплывает на экран: локальная отмена уже
                        // сработала, и красное сообщение о чужой сети поверх
                        // собственного успешного действия только пугает.
                        if let Err(e) = transcribe::cancel_job(&client, &url, key.trim(), &job).await {
                            log::warn!("не удалось погасить задачу {job} на шлюзе: {e}");
                        }
                    }
                });
            }
        }
        Cancelled::Unknown => {}
        Cancelled::Dropped(moved) => {
            emit_transcribe_cancelled(&app, &folder, &base);
            for (position, next) in moved {
                emit_transcribe_progress(&app, &next.folder, &next.base, &format!("queued:{position}"));
            }
        }
    }
    Ok(())
}

/// Значения берутся владением, а не ссылками: расшифровка живёт в `select!`
/// вместе с каналом отмены и не может одалживать ничего у цикла воркера.
async fn run_transcription(folder: Option<String>, base: String, app: AppHandle) -> Result<(), String> {
    let (folder, base, app) = (&folder, base.as_str(), &app);
    let cfg = Config::load(app);
    let mode = transcribe::effective_mode(cfg.transcribe_mode.as_deref(), cfg.transcribe_server.as_deref());

    let dir = match folder {
        Some(f) => recordings_root().join(f),
        None => recordings_root(),
    };
    let prepare_dir = dir.clone();
    let prepare_base = base.to_string();
    let tracks = tokio::task::spawn_blocking(move || {
        recording::PreparedTracks::open(&prepare_dir, &prepare_base)
    })
        .await.map_err(|e| e.to_string())?.map_err(|e| {
            emit_transcribe_error(app, folder, base, &e);
            e
        })?;
    let mic_path = tracks.mic.clone();
    let sys_path = tracks.system.clone();

    let client = match reqwest::Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("не удалось создать HTTP-клиент: {e}");
            emit_transcribe_error(app, folder, base, &msg);
            return Err(msg);
        }
    };

    // Локальный режим идёт тем же путём, что и серверный: обе дорожки, слияние,
    // запись файлов. Разница ровно одна — где считается расшифровка, — и она
    // спрятана в развилке `transcribe::transcribe_track`.
    //
    // Раньше здесь стоял ранний выход: он звал `transcribe_track` один раз и
    // делал `.expect_err`, опираясь на то, что движок не подключён и функция
    // всегда возвращает `local.notWired`. Это ломало обещание из докблока
    // `local::transcribe_local` — «менять код в `main.rs` не придётся, только
    // тело этой функции»: первая же удачная локальная расшифровка роняла бы
    // процесс паникой вместо того, чтобы отдать текст.
    let (url, key) = match параметры_сервера(mode, cfg.stt_gateway_url, cfg.stt_api_key) {
        Ok(pair) => pair,
        Err(msg) => {
            emit_transcribe_error(app, folder, base, msg);
            return Err(msg.to_string());
        }
    };
    // Дорожки ПО ОЧЕРЕДИ, а не через join!.
    //
    // Ускорения параллельность не давала никогда: воркер шлюза работает с
    // concurrency: 1 и всё равно выстраивает задачи друг за другом. Зато
    // клиентские часы у обеих тикали одновременно, и вторая дорожка тратила
    // свой бюджет ожидания, стоя в чужой очереди, — ровно поэтому часовые
    // встречи не доезжали. См. docs/2026-08-27-...-design.md, раздел 2.
    let queue = app.state::<TranscribeQueue>();

    emit_transcribe_track(app, folder, base, "mic");
    let mic_res =
        дорожка_целиком(mode, &client, &url, &key, &mic_path, transcribe::Label::Owner, &*queue, app, folder, base)
            .await;
    emit_transcribe_track(app, folder, base, "system");
    let sys_res =
        дорожка_целиком(mode, &client, &url, &key, &sys_path, transcribe::Label::Others, &*queue, app, folder, base)
            .await;

    let (mic, mic_err) = match mic_res {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let (sys, sys_err) = match sys_res {
        Ok(r) => (Some(r), None),
        Err(e) => (None, Some(e.to_string())),
    };

    if mic.is_none() && sys.is_none() {
        let msg = сообщение_обеих_неудач(
            mic_err.as_deref().unwrap_or("?"),
            sys_err.as_deref().unwrap_or("?"),
        );
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }

    emit_transcribe_progress(app, folder, base, "merging");
    let mic = mic.unwrap_or_default();
    let sys = sys.unwrap_or_default();
    let mut md = transcribe::merge_markdown(&mic, &sys);
    // Частичный отказ — не теряем то, что получилось, но явно помечаем,
    // какая дорожка не удалась (см. Global Constraints и дизайн).
    if let Some(e) = &mic_err {
        md = format!("_Дорожка владельца не транскрибирована: {e}_\n\n{md}");
    }
    if let Some(e) = &sys_err {
        md = format!("_Дорожка собеседников не транскрибирована: {e}_\n\n{md}");
    }
    let mut txt = transcribe::merge_plain(&mic, &sys);
    if let Some(e) = &mic_err {
        txt = format!("[Дорожка владельца не транскрибирована: {e}]\n\n{txt}");
    }
    if let Some(e) = &sys_err {
        txt = format!("[Дорожка собеседников не транскрибирована: {e}]\n\n{txt}");
    }

    let out_dir = dir.join(format!("{base}.transcript"));
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }
    if let Err(e) = std::fs::write(out_dir.join(format!("{base}.md")), &md) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }
    if let Err(e) = std::fs::write(out_dir.join(format!("{base}.txt")), &txt) {
        let msg = e.to_string();
        emit_transcribe_error(app, folder, base, &msg);
        return Err(msg);
    }

    emit_transcribe_done(app, folder, base);
    Ok(())
}

/// Адрес и ключ для выбранного режима, уже вычищенные.
///
/// `Gateway` требует и адрес, и ключ. `WhisperCpp` — только адрес: ключа у
/// whisper-server нет, введённый по привычке игнорируется, а не уезжает в
/// запрос. `Local` не ходит никуда — пустые строки, которые дальше по коду
/// никто не читает. Хвостовой `/` срезается здесь, потому что путь
/// (`/v1/...` у шлюза, `/inference` у whisper-server) дописывает клиент.
fn параметры_сервера(
    mode: transcribe::Mode,
    url: Option<String>,
    key: Option<String>,
) -> Result<(String, String), &'static str> {
    let чистый_адрес = url
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty());
    let чистый_ключ = key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
    match mode {
        transcribe::Mode::Local => Ok((String::new(), String::new())),
        transcribe::Mode::WhisperCpp => match чистый_адрес {
            Some(u) => Ok((u, String::new())),
            None => Err("настройте адрес whisper-сервера"),
        },
        transcribe::Mode::Gateway => match (чистый_адрес, чистый_ключ) {
            (Some(u), Some(k)) => Ok((u, k)),
            _ => Err("настройте URL и ключ шлюза"),
        },
    }
}

/// Что показать, когда не удалась ни одна дорожка.
///
/// Одинаковые половины склеивать нельзя: в локальном режиме без подключённого
/// движка обе дорожки возвращают один и тот же ключ словаря
/// (`local.notWired`), и интерфейс переводит его как есть — см. докблок
/// `local::LocalError`. Склейка «обе дорожки не удались — мик: …; система: …»
/// ключом уже не является и до перевода не доживёт: на экране оказалась бы
/// сырая строка вместо фразы.
fn сообщение_обеих_неудач(mic_err: &str, sys_err: &str) -> String {
    if mic_err == sys_err {
        return mic_err.to_string();
    }
    format!("обе дорожки не удались — мик: {mic_err}; система: {sys_err}")
}

/// Одна дорожка целиком: сообщить об отправке, отправить, запомнить id для
/// отмены, сообщить об ожидании, дождаться.
///
/// Обе стадии эмитятся ЗДЕСЬ, за дорожку, а не разом в `run_transcription` до
/// начала всей работы. Раньше `uploading` ставился ДО построения клиента и ДО
/// `submit()` у первой дорожки, а `polling` — сразу следом, тоже до реальной
/// отправки: разница между ними жила на экране доли секунды (время собрать
/// `reqwest::Client`), а всё время настоящей заливки — до 115 МБ дорожки —
/// шло уже под меткой «Расшифровываю…», и «Отправляю…» не было видно вовсе.
/// Здесь `uploading` стоит перед `submit()`, `polling` — после `note_job()`,
/// и оба раза за дорожку (mic, потом system), а не один раз за всю запись.
///
/// id кладётся в очередь ДО ожидания — иначе отмена, нажатая в первую же
/// минуту, не нашла бы что гасить на шлюзе.
async fn дорожка_целиком(
    mode: transcribe::Mode,
    client: &reqwest::Client,
    url: &str,
    key: &str,
    path: &Path,
    label: transcribe::Label,
    queue: &TranscribeQueue,
    app: &AppHandle,
    folder: &Option<String>,
    base: &str,
) -> Result<transcribe::TrackResult, transcribe::TranscribeError> {
    match mode {
        // «Отправляю…» здесь было бы враньём: локальный движок никуда не
        // шлёт, а whisper-server на localhost принимает файл за секунду и
        // дальше считает — суть происходящего «Расшифровываю…».
        transcribe::Mode::Local | transcribe::Mode::WhisperCpp => {
            emit_transcribe_progress(app, folder, base, "polling")
        }
        transcribe::Mode::Gateway => emit_transcribe_progress(app, folder, base, "uploading"),
    }
    transcribe::transcribe_track(mode, client, url, key, path, label, |job_id| {
        queue.note_job(job_id);
        emit_transcribe_progress(app, folder, base, "polling");
    })
    .await
}

fn main() {
    let (tx, rx) = channel::<Ctl>();
    let tray_tx = tx.clone();
    // Один счётчик на весь процесс: команда и аудио-поток обязаны видеть одно
    // и то же число, иначе эпоха ничего не различает. См. докблок
    // `audio::MonitorEpoch`.
    let monitor_epoch: audio::MonitorEpoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let monitor_epoch_audio = monitor_epoch.clone();
    let hotkey_tx = tx.clone();
    let (transcribe_queue, transcribe_rx) = TranscribeQueue::new();

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
        .manage(transcribe_queue)
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
            set_mic_device,
            set_language,
            set_theme,
            set_transcribe_config,
            set_transcribe_mode,
            set_transcribe_server,
            local_model_status,
            local_model_download,
            local_model_remove,
            set_monitor,
            rename_recording,
            delete_recording,
            set_audio_retention,
            transcribe_recording,
            cancel_transcription,
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
            app.global_shortcut().on_shortcut(shortcut, move |app, _sc, event| {
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

            spawn_transcribe_worker(app.handle().clone(), transcribe_rx);
            spawn_retention_worker(app.handle().clone());
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
            if let tauri::RunEvent::Reopen { has_visible_windows, .. } = event {
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

    /// Запись без расшифровки и нулевой длительности: размеры в этих тестах
    /// исчисляются десятками байт, то есть меньше секунды звука.
    fn rec(name: &str, folder: Option<&str>, mic: bool, system: bool, size: u64) -> Recording {
        Recording {
            name: name.to_string(),
            folder: folder.map(str::to_string),
            mic,
            system,
            size,
            transcript: false,
            duration_sec: 0,
            quiet_mic_db: None,
            recording_now: false,
            callabo_workspaces: vec![],
        }
    }

    fn group(files: &[(Option<&str>, &str, u64)]) -> Vec<Recording> {
        group_with(files, &[])
    }

    /// То же, что `group`, но с найденными рядом папками `<основа>.transcript`
    /// — ключом `(основа, папка)`, каким их отдаёт `collect_files`.
    fn group_with(
        files: &[(Option<&str>, &str, u64)],
        transcripts: &[(&str, Option<&str>)],
    ) -> Vec<Recording> {
        group_full(files, transcripts, &[])
    }

    /// Полная форма стенда: файлы, расшифровки и длительности из
    /// файлов-спутников — тем же ключом `(основа, папка)`, каким их
    /// собирает `collect_files` в `found.durations`.
    fn group_full(
        files: &[(Option<&str>, &str, u64)],
        transcripts: &[(&str, Option<&str>)],
        durations: &[((&str, Option<&str>), u32)],
    ) -> Vec<Recording> {
        let transcripts: HashSet<(String, Option<String>)> = transcripts
            .iter()
            .map(|(b, f)| (b.to_string(), f.map(str::to_string)))
            .collect();
        let durations: HashMap<(String, Option<String>), u32> = durations
            .iter()
            .map(|((b, f), d)| ((b.to_string(), f.map(str::to_string)), *d))
            .collect();
        group_recordings(
            files
                .iter()
                .map(|(f, n, s)| (f.map(str::to_string), n.to_string(), *s)),
            &transcripts,
            &durations,
        )
    }

    /// Дорожка длиной ровно `sec` секунд: заголовок плюс отсчёты.
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

    // ---- расшифровка ---------------------------------------------------------

    /// Гвоздь задачи: раньше обход каталога пропускал всё, что не файл, а
    /// расшифровка лежит именно папкой — окно предлагало расшифровать заново
    /// уже расшифрованную запись, и так каждый раз.
    #[test]
    fn папка_расшифровки_рядом_помечает_запись() {
        let files = [
            (None, "2026-07-17_14-45_zoom.mic.wav", 100),
            (None, "2026-07-17_14-45_zoom.system.wav", 20),
        ];
        assert!(
            !group_with(&files, &[])[0].transcript,
            "без папки рядом расшифровки нет"
        );
        assert!(
            group_with(&files, &[("2026-07-17_14-45_zoom", None)])[0].transcript,
            "папка 2026-07-17_14-45_zoom.transcript рядом с дорожками и есть признак расшифровки"
        );
    }

    /// Ключ пометки — тот же `(основа, папка)`, что и у группировки. Одна
    /// основа может лежать в двух папках сразу (см.
    /// `одна_основа_в_двух_папках_даёт_две_неполные_записи_а_не_одну_целую`), и
    /// расшифровка корневой половины не имеет отношения к половине в `2026-07`.
    #[test]
    fn расшифровка_из_другой_папки_не_приписывается_записи() {
        let list = group_with(
            &[
                (Some("2026-07"), "2026-07-30_13-03_chrome.mic.wav", 10),
                (None, "2026-07-30_13-03_chrome.system.wav", 20),
            ],
            &[("2026-07-30_13-03_chrome", None)],
        );
        assert_eq!(
            list.iter().map(|r| r.transcript).collect::<Vec<_>>(),
            vec![false, true],
            "помечена обязана быть корневая запись, а не тёзка из месячной папки"
        );
    }

    /// Папка расшифровки без единой дорожки рядом — это ровно то состояние,
    /// в которое запись приводит автоочистка старого аудио (см.
    /// `retention.rs`): звук в корзине, расшифровка на месте. Список обязан
    /// показать такую запись, а не проглотить её молча, — иначе от встречи
    /// не осталось бы и следа в интерфейсе, хотя расшифровка жива на диске.
    #[test]
    fn одинокая_папка_расшифровки_это_запись_с_вычищенным_звуком() {
        let list = group_with(&[], &[("2026-07-17_14-45_zoom", None)]);
        let mut ожидание = rec("2026-07-17_14-45_zoom", None, false, false, 0);
        ожидание.transcript = true;
        assert_eq!(list, vec![ожидание]);
    }

    /// Единственный тест здесь, которому нужен настоящий диск: остальное про
    /// расшифровку — чистая логика, а вот «обход видит папку, а не только
    /// файлы» проверяется только обходом. Раньше `collect_files` отбрасывал всё,
    /// что не файл, и никакая правка группировки этого бы не исправила.
    #[test]
    fn обход_каталога_находит_папки_расшифровок_и_в_корне_и_в_месяце() {
        let root = std::env::temp_dir().join(format!("mr-collect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("2026-06-01_10-00_zoom.transcript")).unwrap();
        std::fs::create_dir_all(root.join("2026-07/2026-07-30_13-03_chrome.transcript")).unwrap();
        std::fs::create_dir_all(root.join("архив")).unwrap();
        std::fs::write(root.join("2026-06-01_10-00_zoom.mic.wav"), b"x").unwrap();

        let found = collect_files(&root).unwrap();
        let mut got: Vec<_> = found.transcripts.iter().cloned().collect();
        got.sort();
        assert_eq!(
            got,
            vec![
                ("2026-06-01_10-00_zoom".to_string(), None),
                (
                    "2026-07-30_13-03_chrome".to_string(),
                    Some("2026-07".to_string())
                ),
            ],
            "папка месяца и посторонний каталог расшифровками не считаются"
        );
        assert_eq!(
            found.files,
            vec![(None, "2026-06-01_10-00_zoom.mic.wav".to_string(), 1)],
            "дорожки собираются как и раньше, внутрь .transcript обход не заходит"
        );
        let _ = std::fs::remove_dir_all(&root);
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
        assert_eq!(duration_sec(wav_bytes(0)), 0, "один заголовок — ноль секунд");
        assert_eq!(duration_sec(10), 0, "обрезанный файл не уходит в минус");
        assert_eq!(duration_sec(0), 0);
    }

    // ---- meta.json: длительность переживает автоочистку -----------------

    /// Гвоздь задачи: файл-спутник знает точную длительность и обязан
    /// побеждать оценку по размеру `.wav`, даже когда сам `.wav` цел и
    /// расчёт по нему тоже возможен.
    #[test]
    fn meta_json_побеждает_расчёт_по_размеру_даже_когда_wav_на_месте() {
        let list = group_full(
            &[(None, "2026-07-17_14-45_zoom.mic.wav", wav_bytes(2400))],
            &[],
            &[(("2026-07-17_14-45_zoom", None), 3120)],
        );
        assert_eq!(
            list[0].duration_sec, 3120,
            "meta.json важнее расчёта по размеру, а не наоборот"
        );
    }

    /// Ровно тот сценарий, ради которого задача и делалась: автоочистка
    /// забрала `.wav`, расшифровка (и с ней файл-спутник) осталась —
    /// длительность обязана остаться видимой, а не превратиться в ноль.
    #[test]
    fn meta_json_переживший_чистку_даёт_длительность_без_wav() {
        let list = group_full(
            &[],
            &[("2026-07-17_14-45_zoom", None)],
            &[(("2026-07-17_14-45_zoom", None), 3120)],
        );
        assert_eq!(list.len(), 1);
        assert!(!list[0].mic && !list[0].system, "дорожек уже нет");
        assert_eq!(
            list[0].duration_sec, 3120,
            "длительность из meta.json обязана пережить чистку .wav"
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

    fn qi(base: &str) -> QueueItem {
        QueueItem { folder: None, base: base.to_string() }
    }

    #[test]
    fn первая_запись_в_очереди_получает_позицию_1() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(rx.try_recv(), Ok(qi("a")), "воркер обязан получить её немедленно");
    }

    /// Гвоздь задачи: вторая запись не отвергается ошибкой, как было раньше
    /// («уже идёт транскрипция другой записи»), а встаёт следующей.
    #[test]
    fn вторая_запись_пока_первая_обрабатывается_встаёт_второй_а_не_отвергается() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(q.enqueue(qi("b")), Ok(2));
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "обе записи обязаны дойти до воркера по порядку");
    }

    /// Двойной клик по кнопке — реальный сценарий, а не гипотетический: между
    /// кликом и первым событием `transcribe-progress` кнопка ещё активна.
    /// Без дедупликации в очередь ушли бы два одинаковых задания.
    #[test]
    fn повторный_enqueue_той_же_записи_не_дублирует_и_отдаёт_ту_же_позицию() {
        let (q, mut rx) = TranscribeQueue::new();
        assert_eq!(q.enqueue(qi("a")), Ok(1));
        assert_eq!(q.enqueue(qi("b")), Ok(2));
        assert_eq!(
            q.enqueue(qi("a")),
            Ok(1),
            "повторная постановка уже стоящей в очереди записи не создаёт вторую копию"
        );
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")));
        assert!(
            rx.try_recv().is_err(),
            "третьего сообщения в канале быть не должно — дубликат не отправлялся"
        );
    }

    #[test]
    fn finish_front_убирает_обработанную_запись_и_отдаёт_остальных_по_порядку() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.enqueue(qi("c")).unwrap();
        assert_eq!(q.finish_front(), vec![qi("b"), qi("c")]);
    }

    #[test]
    fn finish_front_на_последней_записи_отдаёт_пустую_очередь() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        assert_eq!(q.finish_front(), Vec::<QueueItem>::new());
    }


    // ---- отмена расшифровки -------------------------------------------------

    /// Между тем, как открылось меню «⋯», и тем, как нажали «Отменить»,
    /// расшифровка успевает закончиться. Это обычный ход событий, а не сбой.
    #[test]
    fn отмена_записи_которой_в_очереди_нет_ничего_не_меняет() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Unknown));
        assert_eq!(q.finish_front(), Vec::<QueueItem>::new(), "очередь не тронута");
    }

    /// Позиции пересчитываются той же арифметикой, что и после `finish_front`:
    /// снялась вторая — третья становится второй.
    #[test]
    fn снятая_из_середины_запись_освобождает_позицию_следующим() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.enqueue(qi("c")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Dropped(vec![(2, qi("c"))])));
        assert_eq!(q.finish_front(), vec![qi("c")]);
    }

    /// Идущей записи новая позиция не сообщается: у неё на экране стадия
    /// («отправка», «расшифровка»), и `queued:1` поверх стадии читался бы как
    /// откат назад.
    #[test]
    fn идущей_записи_позиция_не_пересылается() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("b")), Ok(Cancelled::Dropped(vec![])));
    }

    #[test]
    fn идущая_запись_не_вычёркивается_из_очереди_а_получает_сигнал_остановиться() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        let mut отмена = q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped(vec![])));
        assert!(отмена.try_recv().is_ok(), "сигнал обязан дойти до расшифровки");
        assert_eq!(
            q.finish_front(),
            vec![qi("b")],
            "с фронта снимается именно отменённая запись, а не следующая"
        );
    }

    /// Послать в `oneshot` можно единожды, поэтому после первой отмены канал
    /// пуст. Без отдельного флага «уже в работе» второе нажатие приняло бы
    /// идущую запись за ещё не начатую, вычеркнуло бы её из очереди — и
    /// `finish_front` снял бы с фронта следующую, ни разу не начатую.
    #[test]
    fn повторная_отмена_идущей_записи_не_съедает_следующую() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped(vec![])));
        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Stopped(vec![])), "повтор — не ошибка");
        assert_eq!(q.finish_front(), vec![qi("b")]);
    }

    /// Окно между `recv` воркера и началом работы: запись уже первая в
    /// очереди, но ещё не пошла. Отменить её здесь — значит просто вычеркнуть,
    /// а не слать сигнал в никуда.
    #[test]
    fn ещё_не_начатый_фронт_отменяется_вычёркиванием() {
        let (q, _rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();

        assert_eq!(q.cancel(&qi("a")), Ok(Cancelled::Dropped(vec![])));
        assert!(
            q.start_front(&qi("a")).is_none(),
            "воркер не имеет права начать отменённую запись"
        );
    }

    /// Забрать запись из середины `tokio::mpsc` нельзя, поэтому в канале после
    /// отмены остаётся мёртвая копия. Ловит её `start_front`, сверяясь с
    /// фронтом очереди.
    #[test]
    fn мёртвая_копия_из_канала_воркеру_работать_не_даёт() {
        let (q, mut rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");
        q.cancel(&qi("b")).unwrap();
        q.finish_front();

        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "канал про отмену не знает");
        assert!(q.start_front(&qi("b")).is_none(), "но работать по ней нельзя");
    }

    /// Отменили не глядя, спохватились, поставили заново — запись обязана
    /// пойти в работу, несмотря на мёртвую копию, которая всё ещё лежит в
    /// канале впереди новой.
    #[test]
    fn снятую_запись_можно_поставить_заново() {
        let (q, mut rx) = TranscribeQueue::new();
        q.enqueue(qi("a")).unwrap();
        q.enqueue(qi("b")).unwrap();
        q.start_front(&qi("a")).expect("первая пошла в работу");
        q.cancel(&qi("b")).unwrap();

        assert_eq!(q.enqueue(qi("b")), Ok(2), "встала заново, за идущей");
        q.finish_front();
        assert_eq!(rx.try_recv(), Ok(qi("a")));
        assert_eq!(rx.try_recv(), Ok(qi("b")), "мёртвая копия");
        assert!(q.start_front(&qi("b")).is_some(), "живая постановка — можно работать");
        assert_eq!(rx.try_recv(), Ok(qi("b")), "а это уже сама постановка");
    }

    /// Чтобы отменить задачу на шлюзе, надо знать её id. Он появляется только
    /// после отправки, поэтому очередь обязана уметь его принять на ходу —
    /// и вернуть вместе с исходом `cancel()`, когда запись действительно
    /// идущая (см. `Cancelled::Stopped`).
    #[test]
    fn идущая_запись_запоминает_id_задач_шлюза() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");

        queue.note_job("job_mic".to_string());
        queue.note_job("job_sys".to_string());

        assert_eq!(
            queue.cancel(&item),
            Ok(Cancelled::Stopped(vec!["job_mic".to_string(), "job_sys".to_string()])),
        );
    }

    /// `Cancelled::Stopped` несёт id именно ЗАБРАННЫМИ: второй вызов
    /// `cancel()` не имеет права отдать те же id снова, иначе повторная
    /// отмена (см. `повторная_отмена_идущей_записи_не_съедает_следующую`)
    /// била бы по чужой, уже следующей задаче.
    #[test]
    fn забранные_id_второй_раз_не_отдаются() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");
        queue.note_job("job_mic".to_string());

        assert_eq!(queue.cancel(&item), Ok(Cancelled::Stopped(vec!["job_mic".to_string()])));
        assert_eq!(queue.cancel(&item), Ok(Cancelled::Stopped(vec![])), "id одноразовые");
    }

    /// Следующая запись начинает с чистого листа: id предыдущей к ней
    /// отношения не имеют. Проверяется не напрямую (отдельного геттера для
    /// `jobs` больше нет — только `cancel()` их когда-либо отдаёт), а через
    /// отмену уже второй записи: не появись в ней чужой id, `finish_front`
    /// свою работу сделала.
    #[test]
    fn финиш_фронта_забывает_id_задач() {
        let (queue, _rx) = TranscribeQueue::new();
        let первая = qi("первая");
        queue.enqueue(первая.clone()).expect("постановка");
        queue.start_front(&первая).expect("старт");
        queue.note_job("job_old".to_string());

        queue.finish_front();

        let вторая = qi("вторая");
        queue.enqueue(вторая.clone()).expect("постановка");
        queue.start_front(&вторая).expect("старт");

        assert_eq!(
            queue.cancel(&вторая),
            Ok(Cancelled::Stopped(vec![])),
            "id не переезжают на следующую запись"
        );
    }

    // ---- recording_busy: общий стоп-кран для удаления и автоочистки --------

    /// Свободная запись — не занята ничем: не идёт, не в очереди на
    /// расшифровку.
    #[test]
    fn recording_busy_свободная_запись_не_занята() {
        let status = Status::default();
        let (queue, _rx) = TranscribeQueue::new();
        assert!(!recording_busy(&status, &queue, None, "2026-07-17_14-30_zoom"));
    }

    /// Запись, которая пишется прямо сейчас, занята — и ровно она, а не
    /// любая другая: `folder`/`base` сверяются оба, не только состояние.
    #[test]
    fn recording_busy_занята_пока_идёт_запись() {
        let status = Status::default();
        status.set_current_recording(Some(("2026-07".to_string(), "2026-07-17_14-30_zoom".to_string())));
        let (queue, _rx) = TranscribeQueue::new();

        assert!(recording_busy(&status, &queue, Some("2026-07"), "2026-07-17_14-30_zoom"));
        assert!(
            !recording_busy(&status, &queue, Some("2026-07"), "другая-запись"),
            "идёт другая запись — эта свободна"
        );
    }

    /// Запись в очереди на расшифровку (ждёт или уже обрабатывается) занята
    /// так же, как идущая: `TranscribeQueue::contains` не различает эти два
    /// случая намеренно — см. её докблок.
    #[test]
    fn recording_busy_занята_пока_расшифровывается_или_ждёт_очереди() {
        let status = Status::default();
        let (queue, _rx) = TranscribeQueue::new();
        queue.enqueue(qi("2026-07-17_14-30_zoom")).expect("постановка");

        assert!(recording_busy(&status, &queue, None, "2026-07-17_14-30_zoom"));
        assert!(!recording_busy(&status, &queue, None, "другая-запись"));
    }

    /// Локальная отмена обязана сработать, даже если шлюз недоступен: человек
    /// нажал кнопку, и кнопка не имеет права зависнуть от чужой сети. Неудача
    /// уходит в лог, а не на экран.
    ///
    /// Id идут ВНУТРИ исхода `cancel()`, не отдельным вызовом следом за ним:
    /// `tx.send(())` внутри `cancel()` будит воркер немедленно, и тот успевает
    /// дойти до `finish_front()` (которая чистит `jobs`) раньше, чем снаружи
    /// возьмут лок ещё раз отдельным вызовом. Раздельный вызов — гонка,
    /// которая молча теряет id и оставляет задачу висеть на шлюзе.
    #[test]
    fn отмена_идущей_забирает_id_для_гашения_на_шлюзе() {
        let (queue, _rx) = TranscribeQueue::new();
        let item = qi("встреча");
        queue.enqueue(item.clone()).expect("постановка");
        queue.start_front(&item).expect("старт");
        queue.note_job("job_mic".to_string());

        assert_eq!(
            queue.cancel(&item),
            Ok(Cancelled::Stopped(vec!["job_mic".to_string()])),
            "id обязаны прийти вместе с исходом — иначе гасить на шлюзе будет нечего"
        );
    }

    // ---- путь к папке записи ------------------------------------------------

    fn путь(folder: Option<&str>, base: &str, transcript: bool) -> Result<PathBuf, String> {
        recording_dir(Path::new("/записи"), folder, base, transcript)
    }

    #[test]
    fn запись_из_корня_показывается_самим_корнем() {
        assert_eq!(путь(None, "2026-07-17_14-30_zoom", false), Ok(PathBuf::from("/записи")));
    }

    #[test]
    fn запись_из_месячной_папки_показывается_этой_папкой() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_zoom", false),
            Ok(PathBuf::from("/записи/2026-07"))
        );
    }

    #[test]
    fn расшифровка_лежит_подпапкой_рядом_с_дорожками() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_zoom", true),
            Ok(PathBuf::from("/записи/2026-07/2026-07-17_14-30_zoom.transcript"))
        );
    }

    #[test]
    fn расшифровка_записи_из_корня_лежит_в_корне() {
        assert_eq!(
            путь(None, "2026-07-17_14-30_zoom", true),
            Ok(PathBuf::from("/записи/2026-07-17_14-30_zoom.transcript"))
        );
    }

    /// Из окна приходит имя папки, а не путь. Всё, что не `YYYY-MM`, — не наша
    /// подпапка: `collect_files` в другие и не заходит.
    #[test]
    fn папкой_может_быть_только_месячная() {
        for чужое in ["..", ".", "/", "2026-7", "2026-07/..", "../2026-07", "чужое"] {
            assert!(
                путь(Some(чужое), "2026-07-17_14-30_zoom", false).is_err(),
                "«{чужое}» не месячная папка и открываться не должна"
            );
        }
    }

    /// Основу человек меняет сам — и «Переименовать», и руками в Finder, где
    /// правил нет вообще. Уйти по ней вверх из каталога записей нельзя.
    #[test]
    fn основа_имени_не_выводит_за_каталог_записей() {
        for чужое in ["..", ".", "", "../секреты", "a/../../b", "/etc/passwd", "запись/"] {
            assert!(
                путь(Some("2026-07"), чужое, true).is_err(),
                "«{чужое}» не имя записи и открываться не должно"
            );
        }
    }

    /// Проверка основы не зависит от того, в подпапку идём или нет: правило
    /// «всё, что пришло из окна, проверено» не должно держаться на ветке.
    #[test]
    fn чужая_основа_отвергается_и_без_расшифровки() {
        assert!(путь(Some("2026-07"), "../секреты", false).is_err());
    }

    /// Кириллица, точки и пробелы внутри имени — обычное дело после
    /// переименования; отвергать их незачем.
    #[test]
    fn обычное_переименованное_имя_проходит() {
        assert_eq!(
            путь(Some("2026-07"), "2026-07-17_14-30_созвон с артёмом v1.2", true),
            Ok(PathBuf::from(
                "/записи/2026-07/2026-07-17_14-30_созвон с артёмом v1.2.transcript"
            ))
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

    // ---- локальный режим и отказ обеих дорожек -------------------------------

    /// Локальный режим считает на этой же машине и на шлюз не ходит, поэтому
    /// требовать его адрес и ключ нельзя.
    #[test]
    fn локальный_режим_не_требует_параметров_сервера() {
        assert_eq!(
            параметры_сервера(transcribe::Mode::Local, None, None),
            Ok((String::new(), String::new()))
        );
    }

    /// Шлюз без настроек — по-прежнему отказ, а не пустые строки: иначе
    /// запрос уйдёт в никуда и человек увидит сетевую ошибку вместо понятного
    /// «настройте шлюз».
    #[test]
    fn шлюз_без_настроек_отказывает() {
        assert!(параметры_сервера(transcribe::Mode::Gateway, None, None).is_err());
        assert!(параметры_сервера(
            transcribe::Mode::Gateway,
            Some("   ".to_string()),
            Some("k".to_string())
        )
        .is_err());
        assert!(параметры_сервера(
            transcribe::Mode::Gateway,
            Some("http://localhost:8080".to_string()),
            None
        )
        .is_err());
    }

    /// whisper-server ключа не имеет: нужен только адрес, введённый ключ
    /// игнорируется, а не уходит в запрос.
    #[test]
    fn whisper_cpp_требует_только_адрес() {
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::WhisperCpp,
                Some("http://127.0.0.1:8178/".to_string()),
                None
            ),
            Ok(("http://127.0.0.1:8178".to_string(), String::new()))
        );
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::WhisperCpp,
                Some("http://127.0.0.1:8178".to_string()),
                Some("лишний".to_string())
            ),
            Ok(("http://127.0.0.1:8178".to_string(), String::new()))
        );
        let err = параметры_сервера(transcribe::Mode::WhisperCpp, None, None).unwrap_err();
        assert!(err.contains("whisper"), "{err}");
    }

    /// Хвостовой слеш и пробелы срезаются здесь, а не у места вызова: путь
    /// (`/v1/...` или `/inference`) дописывает клиент, и `.../` дал бы двойной слеш.
    #[test]
    fn шлюз_чистит_пробелы_и_хвостовой_слеш() {
        assert_eq!(
            параметры_сервера(
                transcribe::Mode::Gateway,
                Some("  http://localhost:8080/  ".to_string()),
                Some("  секрет  ".to_string())
            ),
            Ok(("http://localhost:8080".to_string(), "секрет".to_string()))
        );
    }

    /// Когда обе дорожки упали одинаково, человеку уходит один ключ, а не
    /// склейка из двух одинаковых половин: в локальном режиме без движка обе
    /// возвращают ровно `local.notWired`, и интерфейс переводит его как есть
    /// (докблок `local::LocalError`). Склейка ключом уже не является и до
    /// перевода не доживёт — на экране оказалась бы сырая строка.
    #[test]
    fn одинаковый_отказ_обеих_дорожек_доезжает_одним_ключом() {
        assert_eq!(
            сообщение_обеих_неудач("local.notWired", "local.notWired"),
            "local.notWired"
        );
    }

    /// Разные причины не схлопываются: видны обе, иначе непонятно, что чинить.
    #[test]
    fn разные_отказы_дорожек_показываются_обе() {
        let msg = сообщение_обеих_неудач("сеть отвалилась", "шлюз ответил 413");
        assert!(msg.contains("сеть отвалилась"), "{msg}");
        assert!(msg.contains("шлюз ответил 413"), "{msg}");
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
