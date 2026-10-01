//! Трей: иконка-индикатор и меню.
//!
//! Три «активных» состояния (взвод, запись, отказ) по-прежнему рисуются
//! кодом — это залитые кружки, и держать ради них PNG-и в репозитории лишняя
//! связь (файл переименовали → иконка молча пропала). `Image::new_owned`
//! принимает сырую RGBA, чего для кружка более чем достаточно.
//!
//! Состояние покоя (Idle) — исключение: там нет цвета состояния, только сама
//! иконка приложения — фирменный знак. Она держится файлом, `icons/tray.png`,
//! и идёт в шаблонном режиме (см. `set_icon_with_as_template` в `repaint`):
//! macOS сама красит её в системный цвет строки меню — чёрным на светлом
//! фоне, белым на тёмном, — как и остальные иконки меню-бара. Отдельного
//! файла под тёмную/светлую тему не нужно ровно поэтому: цвет в файле не
//! важен, важна только форма (прозрачность/непрозрачность).

use crate::audio::{Ctl, UiState};
use crate::i18n;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Wry};

/// Текущее состояние для иконки. Глобаль, а не поле в `State`, потому что читать
/// её надо из потока-мигалки, а писать — из аудио-потока; заводить ради двух
/// байт `Arc<Mutex<_>>` и протаскивать его через оба места дороже, чем польза.
/// Трей в приложении один, экземпляр приложения — тоже.
static ICON_STATE: AtomicU8 = AtomicU8::new(0);
/// Фаза мигания в Armed: true — кружок горит, false — притушен.
static BLINK_ON: AtomicBool = AtomicBool::new(true);
/// Пункт «Начать/Остановить запись»: его текст меняется под состояние.
///
/// Хранится здесь, потому что достать его из трея обратно нечем — у
/// `TrayIcon` есть `set_menu`, но нет `menu()` (проверено компилятором на
/// tauri 2.11). Единственный способ дотянуться до пункта после сборки меню —
/// не терять ссылку на него.
static TOGGLE_ITEM: OnceLock<MenuItem<Wry>> = OnceLock::new();
/// Текст фатальной ошибки для подсказки трея. `OnceLock`, потому что фатальная
/// ошибка одна: выхода из этого состояния нет, перезаписывать нечем.
static FATAL_MSG: OnceLock<String> = OnceLock::new();

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const RECORDING: u8 = 2;
/// Аудио-поток мёртв. Терминальное состояние: подняться обратно некому — всё
/// `!Send` умерло вместе с потоком.
const FATAL: u8 = 3;

/// Как часто мигает иконка в Armed.
const BLINK: Duration = Duration::from_millis(600);

/// Сколько ждать финализации, прежде чем выйти силой.
///
/// Финализация — это дописать хвост кольца и закрыть два файла, доли секунды.
/// 5 с — запас на тик цикла (200 мс), опрос детектора и медленный диск, а не
/// оценка нормальной работы: срабатывание этого таймера означает, что
/// аудио-поток завис. Меньше ставить нельзя — оборвём живую финализацию и сами
/// сделаем тот самый битый WAV.
const QUIT_GRACE: Duration = Duration::from_secs(5);

const SIZE: u32 = 32;

/// Залитый кружок SIZE×SIZE в RGBA. `barred` — вырезать поперёк полосу (знак
/// «нельзя»).
///
/// Сглаживания нет намеренно: на 32×32 в трее его не видно, а лишний код —
/// видно. Полоса именно вырезается, а не рисуется: цвет фона трея заранее
/// неизвестен (тёмная тема, светлая, произвольные обои), а дырка видна на любом.
fn mark(r: u8, g: u8, b: u8, a: u8, barred: bool) -> Image<'static> {
    let mut buf = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = (SIZE as f32 - 1.0) / 2.0;
    let radius = c - 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (dx, dy) = (x as f32 - c, y as f32 - c);
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            if barred && dy.abs() <= 2.5 {
                continue;
            }
            let i = ((y * SIZE + x) * 4) as usize;
            buf[i] = r;
            buf[i + 1] = g;
            buf[i + 2] = b;
            buf[i + 3] = a;
        }
    }
    Image::new_owned(buf, SIZE, SIZE)
}

fn dot(r: u8, g: u8, b: u8, a: u8) -> Image<'static> {
    mark(r, g, b, a, false)
}

/// Фирменный знак покоя — вшит в бинарь, а не читается с диска: путь
/// относительно исполняемого файла на macOS ненадёжен (bundle можно
/// перенести), а лишний способ не найти иконку на старте того не стоит.
/// Файл один: цвет в нём не важен (см. докблок файла) — иконка идёт
/// шаблонной, и систему красит его сама macOS.
fn brand_icon() -> Image<'static> {
    static ICON: OnceLock<Image<'static>> = OnceLock::new();
    ICON.get_or_init(|| {
        Image::from_bytes(include_bytes!("../icons/tray.png"))
            .expect("icons/tray.png обязан быть валидным PNG")
    })
    .clone()
}

/// Иконка под состояние. Шаблонный режим (см. `repaint`) — только у Idle:
/// три активных состояния несут собственный цвет и идут как есть.
fn icon_for(state: u8, blink_on: bool) -> Image<'static> {
    match state {
        // Записи не будет. Красный перечёркнутый: цветом похож на «пишем»,
        // формой — заведомо нет, и эту разницу видно даже боковым зрением.
        FATAL => mark(220, 50, 50, 255, true),
        // Пишем — красный, всегда горит.
        RECORDING => dot(220, 50, 50, 255),
        // Вопрос висит — жёлтый, мигает: состояние требует ответа.
        ARMED if blink_on => dot(230, 170, 30, 255),
        ARMED => dot(230, 170, 30, 70),
        // Idle — фирменный знак, цветом под тему строки меню.
        _ => brand_icon(),
    }
}

/// Перерисовать иконку и текст пункта меню под текущее состояние.
///
/// Зовётся только с главного потока (см. `set_state`): GUI-объекты Windows
/// принадлежат потоку, который их создал.
fn repaint(app: &AppHandle) {
    let state = ICON_STATE.load(Ordering::Relaxed);
    let Some(tray) = app.tray_by_id("main") else {
        return;
    };
    let lang = i18n::active_lang(app);
    let icon = icon_for(state, BLINK_ON.load(Ordering::Relaxed));
    // Шаблонный режим — только для Idle (фирменный знак, см. докблок файла);
    // три активных состояния несут собственный цвет, и шаблон стёр бы его в
    // сплошную заливку. Атомарный сеттер вместо `set_icon` + отдельного
    // `set_icon_as_template`: раздельные вызовы на macOS рисуют кадр дважды
    // и дают видимое мигание при каждой смене состояния.
    let _ = tray.set_icon_with_as_template(Some(icon), state == IDLE);
    let _ = tray.set_tooltip(Some(match state {
        // FATAL_MSG — ключ словаря (см. `status::DEAD`) либо, в редких
        // случаях из `audio.rs`, произвольный технический текст. `i18n::t`
        // безопасен для обоих: ключ переводится, текст без ключа возвращается
        // как есть (см. докблок `i18n::t`).
        FATAL => FATAL_MSG
            .get()
            .map_or_else(|| i18n::t("tray.tipBroken", &lang), |m| i18n::t(m, &lang)),
        RECORDING => i18n::t("tray.tipRecording", &lang),
        ARMED => i18n::t("tray.tipArmed", &lang),
        _ => i18n::t("tray.tipIdle", &lang),
    }));

    if let Some(item) = TOGGLE_ITEM.get() {
        let _ = item.set_text(match state {
            FATAL => i18n::t("tray.unavailable", &lang),
            RECORDING => i18n::t("tray.stop", &lang),
            ARMED => i18n::t("tray.recordThis", &lang),
            _ => i18n::t("tray.start", &lang),
        });
        // Пункт меню, который гарантированно ничего не сделает, не должен
        // выглядеть работающим: живой на вид GUI поверх мёртвой записи — это и
        // есть починенная болезнь, а не её симптом.
        let _ = item.set_enabled(state != FATAL);
    }
}

/// Перерисовать трей на текущем действующем языке, не меняя состояние.
///
/// Зовётся из `set_language`: смена языка не трогает `ICON_STATE`, но текст
/// пункта меню и подсказка иконки должны обновиться немедленно, а не только
/// на следующую смену состояния записи.
pub fn refresh_texts(app: &AppHandle) {
    let h = app.clone();
    let _ = app.run_on_main_thread(move || repaint(&h));
}

/// Сообщить трею новое состояние. Можно звать откуда угодно — перерисовка сама
/// уедет на главный поток.
pub fn set_state(app: &AppHandle, st: UiState) {
    // Из FATAL дороги назад нет: аудио-поток, который его вызвал, уже не
    // работает, и перекрасить иконку обратно в «всё хорошо» значило бы соврать.
    if ICON_STATE.load(Ordering::Relaxed) == FATAL {
        return;
    }
    let v = match st {
        UiState::Idle => IDLE,
        UiState::Armed => ARMED,
        UiState::Recording => RECORDING,
    };
    ICON_STATE.store(v, Ordering::Relaxed);
    // Каждый заход в Armed начинается с горящей фазы, иначе первый кадр мог бы
    // оказаться притушенным и выглядеть как «ничего не произошло».
    BLINK_ON.store(true, Ordering::Relaxed);
    let h = app.clone();
    let _ = app.run_on_main_thread(move || repaint(&h));
}

/// Показать в трее, что записи больше не будет.
///
/// Зовётся из [`crate::status::fatal`], а не напрямую: трей — один из каналов,
/// но не единственный. Иконка ценна тем, что она на экране всегда: окно скрыто,
/// событие мог никто не слушать, тост глушится под Focus Assist.
pub fn set_fatal(app: &AppHandle, msg: &str) {
    let _ = FATAL_MSG.set(msg.to_string());
    ICON_STATE.store(FATAL, Ordering::Relaxed);
    let h = app.clone();
    let _ = app.run_on_main_thread(move || repaint(&h));
}

/// Поток-мигалка. Работает только в Armed: в остальных состояниях иконка
/// статична, и будить главный поток незачем.
fn spawn_blinker(app: &AppHandle) {
    let h = app.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(BLINK);
        if ICON_STATE.load(Ordering::Relaxed) != ARMED {
            continue;
        }
        BLINK_ON.fetch_xor(true, Ordering::Relaxed);
        let h2 = h.clone();
        let _ = h.run_on_main_thread(move || repaint(&h2));
    });
}

/// Выход из трея.
///
/// Идёт в аудио-поток, а не в `app.exit()`: там `ManualStop` финализирует
/// запись, и только после этого процесс уходит. Прямой выход отсюда оставил бы
/// WAV с нулевой длиной в заголовке — файл существует, открывается и играет
/// тишину.
///
/// Если аудио-поток мёртв (канал закрыт), финализировать уже нечего и некому —
/// выходим сами, иначе «Выход» перестал бы работать вовсе.
///
/// Отдельно — случай «канал принял, но забрать некому»: см. [`QUIT_GRACE`].
fn request_quit(app: &AppHandle, tx: &Sender<Ctl>) {
    if tx.send(Ctl::Shutdown).is_err() {
        app.exit(0);
        return;
    }
    // Успешный send — это ещё не доставка: канал не ограничен, и если
    // аудио-поток завис в det.poll()/pump_audio, команда просто ляжет в очередь
    // навсегда. Тогда «Выход» молча не работает, и пользователь отвечает на это
    // убийством процесса.
    //
    // Форсированный выход файл не спасёт — finalize() в зависшем потоке не
    // случится ни при каком раскладе, и WAV останется с нулевой длиной в
    // заголовке. Но он и не хуже: убитый пользователем процесс дал бы ровно тот
    // же файл, только после ожидания в пустоту. Спасать тут нечего — есть смысл
    // хотя бы не врать кнопкой «Выход».
    let h = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(QUIT_GRACE);
        // Досюда доходим, только если процесс ещё жив, то есть аудио-поток НЕ
        // вызвал exit(0) сам: успей он — этот поток умер бы вместе с процессом.
        eprintln!("аудио-поток не ответил на Shutdown за {QUIT_GRACE:?} — выходим силой");
        h.exit(0);
    });
}

/// Собрать трей. `tx` — тот же канал в аудио-поток, что и у остальных команд:
/// трей ничего не решает сам.
pub fn build(app: &AppHandle, tx: Sender<Ctl>) -> tauri::Result<()> {
    // Действующий язык уже управляется (см. `setup()` в `main.rs` — он идёт
    // ДО этого вызова ровно затем, чтобы меню собиралось сразу на нужном
    // языке, а не на дефолтном с последующей правкой).
    let lang = i18n::active_lang(app);
    let toggle = MenuItem::with_id(app, "toggle", i18n::t("tray.start", &lang), true, None::<&str>)?;
    let folder = MenuItem::with_id(
        app,
        "folder",
        i18n::t("tray.folder", &lang),
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", i18n::t("tray.quit", &lang), true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &folder, &quit])?;
    let _ = TOGGLE_ITEM.set(toggle.clone());

    TrayIconBuilder::with_id("main")
        .icon(icon_for(IDLE, true))
        // Приложение всегда стартует в Idle (см. `ICON_STATE`), поэтому
        // начальная иконка — фирменный знак, и он шаблонный (см. `repaint`).
        .icon_as_template(true)
        .tooltip(i18n::t("tray.tipIdle", &lang))
        .menu(&menu)
        // Меню — по правой кнопке (привычка Windows), левая открывает окно.
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "toggle" => {
                // Провал send здесь — это «нажал и ничего»: пункт меню есть,
                // запись не идёт, причины не видно. Молчать об этом нельзя ровно
                // так же, как и в request_quit ниже, — случай один и тот же.
                if tx.send(Ctl::Toggle).is_err() {
                    crate::status::fatal(app, crate::status::DEAD.to_string());
                }
            }
            "folder" => {
                if let Err(e) = crate::open_folder() {
                    eprintln!("не удалось открыть папку: {e}");
                }
            }
            "quit" => request_quit(app, &tx),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                crate::show_main_window(app);
            }
        })
        .build(app)?;

    spawn_blinker(app);
    Ok(())
}
