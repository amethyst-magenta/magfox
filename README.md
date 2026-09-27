# magfox

`magfox` — небольшой локальный launcher для Firefox. Он автоматически
запускает стартовую страницу через `darkhttpd`, открывает её в Firefox и
завершает сервер, когда Firefox закрыт.

## Как это работает

При первом запуске `magfox`:

1. запускает сервер на постоянном порте `127.0.0.1:8765`;
2. запускает `darkhttpd` с каталогом `startpage/`;
3. открывает стартовую страницу в Firefox;
4. следит за процессами Firefox;
5. останавливает свой `darkhttpd` после завершения Firefox.

Порт всегда один и тот же, поэтому новая вкладка может обращаться к серверу
по заранее известному адресу:

```text
http://127.0.0.1:8765/
```

Lock-файл находится в:

```text
$XDG_RUNTIME_DIR/magfox.lock
```

Повторный запуск `magfox` не создаёт второй сервер. Переданные аргументы
передаются уже работающему Firefox:

```bash
magfox https://example.org
```

Без аргументов открывается локальная стартовая страница.
Каждый вызов использует режим Firefox `--new-window`, поэтому открывается
новое окно, а не новая вкладка в уже открытом окне.

## Запуск из исходников

Нужны Rust/Cargo, Firefox и `darkhttpd`.

Для разработки:

```bash
cargo run
```

Для release-сборки:

```bash
cargo build --release
./target/release/magfox
```

Обработка `SIGINT` и `SIGTERM` останавливает `darkhttpd` и удаляет lock-файл.

## Структура

```text
magfox/
├── Cargo.toml
├── Cargo.lock
├── src/main.rs
└── startpage/
    ├── index.html
    ├── style.css
    └── main.js
```
