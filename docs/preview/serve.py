#!/usr/bin/env python3
"""Стенд состояний интерфейса.

Открывает НАСТОЯЩИЕ `ui/index.html`, `ui/main.js` и `ui/ask.html` в браузере,
подменив только слой Tauri (см. `shim.js`). Нужен затем, что половину состояний
в живом приложении не вызвать: фатальную ошибку, отказ в захвате системного
звука, очередь из трёх расшифровок и первую загрузку списка можно увидеть,
только сломав что-нибудь по-настоящему.

Стенд НЕ заменяет живое приложение: машины записи, прав macOS и трея здесь нет.
Перед показом смотреть надо оба.

    python3 docs/preview/serve.py [порт]

Порт по умолчанию — 3015. Дальше открыть http://127.0.0.1:3015/

Дополнительно:
    /__measure?s=<сцена>   замер раскладки: что вылезает за 460, где появилась
                           горизонтальная прокрутка, что достижимо табом.
                           Результат уезжает в <title> — читается из headless.
    /__shot?s=<сцена>      та же сцена без панели, кадром ровно 460×640,
                           под `--screenshot` в headless-браузере.
"""

import http.server
import pathlib
import socketserver
import sys

ЗДЕСЬ = pathlib.Path(__file__).resolve().parent
UI = ЗДЕСЬ.parent.parent / "ui"
ПОРТ = int(sys.argv[1]) if len(sys.argv) > 1 else 3015

# Порядок тот же, в каком состояния проходят перед показом: сначала жизнь
# приложения, потом список, потом беды, потом настройки, потом крайние случаи.
ТИПЫ = {
    ".js": "application/javascript",
    ".css": "text/css",
    ".json": "application/json",
    ".svg": "image/svg+xml",
    ".html": "text/html",
}

СЦЕНЫ = [
    ("idle", "Покой"),
    ("armed", "Взвод — окно открыли, пока вопрос висит"),
    ("ask", "Всплывашка «Записать встречу?»"),
    ("recstart", "Идёт запись — начали при открытом окне"),
    ("recording", "Идёт запись — окно открыли посреди встречи"),
    ("muted", "Recording with microphone muted"),
    ("loading", "Первая загрузка списка"),
    ("empty", "Пустой список"),
    ("long", "Длинное имя записи"),
    ("rename", "Переименование"),
    ("menu", "Меню — обычная запись"),
    ("menudone", "Меню — уже расшифрована"),
    ("menubusy", "Меню — расшифровка идёт"),
    ("transcribing", "Идёт расшифровка"),
    ("queued", "Очередь из трёх"),
    ("done", "Запись с готовой расшифровкой"),
    ("cancelled", "Отмена расшифровки — после нажатия"),
    ("rowerror", "Ошибка расшифровки"),
    ("nosys", "Нет системного звука"),
    ("devwarn", "Недоступный микрофон"),
    ("fatal", "Фатальная ошибка"),
    ("settings", "Экран настроек"),
    ("miccheck", "Проверка микрофона"),
    ("longmic", "Длинное имя микрофона"),
    ("deferred", "Микрофон сменили под идущей записью"),
]

СТЕНД = """<!doctype html><html lang=ru><meta charset=utf-8>
<title>Состояния — Записи встреч</title>
<style>
 body{margin:0;background:#12131a;color:#c0caf5;
      font:13px/1.4 -apple-system,system-ui,sans-serif;
      display:flex;gap:28px;padding:24px;align-items:flex-start}
 .panel{width:270px;flex:none}
 h3{margin:0 0 10px;font-size:11px;letter-spacing:.07em;text-transform:uppercase;color:#565f89}
 a{display:block;padding:7px 10px;margin-bottom:2px;border-radius:7px;color:inherit;
   text-decoration:none;font-size:13px}
 a:hover{background:#1f2130}
 a.on{background:#7aa2f7;color:#12131a;font-weight:600}
 .win{width:460px;height:640px;border:1px solid #292e42;border-radius:10px;
      overflow:hidden;box-shadow:0 20px 60px rgba(0,0,0,.5);background:#1a1b26}
 .win.wide{width:876px;height:110px;background:linear-gradient(160deg,#2b1f4a,#1b2340 60%,#101827)}
 iframe{width:100%;height:100%;border:0;display:block}
 .note{color:#565f89;font-size:12px;margin-top:14px;line-height:1.55}
</style>
<div class=panel>
 <h3>Состояние</h3>
 {links}
 <p class=note>Окно ровно 460 — как настоящее.<br>
 Наведи мышь на строку списка: время справа сменится на «⋯».<br>
 То же по Tab — им экран проходится целиком.<br><br>
 Это интерфейс из <code>ui/</code>, но без бэкенда: машины записи, прав macOS
 и трея здесь нет. Перед показом смотреть надо и стенд, и живое приложение.</p>
</div>
<div><div class=win id=win><iframe id=f src="/index.html?s={first}"></iframe></div></div>
<script>
 const f=document.getElementById('f'), win=document.getElementById('win');
 document.querySelectorAll('a[data-s]').forEach(a=>a.onclick=e=>{
   e.preventDefault();
   document.querySelectorAll('a[data-s]').forEach(x=>x.classList.remove('on'));
   a.classList.add('on');
   // Всплывашка — отдельное окно, и размеры у неё свои.
   const ask = a.dataset.s === 'ask';
   win.classList.toggle('wide', ask);
   f.src = ask ? '/ask.html' : '/index.html?s=' + a.dataset.s;
 });
 document.querySelector('a[data-s]').classList.add('on');
</script>
</html>"""

# Замер живёт в рамке 460, а не в самой странице: ширину окна в headless-браузере
# не задать точно, а ширина iframe — задаётся.
МЕРКА = """<!doctype html><meta charset=utf-8><title>ждём</title>
<style>html,body{margin:0}iframe{width:460px;height:640px;border:0;display:block}</style>
<iframe id=f src="/index.html?s={s}"></iframe>
<script>
setTimeout(()=>{
  const d=document.getElementById('f').contentDocument;
  const W=d.documentElement.clientWidth;
  const шире=[],прокрутки=[],фокус=[];
  d.querySelectorAll('*').forEach(el=>{
    const r=el.getBoundingClientRect();
    if(r.width===0&&r.height===0)return;
    const имя=el.tagName.toLowerCase()+(el.id?'#'+el.id:'')
      +(typeof el.className==='string'&&el.className.trim()
        ?'.'+el.className.trim().split(/\\s+/).join('.'):'');
    if(r.right>W+0.5||r.left<-0.5) шире.push(имя+' ['+r.left.toFixed(0)+'..'+r.right.toFixed(0)+']');
    if(el.scrollWidth>el.clientWidth+0.5&&el.clientWidth>0)
      прокрутки.push(имя+' sw='+el.scrollWidth+' cw='+el.clientWidth);
  });
  d.querySelectorAll('a[href],button,input,select,textarea,[tabindex]').forEach(el=>{
    const r=el.getBoundingClientRect();
    const ст=el.ownerDocument.defaultView.getComputedStyle(el);
    if(r.width>0&&r.height>0&&ст.visibility!=='hidden'&&!el.disabled)
      фокус.push(el.tagName.toLowerCase()+(el.id?'#'+el.id:'.'+(el.className||'')));
  });
  document.title=JSON.stringify({ширина:W,шире:шире,прокрутки:прокрутки,фокус:фокус});
},3000);
</script>"""


class Стенд(http.server.SimpleHTTPRequestHandler):
    def отдать(self, тело, тип):
        b = тело.encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", тип)
        self.send_header("Content-Length", str(len(b)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(b)

    def пары(self):
        запрос = self.path.split("?")
        if len(запрос) < 2:
            return {}
        return dict(п.split("=", 1) for п in запрос[1].split("&") if "=" in п)

    def сцена(self):
        return self.пары().get("s", "idle")

    def язык(self):
        """`ru` по умолчанию: у README две версии, и для английской нужны
        английские кадры. Чужие значения не пропускаем — i18n знает два."""
        я = self.пары().get("lang", "ru")
        return я if я in ("ru", "en") else "ru"

    def со_стендом(self, файл):
        """Тот же файл из ui/, но с подставным Tauri первым скриптом в <head>."""
        html = (UI / файл).read_text(encoding="utf-8")
        метка = "<head>" if "<head>" in html else "<body>"
        return html.replace(метка, метка + '\n<script src="/__shim.js"></script>', 1)

    def do_GET(self):
        путь = self.path.split("?")[0]
        if путь in ("/", "/index"):
            ссылки = "".join(f'<a href=# data-s="{к}">{н}</a>' for к, н in СЦЕНЫ)
            return self.отдать(
                СТЕНД.replace("{links}", ссылки).replace("{first}", СЦЕНЫ[0][0]),
                "text/html; charset=utf-8",
            )
        if путь == "/__shim.js":
            return self.отдать(
                (ЗДЕСЬ / "shim.js").read_text(encoding="utf-8"),
                "application/javascript; charset=utf-8",
            )
        if путь == "/__measure":
            return self.отдать(МЕРКА.replace("{s}", self.сцена()), "text/html; charset=utf-8")
        if путь == "/__shot":
            # Высота НЕ фиксирована. Раньше стояло 640 — высота настоящего
            # окна, — и всё, что в него не влезало, обрезалось посреди
            # карточки: на кадре оставался обрубок фразы. Для съёмки нужен
            # экран целиком, поэтому рамка растёт под содержимое. Ширина
            # остаётся настоящей: раскладка зависит от неё, не от высоты.
            #
            # Фон прозрачный, углы скруглены: кадр кладут в README на чужой
            # фон, и прямоугольник со своей подложкой выглядит там наклейкой.
            # Снимать с `--default-background-color=00000000`.
            #
            # Высота меряется в цикле, а не один раз по `load`: сцены досыпают
            # строки событиями уже после загрузки (см. shim.js), и однократный
            # замер поймал бы список короче настоящего.
            return self.отдать(
                "<!doctype html><meta charset=utf-8>"
                "<style>html,body{margin:0;background:transparent}"
                ".ramka{width:460px;border-radius:14px;overflow:hidden;display:block}"
                "iframe{width:460px;height:640px;border:0;display:block}</style>"
                f'<div class=ramka><iframe id=f src="/index.html?s={self.сцена()}&lang={self.язык()}"></iframe></div>'
                "<script>"
                "var f=document.getElementById('f');"
                "f.addEventListener('load',function(){"
                "var d=f.contentDocument;var n=0;"
                "var t=setInterval(function(){"
                "var b=d.querySelector('.body');"
                "var over=b?b.scrollHeight-b.clientHeight:0;"
                "if(over>0){f.style.height=(f.clientHeight+over)+'px';}"
                "if(++n>20){clearInterval(t);}"
                "},150);"
                "});"
                "</script>",
                "text/html; charset=utf-8",
            )
        if путь in ("/index.html", "/ask.html"):
            return self.отдать(self.со_стендом(путь.lstrip("/")), "text/html; charset=utf-8")

        файл = UI / путь.lstrip("/")
        # Подкаталоги ui/ отдаются тоже: редизайн разложил тексты по
        # `ui/i18n/strings.json`, а иконки по `ui/assets/`. Прежняя проверка
        # `файл.parent == UI` защищала от обхода путей, но заодно роняла их в
        # 404 — стенд показывал интерфейс без стилей и с ключами вместо фраз.
        # Обход путей закрыт честнее: сравниваем разрешённый путь с корнем.
        try:
            внутри = файл.resolve().is_relative_to(UI.resolve())
        except (OSError, ValueError):
            внутри = False
        if файл.is_file() and внутри:
            # Тип обязателен верный: CSS, отданный как text/html, браузер
            # молча игнорирует — именно так стенд и «терял» вёрстку.
            тип = ТИПЫ.get(файл.suffix, "application/octet-stream")
            return self.отдать(файл.read_text(encoding="utf-8"), f"{тип}; charset=utf-8")
        self.send_error(404)

    def log_message(self, *_):
        pass


socketserver.TCPServer.allow_reuse_address = True
with socketserver.TCPServer(("127.0.0.1", ПОРТ), Стенд) as сервер:
    print(f"стенд состояний: http://127.0.0.1:{ПОРТ}/")
    сервер.serve_forever()
