<p align="center">
  <img src="ui/assets/logo-white-red.svg#gh-dark-mode-only" width="96" alt="MeetRec">
  <img src="ui/assets/logo-black-red.svg#gh-light-mode-only" width="96" alt="MeetRec">
</p>

<h1 align="center">MeetRec</h1>

<p align="center">
  Records your calls from your own machine. No bot joins the meeting, and the files stay on your disk.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT"></a>
  <img src="https://img.shields.io/badge/macOS-14.4%2B-lightgrey" alt="macOS 14.4+">
  <img src="https://img.shields.io/badge/Windows-10%2F11-lightgrey" alt="Windows 10/11">
  <a href="README.ru.md">Русская версия</a>
</p>

---

MeetRec notices when a call starts, offers to record it, and writes two separate tracks: your microphone and the system audio. Afterwards it can turn the recording into text with the speakers kept apart.

<p align="center">
  <img src="docs/media/meetrec.gif" width="720" alt="The MeetRec window: a list of recorded calls from Zoom, Teams, Slack and Discord, a recording running with both tracks, the recording menu and the settings screen">
</p>

## What it does

- **Notices calls.** Zoom, Teams, Slack and Discord are recognised by their process. A small panel appears over the call window and asks whether to record.
- **Keeps the first seconds.** Audio runs through a ring buffer, so recording starts a few seconds before you answer the question. Opening lines are not lost.
- **Two separate channels.** Your microphone and the system audio go into one WAV file, with each source on its own channel.
- **Stays out of the way.** An icon in the menu bar, a global shortcut, monthly folders, renaming, per-device microphone choice.

## What makes it different

Most meeting recorders put a bot into the call and keep the files on their servers. MeetRec takes the audio from your own machine and writes it to your own disk. Nobody in the call sees another participant, and no recording leaves the computer unless you explicitly upload it to Callabo.

## Install

Download the latest build from [Releases](../../releases).

**macOS** — `MeetRec_x.y.z_aarch64.dmg` for Apple Silicon, `MeetRec_x.y.z_x64.dmg` for Intel.

The app is not signed with an Apple developer certificate, so the first launch is blocked. Right-click the app and choose Open, or run:

```sh
xattr -cr /Applications/MeetRec.app
```

**Windows** — `MeetRec_x.y.z_x64-setup.exe` to install, or `MeetRec_vx.y.z_x64-portable.exe` to run without installing.
For Windows ARM64, use `MeetRec_vx.y.z_arm64-portable.exe` (no installer).

The app notices new releases on its own: once a day it asks GitHub and, if a newer version is out, shows a "Download / Later" banner in the window. Download opens the release page in your browser; installing is manual, same as the first time. Nothing but the version number is exchanged.

## Requirements

- **macOS 14.4 or newer.** System audio is captured through the Core Audio process tap API, which does not exist in earlier versions.
- **Windows 10 or 11.** System audio goes through WASAPI loopback.

## Permissions

On macOS the system asks twice: once for the microphone, once for screen and system audio recording. Both are needed — without the second one, only your own voice is recorded and calls are not detected at all, because MeetRec recognises a call by the system audio.

The permission is tied to the exact binary. After you replace the app with a new build, macOS may ask again.

On Windows no separate permission is required.

## Where the files go

```
~/Recordings/2026-09/
  2026-09-02_14-30_zoom.wav           channel 1: microphone; channel 2: system audio
```

On Windows the same tree lives in `%USERPROFILE%\Recordings\`.

New recordings use one two-channel WAV (16 kHz, 16-bit PCM): 512 kbps total,
about 230 MB per hour. The sources remain separate channels, not a mixed track.
If system capture is unavailable, the WAV contains only the microphone (256 kbps).
Older separate `.mic.wav` and `.system.wav` recordings remain supported.

## Callabo and privacy

Recording is entirely local. Audio leaves this computer only when you explicitly choose an upload to Callabo. Built-in transcription, gateway/Whisper settings, model downloads, transcription queues, and transcript-dependent audio cleanup have been removed. Existing transcript folders and downloaded model files are left untouched; recordings containing only legacy transcripts no longer appear in the recording list.

### Upload to Callabo

Settings → Callabo: enter a Personal Access Token, click **Save token / connect**, and select the **default workspace**. Then open a completed recording’s actions menu → **Upload to Callabo**. The dialog lets you choose the workspace, title, visibility, teams, transcription language, labels, and optional access grants. New combined WAV files (including microphone-only WAVs) are supported; merge legacy separate tracks first. Uploads are manual and send the original audio to Callabo for processing under your account’s limits.

On Windows the PAT is saved in the current user's Windows Credential Manager (`net.meetrec.app/Callabo/PAT`), not in `config.json`, logs, or upload receipts. The saved PAT never returns to the webview; the input is cleared after saving. **Remove saved token** deletes this app's credential. There is no plaintext fallback; secure token persistence for other OSes is not implemented yet.

Every dialog starts with a blank title (the API title field is omitted so Callabo can generate one) and `workspace` visibility. Verify visibility before sending. Teams, language, labels, and access grants from the last **successful** upload are saved separately for each workspace. Cancelled/failed uploads do not change these defaults; switching the dialog's workspace does not change the Settings default. Template overrides are not documented by the upload API, so the dialog shows the selected teams' template settings but does not send an invented template field.

A non-secret `<recording>.callabo.json` history stores every remote record ID, workspace, upload choices, and state. A finished recording can be uploaded again, including to another workspace; the recording list displays the successful workspace names once each. Old single-upload receipts are read automatically and preserved when adding uploads. Failed file transfers reuse the unfinished record in the selected workspace only with the same choices on retry. If creation/completion timed out and the result is uncertain, check Callabo before retrying; the app refuses to create a duplicate automatically in that workspace, without blocking other workspaces. Renaming/deleting a local recording also moves/deletes its history, not the remote Callabo records.

Launching MeetRec again restores and focuses the existing window (even from the tray or minimized), without restarting recording or registering another hotkey. When switching from a build without single-instance support, quit that older build through its tray menu once before starting the new executable.

This integration follows the v1 upload protocol used by the [official Callabo CLI](https://github.com/rtzr/callabo-cli) v0.1.13. The [public v2 API](https://callabo.ai/en/developers) does not currently document uploading; workspace API/PAT permissions are required. Mock API tests cover the protocol; live account compatibility still needs verification.

## Shortcuts

`Ctrl+Shift+R` starts and stops recording. It works with the window closed.

## Build from source

Requires Rust and Node.

```sh
git clone https://github.com/mmaximov97/meeting-recorder
cd meeting-recorder
cargo test --workspace
npm install
npx tauri build
```

Use `npx tauri build`, not `cargo tauri build`.

Every push runs the test suite and a build on both macOS and Windows. Tagging `vX.Y.Z` builds and publishes the installers.

## Contributing

Issues and pull requests are welcome. Two things worth knowing before you open one:

- The code and its comments are written in Russian. Function and variable names too. Pull requests in either language are fine.
- The audio path is covered by tests, and they are expected to stay green. Run `cargo test --workspace` before you push.

## Authors

<!-- TODO: подставить ссылку на Cypher Products, когда будет сайт или страница -->
Built by [Cypher Products](#).

- Mikhail Maksimov — development — [github.com/mmaximov97](https://github.com/mmaximov97)
- Anna Dorogova — design — [adorogova.com](https://adorogova.com) · [github.com/blinbirka](https://github.com/blinbirka)

## Support

MeetRec is free and always will be. If it saved you an hour, you can buy the two of us a coffee.

**USDT, TRON network (TRC-20)**

```
TFzpPkaSRQXLCEg9ZYf4MiwHNbzD4bYCgE
```

Send only on the TRON network. A transfer on any other network cannot be recovered.

## License

MIT. See [LICENSE](LICENSE).
