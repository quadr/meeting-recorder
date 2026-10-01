// Слабая линковка CoreAudio — та же и по той же причине, что в корневом
// `build.rs`: без неё символы Process Tap на macOS старее 14.4 убивают процесс в
// dyld до `main`, и guard версии из `src/capture/macos.rs` не успевает сказать
// человеку, что нужна 14.4. Директива не наследуется от крейта-зависимости:
// линк у этого бинаря свой, значит и флаг нужен свой.
//
// Сторож этой строки — тест `gui_крейт_линкует_coreaudio_слабо` в
// `src-tauri/src/main.rs`.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
    }
    tauri_build::build();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // tauri-build embeds resources in binaries, not examples. The native
        // smoke example also needs Common Controls v6 (TaskDialogIndirect).
        let manifest = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap())
            .join("examples/smoke.manifest");
        println!("cargo:rerun-if-changed={}", manifest.display());
        println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-examples=/MANIFESTINPUT:{}", manifest.display());
    }
}
