//! Разделяемая истина о состоянии: единственное место, где UI может СПРОСИТЬ, а
//! не только УСЛЫШАТЬ.
//!
//! Одного `emit` для этого мало, и не по недосмотру. Аудио-поток поднимается в
//! `setup()`, то есть ДО того, как webview выполнит `listen("fatal")`. Tauri не
//! буферизует события для поздних подписчиков — значит, `emit` из ранней ошибки
//! уходит в никуда, а в релизе (`windows_subsystem = "windows"`) от неё не
//! остаётся даже `eprintln`. Итог, который это чинит: живой GUI, серый трей,
//! «Ожидание встречи» — и ни одной записи.
//!
//! Поэтому истина живёт здесь, а `emit` — только уведомление о её изменении. Кто
//! опоздал на событие, тот спросит `get_state` (см. `main.rs`) и получит ровно
//! то же самое. Гонка «подписался позже, чем случилось» закрывается не тем, что
//! мы угадали момент, а тем, что момент перестал иметь значение.
//!
//! Это же чинит перезагрузку webview посреди записи: страница поднимается с
//! настоящим состоянием, а не с захардкоженным «Ожидание встречи» из `index.html`.

use crate::audio::UiState;
use crate::i18n;
use crate::tray;
use serde::Serialize;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

/// Что случилось, когда команда не доехала до аудио-потока.
///
/// Один текст на все места, потому что случай один: канал закрыт (поток вышел)
/// или мьютекс отравлен (поток паниковал, держа его). И то и другое — навсегда:
/// поднимать аудио-поток заново некому, всё `!Send` умерло вместе с ним.
///
/// Значение — ключ словаря (`ui/i18n/strings.json`), а не готовая фраза:
/// строка уходит и в `Snapshot.fatal`/событие `fatal` (там её переводит сам
/// webview через `window.i18n.t`), и в нативную подсказку трея (там её
/// переводит `i18n::t` здесь, в Rust, — см. `tray::repaint`). Была бы здесь
/// готовая фраза на языке, выбранном при старте, — смена языка в интерфейсе
/// на лету эту конкретную строку бы не тронула.
pub const DEAD: &str = "permission.streamDead";

/// Всё, что UI должен знать о нас в любой момент времени.
#[derive(Serialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct Snapshot {
    pub state: UiState,
    /// `Some` — записи не будет до перезапуска. Не «ошибка на этом тике»
    /// (`emit("error")`), а «дальше ничего не работает».
    pub fatal: Option<String>,
    /// `Some(идентификатор эндпоинта)` — просили этот микрофон, не нашли, пишем
    /// в системный дефолт. Не фатально (запись идёт), но молчать нельзя.
    ///
    /// Не имя: аудио-поток, который это пишет, имени не знает (см.
    /// `audio.rs::sync`). Человеческое имя подставляет тот, у кого есть
    /// конфиг, — тост в `audio.rs` и баннер в `ui/main.js`, оба через
    /// `Config::load`.
    pub device_warning: Option<String>,
    /// `true` — устройство сменили под идущей записью, и она продолжает
    /// писаться прежним. Не предупреждение и не ошибка: приложение делает
    /// ровно то, что должно, — но без этой строки смена выглядит применённой,
    /// хотя применится только к следующей записи.
    pub mic_deferred: bool,
    /// `Some(причина)` — тап не поднялся, приложение работает в режиме одного
    /// микрофона. Не `fatal`: запись есть, просто половинная.
    ///
    /// Держится до конца процесса и снятию не подлежит — в отличие от
    /// `device_warning`, которое снимается возвратом устройства. Тап на macOS
    /// поднимается ровно один раз, при старте (см. `capture::SystemTap`), так
    /// что разрешение, выданное после отказа, доедет только до следующего
    /// запуска. Баннер, который умеет гаснуть сам, здесь врал бы: он погас бы,
    /// а вторая дорожка так и не появилась бы.
    pub no_system_audio: Option<String>,
    /// `Some` — прямо сейчас на диск пишется ровно эта пара дорожек.
    ///
    /// Не событие, а факт снимка, по той же причине, что и весь остальной
    /// модуль: `list_recordings` (см. `main.rs`) вызывается по требованию, а
    /// не подписан ни на что, и обязан на каждый свой вызов узнавать, какая
    /// из найденных на диске записей ещё растёт, — иначе `delete_recording`
    /// мог бы тронуть файл посреди записи.
    pub current_recording: Option<CurrentRecording>,
}

/// Какая запись пишется прямо сейчас.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct CurrentRecording {
    /// Месячная папка (`"2026-07"`).
    pub folder: String,
    /// Основа имени, без суффикса дорожки и расширения.
    pub base: String,
}

/// Что человек должен понять про режим одного микрофона.
///
/// Главное здесь — не «нет второй дорожки», а «автоопределение мертво»: на
/// macOS звонок отличается от просто открытого Zoom ровно тем, что система при
/// нём звучит (см. `audio::mac_should_arm`), и без тапа этого сигнала нет
/// вовсе. Умолчи об этом — и человек неделю будет думать, что сломался детект,
/// а не что он сам нажал «Не разрешать».
/// Ключ словаря, не готовая фраза — используется только в теле нативного
/// уведомления (`no_system_audio` ниже), которое рисует сам Rust через
/// `i18n::t`. В webview этот же смысл идёт отдельными ключами
/// `permission.noSystemAudioShort`/`permission.noSystemAudio` напрямую из
/// `ui/main.js`, минуя этот модуль, — см. `показать_отсутствие_системного_звука`.
pub const NO_SYSTEM_AUDIO: &str = "permission.noSystemAudio";

/// Разделяемое состояние. Живёт в `tauri::State`, пишется аудио-потоком (и теми,
/// у кого не прошёл `send`), читается командой `get_state` из webview.
#[derive(Default)]
pub struct Status(Mutex<Snapshot>);

impl Status {
    /// Отравленный мьютекс здесь не повод терять состояние: `Snapshot` — это два
    /// независимых поля, порванной середины у него не бывает. Паниковать в ответ
    /// на чужую панику значило бы менять «GUI без записи» на «GUI без GUI».
    fn lock(&self) -> std::sync::MutexGuard<'_, Snapshot> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.lock().clone()
    }

    /// Записать состояние. `true` — оно изменилось, то есть есть о чём сообщать
    /// наружу. Проверка «изменилось ли» живёт здесь, а не в вызывающем: копия
    /// «последнего отправленного» рядом с настоящим состоянием — это второй
    /// источник правды, который однажды разъедется с первым.
    pub fn set_state(&self, s: UiState) -> bool {
        let mut g = self.lock();
        if g.state == s {
            return false;
        }
        g.state = s;
        true
    }

    /// Записать отложенность смены микрофона. `true` — изменилось, есть о чём
    /// сообщать наружу. Снятие — тоже изменение: запись кончилась, следующая
    /// возьмёт новое устройство, и висящая строка «применится к следующей»
    /// стала бы враньём.
    pub fn set_mic_deferred(&self, deferred: bool) -> bool {
        let mut g = self.lock();
        if g.mic_deferred == deferred {
            return false;
        }
        g.mic_deferred = deferred;
        true
    }

    /// Записать фатальную ошибку. `true` — она первая; о повторной сообщать
    /// незачем (и нечем: выхода из этого состояния нет).
    pub fn set_fatal(&self, msg: &str) -> bool {
        let mut g = self.lock();
        if g.fatal.is_some() {
            return false;
        }
        g.fatal = Some(msg.to_string());
        // Раз записи больше не будет, состояние обязано это отражать, а не
        // застыть на «Идёт запись»: аудио-поток, который его двигал, уже мёртв,
        // и `sync` больше никогда не поправит эту надпись.
        g.state = UiState::Idle;
        true
    }

    /// `true` — изменилось, есть о чём сообщать. Снятие предупреждения — тоже
    /// изменение: устройство могли вернуть, и висящий баннер врал бы.
    pub fn set_device_warning(&self, w: Option<&str>) -> bool {
        let mut g = self.lock();
        let new = w.map(str::to_string);
        if g.device_warning == new {
            return false;
        }
        g.device_warning = new;
        true
    }

    /// `true` — записали впервые. Как и у `set_fatal`, повторно не
    /// перезаписывается: состояние это на весь процесс (см. поле), и второго
    /// отказа за один запуск быть не может.
    pub fn set_no_system_audio(&self, reason: &str) -> bool {
        let mut g = self.lock();
        if g.no_system_audio.is_some() {
            return false;
        }
        g.no_system_audio = Some(reason.to_string());
        true
    }

    /// Записать, какая запись сейчас пишется. `true` — изменилось. Снятие
    /// (`None`, запись кончилась) — тоже изменение, по той же причине, что и
    /// у `set_device_warning`: висящее значение соврало бы, что растёт файл,
    /// который уже закрыт.
    ///
    /// Аргумент — `(папка, основа)`, а не готовый `CurrentRecording`: тот, кто
    /// зовёт (`audio::sync`), знает пару значений из `App::current_recording`,
    /// а собирать из неё структуру — дело этого модуля, не звонящего.
    pub fn set_current_recording(&self, r: Option<(String, String)>) -> bool {
        let mut g = self.lock();
        let new = r.map(|(folder, base)| CurrentRecording { folder, base });
        if g.current_recording == new {
            return false;
        }
        g.current_recording = new;
        true
    }
}

/// Сообщить, что системный звук захватить не удалось и дальше пишется один
/// микрофон.
///
/// Каналы те же, что у [`fatal`], и по тем же причинам — кроме трея: он
/// показывает состояние записи, а она работает, и красить его в отказ значило
/// бы соврать. Разница с `fatal` в намерении: там «дальше ничего не будет»,
/// здесь «дальше будет половина».
///
/// Окно показывается по той же причине, что и в `fatal`: крестик у главного
/// окна не закрывает приложение, а прячет его (`WindowEvent::CloseRequested`
/// в `main.rs` зовёт `hide()`, не отдаёт закрытие ОС) — оно стартует видимым
/// (`"visible": true` в `tauri.conf.json`, политика `ActivationPolicy::Regular`),
/// но к моменту этой ошибки могло быть уже спрятано ровно так же, как прячется
/// по клику на крестик. Без явного показа предупреждение осталось бы только в
/// `Snapshot`, а на экране — нигде. Постоянная же часть — баннер в окне,
/// который живёт в `Snapshot` и переживает и перезагрузку webview, и закрытие
/// окна.
pub fn no_system_audio(app: &AppHandle, reason: String) {
    if !app.state::<Status>().set_no_system_audio(&reason) {
        return;
    }
    eprintln!("системный звук не захватывается: {reason}");
    let _ = app.emit("no-system-audio", reason.clone());
    let lang = i18n::active_lang(app);
    let _ = app
        .notification()
        .builder()
        .title(i18n::t("notify.micOnlyTitle", &lang))
        .body(i18n::t(NO_SYSTEM_AUDIO, &lang))
        .show();
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Сообщить о фатальной ошибке всеми каналами разом.
///
/// Каналов четыре, потому что ни один по отдельности не доходит:
/// - `emit` — только для тех, кто уже подписан (а на старте не подписан никто);
/// - трей — единственная часть GUI, которая на экране всегда, но её надо
///   заметить;
/// - тост — единственный канал, который сам придёт к пользователю, но Windows
///   глушит его под Focus Assist, а на macOS при первом запуске разрешение на
///   уведомления ещё не выдано, и баннера не будет вовсе;
/// - окно — единственный канал, который нельзя не заметить.
///
/// Пятый канал — `Snapshot` + `get_state` — не здесь, а в самом факте записи в
/// `Status`: он работает для тех, кто ещё даже не загрузился.
///
/// Окно показывается ЗДЕСЬ, а не только по клику в трее, потому что стартовое
/// окно может быть спрятано крестиком (`hide()`, а не закрытие), и без этого показа
/// самый ранний фатальный отказ — guard версии macOS в `audio::run()` — выглядел
/// бы как «приложение запустилось и ничего не делает»: окна нет, тоста нет,
/// трей серый, а причина видна только тому, кто догадался открыть окно из трея.
/// Ровно та же логика, что у `audio::ask()`: сообщение без окна — сообщение в
/// пустоту.
///
/// Нормальный старт это не трогает: сюда попадают только на фатальной ошибке, а
/// `set_fatal` пропускает дальше первой ровно один раз за процесс.
pub fn fatal(app: &AppHandle, msg: String) {
    if !app.state::<Status>().set_fatal(&msg) {
        return;
    }
    eprintln!("фатально: {msg}");
    let _ = app.emit("fatal", msg.clone());
    tray::set_fatal(app, &msg);
    let lang = i18n::active_lang(app);
    let _ = app
        .notification()
        .builder()
        .title(i18n::t("notify.fatalTitle", &lang))
        .body(i18n::t("notify.fatalBody", &lang))
        .show();
    // Даже если webview поднялся позже этого `emit`, он спросит `get_state` и
    // покажет ту же причину (см. `ui/main.js`), — поэтому здесь достаточно
    // показать окно, отдельно передавать в него текст не нужно.
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn снимок_по_умолчанию_это_idle_без_ошибки() {
        let s = Status::default();
        assert_eq!(
            s.snapshot(),
            Snapshot {
                state: UiState::Idle,
                fatal: None,
                device_warning: None,
                mic_deferred: false,
                no_system_audio: None,
                current_recording: None,
            }
        );
    }

    /// Снятие — такое же изменение, как установка: запись кончилась, следующая
    /// возьмёт новое устройство, и висящая строка «применится к следующей»
    /// стала бы враньём.
    #[test]
    fn отложенность_смены_микрофона_сообщается_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_mic_deferred(true));
        assert!(!s.set_mic_deferred(true));
        assert!(s.set_mic_deferred(false), "снятие — тоже изменение");
        assert!(!s.snapshot().mic_deferred);
    }

    /// Тот, кто открыл окно уже после смены устройства, обязан увидеть строку:
    /// событие к тому моменту давно ушло. Ровно та причина, по которой в этом
    /// модуле вообще живёт снимок.
    #[test]
    fn отложенность_видна_в_снимке() {
        let s = Status::default();
        s.set_mic_deferred(true);
        assert!(s.snapshot().mic_deferred);
    }

    #[test]
    fn set_state_сообщает_только_об_изменении() {
        let s = Status::default();
        assert!(
            s.set_state(UiState::Recording),
            "первое изменение — новость"
        );
        assert!(!s.set_state(UiState::Recording), "то же самое — не новость");
        assert!(s.set_state(UiState::Idle));
        assert_eq!(s.snapshot().state, UiState::Idle);
    }

    /// Смысл всей находки 1: тот, кто спросит ПОСЛЕ ошибки, обязан её увидеть —
    /// независимо от того, слушал ли он `emit` в момент, когда она случилась.
    #[test]
    fn fatal_виден_в_снимке_тому_кто_опоздал_на_событие() {
        let s = Status::default();
        assert!(s.set_fatal("детектор не поднялся"));
        assert_eq!(s.snapshot().fatal.as_deref(), Some("детектор не поднялся"));
    }

    #[test]
    fn fatal_гасит_состояние_в_idle() {
        let s = Status::default();
        s.set_state(UiState::Recording);
        s.set_fatal("аудио-поток умер");
        assert_eq!(
            s.snapshot().state,
            UiState::Idle,
            "мёртвый поток не может «продолжать запись»"
        );
    }

    #[test]
    fn повторный_fatal_не_перетирает_первый() {
        let s = Status::default();
        assert!(s.set_fatal("первая причина"));
        assert!(
            !s.set_fatal("вторая причина"),
            "о повторной сообщать нечего"
        );
        assert_eq!(
            s.snapshot().fatal.as_deref(),
            Some("первая причина"),
            "интересна причина, а не последствия"
        );
    }

    #[test]
    fn предупреждение_об_устройстве_сообщается_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(!s.set_device_warning(Some("Headset (Boss Bose)")));
        assert!(s.set_device_warning(None), "снятие — тоже изменение");
        assert_eq!(s.snapshot().device_warning, None);
    }

    /// Тот, кто открыл окно посреди записи, обязан увидеть предупреждение —
    /// ровно та же причина, по которой в снимке живёт fatal.
    #[test]
    fn предупреждение_видно_в_снимке() {
        let s = Status::default();
        s.set_device_warning(Some("Yeti"));
        assert_eq!(s.snapshot().device_warning.as_deref(), Some("Yeti"));
    }

    // ---- нет системного звука ---------------------------------------------

    #[test]
    fn отказ_тапа_запоминается_с_причиной() {
        let s = Status::default();
        assert!(s.set_no_system_audio("разрешение не выдано"));
        assert_eq!(
            s.snapshot().no_system_audio.as_deref(),
            Some("разрешение не выдано")
        );
    }

    #[test]
    fn повторный_отказ_тапа_не_перетирает_первый() {
        let s = Status::default();
        assert!(s.set_no_system_audio("первая причина"));
        assert!(!s.set_no_system_audio("вторая причина"));
        assert_eq!(
            s.snapshot().no_system_audio.as_deref(),
            Some("первая причина")
        );
    }

    /// Отказ тапа — не фатальная ошибка: запись работает, просто половинная.
    /// Без этой границы предупреждение погасило бы кнопки в UI
    /// (`показать_фатальную` их выключает) и остановило бы состояние в Idle,
    /// то есть отняло бы ровно ту ручную запись, ради которой режим и сделан.
    #[test]
    fn отказ_тапа_не_фатален_и_не_трогает_состояние() {
        let s = Status::default();
        s.set_state(UiState::Recording);
        s.set_no_system_audio("разрешение не выдано");
        let сн = s.snapshot();
        assert_eq!(сн.fatal, None, "запись работает — это не fatal");
        assert_eq!(
            сн.state,
            UiState::Recording,
            "идущую запись предупреждение не останавливает"
        );
    }

    // ---- какая запись пишется прямо сейчас ---------------------------------

    #[test]
    fn текущая_запись_сообщается_только_об_изменении() {
        let s = Status::default();
        assert!(s.set_current_recording(Some(("2026-07".into(), "2026-07-17_14-30_zoom".into()))));
        assert!(!s.set_current_recording(Some((
            "2026-07".into(),
            "2026-07-17_14-30_zoom".into()
        ))));
        assert!(s.set_current_recording(None), "снятие — тоже изменение");
        assert_eq!(s.snapshot().current_recording, None);
    }

    /// Тот, кто спросит снимок посреди записи (список записей, удаление),
    /// обязан узнать текущую запись — та же причина, по которой
    /// в снимке вообще живёт это поле.
    #[test]
    fn текущая_запись_видна_в_снимке() {
        let s = Status::default();
        s.set_current_recording(Some(("2026-07".into(), "2026-07-17_14-30_zoom".into())));
        assert_eq!(
            s.snapshot().current_recording,
            Some(CurrentRecording {
                folder: "2026-07".into(),
                base: "2026-07-17_14-30_zoom".into(),
            })
        );
    }
}
