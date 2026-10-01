use tauri::{AppHandle, Manager};

/// Shared by tray clicks and a second launch, including minimized/hidden windows.
pub fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn single_instance_is_registered_before_other_plugins_and_workers() {
        let source = include_str!("main.rs");
        let builder = source.split("tauri::Builder::default()").nth(1).unwrap();
        let single = builder
            .find(".plugin(tauri_plugin_single_instance::init")
            .unwrap();
        let logging = builder.find("tauri_plugin_log::Builder::new()").unwrap();
        let hotkey = builder
            .find("tauri_plugin_global_shortcut::Builder::new()")
            .unwrap();
        let audio = builder.find("audio::run(").unwrap();
        assert!(single < logging && single < hotkey && single < audio);
        assert!(builder[single..logging].contains("show_main_window(app)"));
        assert!(include_str!("tray.rs").contains("crate::show_main_window(app)"));
    }
}
