# Contributing to MeetRec

Thanks for taking a look. MeetRec is a desktop recorder for meetings, built with Tauri: a Rust core for audio capture and call detection, plain HTML and JavaScript for the interface.

Bug reports are as useful as code. If you hit something and don't want to dig into the source, open an issue. Audio capture behaves differently on almost every machine, and we can't test them all.

## Before you start

Install:

- **Rust**, stable channel: [rustup.rs](https://rustup.rs)
- **Node.js** with npm
- Platform build tools: Xcode Command Line Tools on macOS, Visual Studio Build Tools with the C++ workload on Windows

Supported systems:

| | Version |
|---|---|
| macOS | 11.0 and up |
| macOS, capturing system audio | **14.4 and up**. The Core Audio process tap does not exist before that |
| Windows | 10 and 11 |

That macOS 14.4 line explains most "it records my microphone but not the other person" reports.

## Running it

```sh
npm install
npx tauri dev
```

The app takes over audio devices and asks for microphone permission on first run.

## Tests

```sh
cargo test --workspace
```

`--workspace` matters. The project is two crates, the core in `src/` and the GUI in `src-tauri/`, and without it you only run half the suite. Tests live next to the code they cover, in `#[cfg(test)]` modules, not in a separate directory.

Callabo UI tests run with `npm run test:callabo-ui`. Transcript-removal regression tests run with `npm run test:recorder-ui`.

## Building

```sh
npx tauri build --no-bundle    # compile only, what CI runs
npx tauri build                # full installer: NSIS on Windows, .app and .dmg on macOS
```

On macOS there is one extra check after a build:

```sh
npm run check-tap-lazy-bind
```

It verifies that the Core Audio process tap is bound lazily. If it isn't, the app refuses to launch on macOS older than 14.4 instead of simply doing without system audio.

## Layout

```
src/                 core: audio capture, call detection, storage
  capture/           macos.rs — Core Audio process tap
                     windows.rs — WASAPI
  detector/          which app is on a call, per platform
src-tauri/           the Tauri GUI crate
  src/i18n.rs        reads strings.json for tray and notifications
ui/                  frontend: HTML and plain JavaScript, no framework
  i18n/strings.json  every piece of user-facing text
  tokens.css         colors, spacing, typography
.github/workflows/   CI
docs/                design notes and implementation plans, in Russian
```

## Text in the interface

**Every string a person can read lives in `ui/i18n/strings.json`.** Nothing is hardcoded in `.js` or `.rs`, and a pull request that hardcodes one will be asked to move it.

The file is one shared source for both sides of the app:

```json
{
  "settings": {
    "audioTitle": { "ru": "Звук", "en": "Audio" }
  }
}
```

Both `ru` and `en` are required. A missing translation shows up as a hole in the interface, not as a quiet fallback to the other language. Read it with `window.i18n.t("settings.audioTitle")` from JavaScript, or `t("settings.audioTitle", lang)` from Rust. Rust embeds the same file at compile time, so the two can never drift apart.

If you don't speak Russian, put the English string in both fields and say so in the pull request. Someone will fix the Russian.

## Platform-specific code

Audio capture and call detection are written twice, once per platform, in `capture/` and `detector/`. A change to one usually needs the mirror change in the other. CI builds on both, so a one-sided change fails there rather than in review.

## Commits and pull requests

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org): `feat:`, `fix:`, `docs:`, `chore:`, `perf:`. The existing history is in Russian; English is equally welcome, so write in whichever you think in.

Before you open a pull request:

- `cargo test --workspace` passes
- `cargo fmt` has been run
- new user-facing text is in `strings.json`, in both languages

CI runs the tests and a build on Windows and macOS. Both have to be green.

Small fixes can go straight to a pull request. For anything larger, open an issue first. Parts of this codebase have constraints that aren't obvious from reading it, and it's a shame to find that out after the work is done.

## License

MIT. Contributions are accepted under the same terms.
