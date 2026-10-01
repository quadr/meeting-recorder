//! Native single-instance smoke test; no recording, credentials, hotkeys or tray.
//! cargo run -p meeting-recorder-gui --example single_instance_smoke --target aarch64-pc-windows-msvc
#[path = "../src/window.rs"]
mod window;

use std::{
    path::Path,
    process::Command,
    time::{Duration, Instant},
};
use tauri::Manager;

fn wait_for(path: &Path, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if std::fs::read_to_string(path).ok().as_deref() == Some(expected) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for {expected}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn primary(dir: &Path) {
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = format!(
        "net.meetrec.smoke.{}",
        dir.file_name().unwrap().to_str().unwrap()
    );
    context.config_mut().app.windows.clear();
    let progress = dir.join("progress");
    let setup_count = dir.join("setup-count");
    let setup_progress = progress.clone();
    let mut launches = 0;
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(move |app, _, _| {
            window::show_main_window(app);
            let main = app.get_webview_window("main").unwrap();
            assert!(main.is_visible().unwrap(), "Window stayed hidden");
            assert!(!main.is_minimized().unwrap(), "Window stayed minimized");
            assert!(main.is_focused().unwrap(), "Window was not focused");
            launches += 1;
            if launches == 1 {
                main.minimize().unwrap();
                main.hide().unwrap();
                std::fs::write(&progress, "restored-hidden").unwrap();
            } else {
                std::fs::write(&progress, "restored-minimized").unwrap();
                app.exit(0);
            }
        }))
        .setup(move |app| {
            // A second instance must exit before this setup or any worker runs.
            assert!(!setup_count.exists(), "Second instance initialized the app");
            std::fs::write(&setup_count, "1")?;
            tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External("about:blank".parse().unwrap()),
            )
            .title("MeetRec single-instance smoke test")
            .visible(false)
            .skip_taskbar(true)
            .build()?;
            std::fs::write(&setup_progress, "ready-hidden")?;
            Ok(())
        })
        .build(context)
        .expect("Test app initialization failed")
        .run(|_, _| {});
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.len() == 3 {
        primary(Path::new(&args[2]));
        return;
    }
    let dir = std::env::temp_dir().join(format!("meetrec-instance-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut first = Command::new(&executable)
        .arg("primary")
        .arg(&dir)
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_for(&dir.join("progress"), "ready-hidden");
        for expected in ["restored-hidden", "restored-minimized"] {
            let mut second = Command::new(&executable)
                .arg("secondary")
                .arg(&dir)
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some(status) = second.try_wait().unwrap() {
                    assert!(status.success(), "Second launch did not exit cleanly");
                    break;
                }
                if Instant::now() >= deadline {
                    let _ = second.kill();
                    let _ = second.wait();
                    panic!("Second process did not exit");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            wait_for(&dir.join("progress"), expected);
            if expected == "restored-hidden" {
                assert!(
                    first.try_wait().unwrap().is_none(),
                    "Existing process exited"
                );
            }
        }
        assert!(first.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(dir.join("setup-count")).unwrap(),
            "1"
        );
    }));
    if result.is_err() {
        let _ = first.kill();
        let _ = first.wait();
    }
    // This directory was created by this test and contains only test markers.
    std::fs::remove_dir_all(&dir).unwrap();
    result.unwrap();
    println!("PASS: hidden/minimized window restored and focused; first process preserved; second launch exited before app setup.");
}
