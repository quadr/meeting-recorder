// Подставной Tauri для стенда состояний.
//
// Стенд открывает НАСТОЯЩИЕ ui/index.html и ui/main.js — здесь подменён только
// тот слой, который в приложении отвечает Rust: команды `invoke` и события
// `listen`. Поэтому на стенде видно ровно то, что увидит человек в окне, а не
// его нарисованное подобие: разъедется вёрстка — разъедется и здесь.
//
// Чего стенд НЕ проверяет: саму машину записи, права macOS, поведение трея и
// то, что бэкенд действительно шлёт эти события в этом порядке.
//
// Сцена выбирается параметром `?s=`; список — в serve.py.
(function () {
  const п = new URLSearchParams(location.search);
  const ВСПЛЫВАШКА = location.pathname.endsWith("ask.html");
  const S = п.get("s") || "idle";
  // Язык сцены задаётся `?lang=`, а не системой: у README две версии, и для
  // английской нужны английские кадры. Без параметра — русский, как раньше.
  const ЯЗЫК = п.get("lang") || "ru";

  /// Самое длинное имя устройства, какое встречалось живьём. Стоит здесь,
  /// потому что канон требует проверять раскладку именно на нём.
  const ДЛИННЫЙ_МИК =
    "Microphone Array (Intel(R) Smart Sound Technology (Intel(R) SST))";
  const ДЛИННОЕ_ИМЯ =
    "Созвон с командой про редизайн интерфейса и планы на следующий квартал — часть вторая";

  const слушатели = new Map();
  const emit = (имя, payload) =>
    (слушатели.get(имя) || []).forEach((cb) => cb({ payload }));
  const пауза = (мс, v) => new Promise((r) => setTimeout(() => r(v), мс));

  // ── Подставной список записей ───────────────────────────────────────────
  const сутки = 86400000;
  const t = (сдвиг, час, мин) => {
    const d = new Date(Date.now() - сдвиг * сутки);
    d.setHours(час, мин, 0, 0);
    return d;
  };
  const имя_файла = (d, хвост) => {
    const p = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}_${p(d.getHours())}-${p(d.getMinutes())}_${хвост}`;
  };
  const запись = (d, хвост, доп = {}) => ({
    name: имя_файла(d, хвост),
    folder: имя_файла(d, "x").slice(0, 7),
    mic: true,
    system: true,
    size: 48_500_000,
    transcript: false,
    duration_sec: 2412,
    ...доп,
  });

  const СПИСОК = [
    запись(t(0, 10, 0), "zoom-us"),
    запись(t(0, 14, 30), "microsoft-teams", { duration_sec: 1830, size: 31_200_000 }),
    запись(t(1, 11, 15), "slack", { duration_sec: 640, size: 12_800_000 }),
    запись(t(1, 16, 5), "discord", { duration_sec: 3720, size: 74_000_000 }),
    // Без системной дорожки — у неё карточка «Собеседников не слышно».
    запись(t(12, 9, 0), "unknown", { duration_sec: 55, size: 1_100_000, system: false }),
    // Перекос громкости — карточка «Вас слышно заметно тише».
    запись(t(12, 18, 40), "manual", { duration_sec: 900, size: 18_000_000, imbalance_db: 14.2 }),
  ];
  if (S === "long" || S === "rename") {
    СПИСОК[0].name = имя_файла(t(0, 10, 0), "x").slice(0, 17) + ДЛИННОЕ_ИМЯ;
  }
  if (S === "done" || S === "menudone") СПИСОК[0].transcript = true;

  // ── Снимок состояния, который отдаёт get_state ──────────────────────────
  const снимок = {
    mute: { mic: S === "muted", system: false, revision: 0 },
    state: ["recording", "muted"].includes(S) ? "recording" : S === "armed" ? "armed" : "idle",
    fatal: S === "fatal" ? "детектор не поднялся: no such device" : null,
    device_warning: S === "devwarn" ? "{0.0.1.00000000}.{9f1c…}" : null,
    mic_deferred: S === "deferred",
    no_system_audio: S === "nosys" ? "process tap: разрешение не выдано (-3812)" : null,
  };

  let метроном = null;
  const метр = () => {
    const ш = (b) => Math.max(0, Math.min(1, b + (Math.random() - 0.5) * 0.12));
    // 0,28 линейного пика — это -11 дБFS, то есть обычная речь на здоровом
    // уровне записи. На метре она даёт ~80%: см. канон 2.9, почему подсказка
    // говорит «до конца зелёного», а не «до середины».
    emit("levels", { mic: снимок.mute.mic ? 0 : ш(0.28), system: снимок.mute.system ? 0 : ш(0.19), monitoring: true, mute: { ...снимок.mute } });
  };

  const команды = {
    callabo_auth_status: () => пауза(0, false),
    get_state: () => {
      // В приложении `ask` уходит раньше смены состояния — повторяем порядок,
      // иначе строка взвода не узнает, кого услышали.
      if (S === "armed") emit("ask", "zoom.us");
      return пауза(30, снимок);
    },
    // Сцена «первая загрузка» — это список, который не приехал никогда.
    list_recordings: () =>
      пауза(S === "loading" ? 1e9 : 260, S === "empty" ? [] : СПИСОК),
    list_mic_devices: () =>
      пауза(120, [
        { id: "id-1", name: ДЛИННЫЙ_МИК },
        { id: "id-2", name: "MacBook Pro Microphone" },
        { id: "id-3", name: "AirPods Pro" },
      ]),
    get_config: () =>
      пауза(20, {
        mic_device_id:
          S === "devwarn" ? "{0.0.1.00000000}.{9f1c…}" : S === "longmic" ? "id-1" : "id-2",
        mic_device_name:
          S === "devwarn" || S === "longmic" ? ДЛИННЫЙ_МИК : "MacBook Pro Microphone",
        stt_gateway_url: "http://localhost:8080",
        stt_api_key: "секрет",
        // Поля редизайна. Без них `theme.js` и `i18n.js` не находят, что
        // применить, и молча уходят в системные значения — в headless-браузере
        // это светлая тема и английский, то есть стенд показывает не то, что
        // увидит человек с русской системой и тёмной темой.
        theme: "dark",
        language: ЯЗЫК,
        transcribe_mode: "server",
        audio_retention_days: null,
      }),
    // Команды, добавленные редизайном. Шим отклоняет неизвестную команду, а
    // отказ в неудачном месте обрывает инициализацию — стенд тогда показывает
    // скелетон вместо списка. Поэтому покрываем весь вызываемый набор, даже
    // там, где настоящий бэкенд ничего не возвращает.
    delete_recording: () => пауза(20, null),
    open_url: () => пауза(0, null),
    set_theme: () => пауза(0, null),
    set_language: () => пауза(0, null),
    set_audio_retention: () => пауза(0, null),
    set_transcribe_mode: () => пауза(0, null),
    // Локальная расшифровка: на стенде модель «на месте», чтобы был виден
    // рабочий вид экрана, а не вечное «скачайте модель».
    local_model_status: () =>
      пауза(20, { state: "ready", size_bytes: 1_600_000_000, free_bytes: 40_000_000_000 }),
    local_model_download: () => пауза(0, null),
    local_model_remove: () => пауза(0, null),

    set_mute: async ({ source, muted }) => {
      if (снимок.state !== "recording") throw new Error("Not recording");
      await пауза(120);
      снимок.mute = { ...снимок.mute, [source]: muted, revision: снимок.mute.revision + 1 };
      emit("mute", { ...снимок.mute });
      return { ...снимок.mute };
    },
    set_monitor: ({ on }) => {
      clearInterval(метроном);
      if (on) метроном = setInterval(метр, 100);
      else emit("levels", { mic: 0, system: 0, monitoring: false });
      return пауза(80);
    },
    send_event: ({ name }) => {
      if (name === "toggle") {
        снимок.state = снимок.state === "recording" ? "idle" : "recording";
        снимок.mute = { mic: false, system: false, revision: снимок.mute.revision + 1 };
        emit("mute", { ...снимок.mute });
        emit("state", снимок.state);
        if (снимок.state === "recording") метроном = setInterval(метр, 100);
        else clearInterval(метроном);
      }
      // Во взводе «да» говорит кнопка записи (toggle), а не отдельная кнопка:
      // confirm остаётся ответом всплывашки.
      if (name === "confirm") emit("state", (снимок.state = "recording"));
      if (name === "decline") emit("state", (снимок.state = "idle"));
      return пауза(60);
    },
    transcribe_recording: ({ folder, base }) => {
      const к = { folder: folder ?? null, base };
      пауза(300).then(() => emit("transcribe-progress", { ...к, stage: "uploading" }));
      пауза(1600).then(() => emit("transcribe-progress", { ...к, stage: "polling" }));
      return пауза(60);
    },
    cancel_transcription: ({ folder, base }) =>
      пауза(80).then(() => emit("transcribe-cancelled", { folder: folder ?? null, base })),
    rename_recording: () => пауза(120),
    set_mic_device: () => пауза(80),
    set_transcribe_config: () => пауза(80),
    open_recording_folder: () => пауза(40),
    open_folder: () => пауза(40),
    open_privacy_settings: () => пауза(40),
    open_repository: () => пауза(40),
  };

  window.__TAURI__ = {
    core: {
      invoke: (имя, арг = {}) =>
        команды[имя]
          ? Promise.resolve(команды[имя](арг))
          : Promise.reject(`на стенде нет команды ${имя}`),
    },
    event: {
      listen: (имя, cb) => {
        if (!слушатели.has(имя)) слушатели.set(имя, []);
        слушатели.get(имя).push(cb);
        // Всплывашка ничего не спрашивает у бэкенда — она только ждёт `ask`.
        // Главное окно получает то же событие из `get_state`, чтобы порядок
        // «сначала кто, потом состояние» совпадал с настоящим.
        if (имя === "ask" && ВСПЛЫВАШКА) setTimeout(() => emit("ask", "zoom.us"), 60);
        return Promise.resolve(() => {});
      },
    },
    // Нужен всплывашке (ui/ask.js): она прячет собственное окно.
    // `hide` нужен всплывашке (ui/ask.js), `setTitle` — главному окну: оно
    // переименовывает себя при смене языка (main.js:1549 и :2186). Обе
    // возвращают промис, потому что вызывающий вешает на них `.catch`.
    window: {
      getCurrentWindow: () => ({
        hide: () => Promise.resolve(),
        setTitle: () => Promise.resolve(),
      }),
    },
    // `main.js` разбирает `app` НА ВЕРХНЕМ УРОВНЕ, седьмой строкой. Без этой
    // ветки там TypeError до всего остального: модуль не выполняется вовсе,
    // и стенд показывает скелетон вместо списка, светлую тему вместо тёмной и
    // ключи вместо фраз. Симптомов три, причина одна — поэтому и лечится здесь,
    // а не подпорками в каждом из трёх мест.
    app: { getVersion: () => Promise.resolve("0.2.0") },
  };

  // ── Сцены, которые складываются из событий после старта ─────────────────
  const меню_строки = (и, потом) =>
    setTimeout(() => {
      document.querySelectorAll("#list .dots button")[и]?.click();
      потом && setTimeout(потом, 160);
    }, 450);

  addEventListener("load", () =>
    setTimeout(() => {
      if (["recording", "muted"].includes(S)) метроном = setInterval(метр, 100);
      if (S === "recstart") $("rec-btn")?.click();

      const первая = { folder: СПИСОК[0].folder, base: СПИСОК[0].name };
      const вторая = { folder: СПИСОК[1].folder, base: СПИСОК[1].name };
      const третья = { folder: СПИСОК[2].folder, base: СПИСОК[2].name };

      if (S === "transcribing") emit("transcribe-progress", { ...первая, stage: "polling" });
      if (S === "queued") {
        emit("transcribe-progress", { ...первая, stage: "polling" });
        emit("transcribe-progress", { ...вторая, stage: "queued:2" });
        emit("transcribe-progress", { ...третья, stage: "queued:3" });
      }
      if (S === "rowerror")
        emit("transcribe-error", { ...первая, message: "шлюз не ответил за 120 с" });
      if (S === "menubusy" || S === "cancel" || S === "cancelled")
        emit("transcribe-progress", { ...вторая, stage: "polling" });

      if (S === "menu") меню_строки(0);
      if (S === "menudone") меню_строки(0);
      if (S === "menubusy" || S === "cancel") меню_строки(1);
      if (S === "rename")
        меню_строки(0, () =>
          [...document.querySelectorAll(".menu button")]
            .find((b) => b.textContent.includes("Переименовать"))
            ?.click(),
        );
      if (S === "cancelled")
        меню_строки(1, () =>
          [...document.querySelectorAll(".menu button")]
            .find((b) => b.textContent.includes("Отменить"))
            ?.click(),
        );

      if (["settings", "deferred", "miccheck", "longmic"].includes(S))
        $("to-settings")?.click();
      if (S === "miccheck") setTimeout(() => $("check")?.click(), 220);
    }, 60),
  );

  function $(id) {
    return document.getElementById(id);
  }
})();
