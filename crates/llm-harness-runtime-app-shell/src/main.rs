#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use llm_harness_runtime_app_shell::core_process::{CoreProcess, CoreProcessConfig};
use parking_lot::Mutex;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tauri::Manager;

#[derive(Clone, Default)]
struct ShellState {
    core_process: Arc<Mutex<Option<CoreProcess>>>,
    shutdown_requested: Arc<AtomicBool>,
}

fn main() -> ExitCode {
    let config = match CoreProcessConfig::from_env() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("llm-harness-app-shell: {error}");
            return ExitCode::from(2);
        }
    };
    let state = ShellState::default();

    let run_result = tauri::Builder::default()
        .manage(state)
        .setup(move |app| {
            let app_handle = app.handle().clone();
            let state = app.state::<ShellState>().inner().clone();
            thread::spawn(move || {
                let error_handle = app_handle.clone();
                if let Err(error) = start_core_and_window(app_handle, state, config) {
                    eprintln!("llm-harness-app-shell: {error}");
                    error_handle.exit(1);
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() != "main" {
                return;
            }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let app_handle = window.app_handle().clone();
                let state = window.app_handle().state::<ShellState>().inner().clone();
                if state.shutdown_requested.swap(true, Ordering::SeqCst) {
                    return;
                }
                thread::spawn(move || {
                    let core_process = state.core_process.lock().take();
                    if let Some(core_process) = core_process {
                        core_process.shutdown();
                    }
                    if let Some(window) = app_handle.get_webview_window("main") {
                        let _ = window.destroy();
                    }
                    app_handle.exit(0);
                });
            }
        })
        .run(tauri::generate_context!());

    match run_result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("llm-harness-app-shell: {error}");
            ExitCode::from(1)
        }
    }
}

fn start_core_and_window(
    app_handle: tauri::AppHandle,
    state: ShellState,
    config: CoreProcessConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = resolve_core_config(app_handle.clone(), config)?;
    {
        let mut slot = state.core_process.lock();
        if state.shutdown_requested.load(Ordering::SeqCst) {
            return Ok(());
        }
        let core_process = CoreProcess::spawn(config)?;
        let panel_url = core_process.panel_url().clone();
        *slot = Some(core_process);
        let panel_url = panel_url.as_str().parse::<tauri::Url>()?;
        tauri::WebviewWindowBuilder::new(
            &app_handle,
            "main",
            tauri::WebviewUrl::External(panel_url),
        )
        .title("LLM Harness Runtime")
        .inner_size(1280.0, 800.0)
        .min_inner_size(960.0, 640.0)
        .build()?;
    };
    Ok(())
}

fn resolve_core_config(
    app_handle: tauri::AppHandle,
    mut config: CoreProcessConfig,
) -> Result<CoreProcessConfig, Box<dyn std::error::Error>> {
    if config.data_root.is_none() {
        config.data_root = Some(app_handle.path().app_data_dir()?);
    }
    Ok(config)
}
