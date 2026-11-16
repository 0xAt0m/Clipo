// Clipo — client en tâche de fond (zone de notification) + fenêtre à la demande.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use base64::Engine as _;
use clipo_core::clip::{self, ClipInfo};
use clipo_core::config::Config;
use clipo_core::export::{self, Project};
use clipo_core::ffmpeg::{self, MediaInfo};
use clipo_core::{Engine, Status};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::path::BaseDirectory;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

struct AppState {
    engine: Mutex<Engine>,
    saving: AtomicBool,
    overlay_seq: AtomicU32,
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    // La fenêtre est détruite à la fermeture : aucun moteur web ne tourne en fond.
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Clipo")
        .inner_size(1240.0, 800.0)
        .min_inner_size(960.0, 620.0)
        .build();
}

#[cfg(windows)]
fn beep(ok: bool) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBeep(kind: u32) -> i32;
    }
    // MB_ICONASTERISK / MB_ICONHAND
    unsafe {
        MessageBeep(if ok { 0x40 } else { 0x10 });
    }
}

#[cfg(not(windows))]
fn beep(_ok: bool) {}

/// Sauve le contenu du buffer dans un clip, sans bloquer l'interface.
fn save_clip(app: &AppHandle) {
    let state = app.state::<AppState>();
    if state.saving.swap(true, Ordering::SeqCst) {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        let state = app.state::<AppState>();
        let job = state.engine.lock().unwrap().clip_job();
        let result = job.and_then(|j| j.run());
        state.saving.store(false, Ordering::SeqCst);
        match result {
            Ok(info) => {
                beep(true);
                let _ = app.emit("clip-saved", &info);
            }
            Err(e) => {
                beep(false);
                let _ = app.emit("clip-error", e);
            }
        }
    });
}

fn register_hotkey(app: &AppHandle, hotkey: &str) -> Result<(), String> {
    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister_all();
    shortcuts
        .register(hotkey)
        .map_err(|e| format!("Raccourci « {hotkey} » indisponible : {e}"))
}

fn apply_autostart(app: &AppHandle, on: bool) {
    let manager = app.autolaunch();
    let _ = if on { manager.enable() } else { manager.disable() };
}

/// Applique une configuration : moteur, raccourci, démarrage automatique.
fn apply_config(app: &AppHandle, cfg: Config) -> Result<Config, String> {
    let state = app.state::<AppState>();
    let (cfg, old_hotkey, engine_result) = {
        let mut engine = state.engine.lock().unwrap();
        let old_hotkey = engine.cfg.hotkey.clone();
        let result = engine.set_config(cfg);
        (engine.cfg.clone(), old_hotkey, result)
    };
    apply_autostart(app, cfg.autostart);
    let mut hotkey_result = Ok(());
    if cfg.hotkey != old_hotkey {
        hotkey_result = register_hotkey(app, &cfg.hotkey);
        if hotkey_result.is_err() {
            // On garde l'ancien raccourci plutôt que de laisser l'app sans raccourci.
            let _ = register_hotkey(app, &old_hotkey);
            let mut engine = state.engine.lock().unwrap();
            let mut fixed = engine.cfg.clone();
            fixed.hotkey = old_hotkey;
            let _ = engine.set_config(fixed);
        }
    }
    let _ = app.emit("status-changed", ());
    engine_result?;
    hotkey_result?;
    let current = state.engine.lock().unwrap().cfg.clone();
    Ok(current)
}

// ───────────────────────────── Commandes ─────────────────────────────

#[tauri::command]
fn get_config(state: State<AppState>) -> Config {
    state.engine.lock().unwrap().cfg.clone()
}

#[tauri::command(async)]
fn set_config(app: AppHandle, cfg: Config) -> Result<Config, String> {
    apply_config(&app, cfg)
}

#[tauri::command(async)]
fn get_status(state: State<AppState>) -> Status {
    state.engine.lock().unwrap().status()
}

#[tauri::command]
fn save_clip_now(app: AppHandle) {
    save_clip(&app);
}

#[tauri::command(async)]
fn list_clips(state: State<AppState>) -> Vec<ClipInfo> {
    let (ffmpeg, dir) = {
        let e = state.engine.lock().unwrap();
        (e.ffmpeg.clone(), PathBuf::from(&e.cfg.clips_dir))
    };
    clip::list_clips(&ffmpeg, &dir)
}

#[tauri::command(async)]
fn delete_clip(path: String) -> Result<(), String> {
    clip::delete_clip(Path::new(&path))
}

#[tauri::command(async)]
fn rename_clip(path: String, name: String) -> Result<ClipInfo, String> {
    clip::rename_clip(Path::new(&path), &name)
}

#[tauri::command]
fn reveal(path: String) {
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg("/select,").arg(&path).spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = path;
    }
}

#[tauri::command(async)]
fn load_project(clip_path: String) -> Option<String> {
    std::fs::read_to_string(clip::sidecar_dir(Path::new(&clip_path)).join("project.json")).ok()
}

#[tauri::command(async)]
fn save_project(clip_path: String, json: String) -> Result<(), String> {
    let dir = clip::sidecar_dir(Path::new(&clip_path));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join("project.json"), json).map_err(|e| e.to_string())
}

#[tauri::command(async)]
fn probe_media(state: State<AppState>, path: String) -> Result<MediaInfo, String> {
    let ffmpeg = state.engine.lock().unwrap().ffmpeg.clone();
    ffmpeg::probe(&ffmpeg, Path::new(&path))
}

/// Lit une image du disque sous forme d'URL de données (pour les calques).
#[tauri::command(async)]
fn read_data_url(path: String) -> Result<String, String> {
    let bytes = std::fs::read(&path).map_err(|e| format!("Image illisible : {e}"))?;
    if bytes.len() > 40 * 1024 * 1024 {
        return Err("Image trop volumineuse (40 Mo maximum).".into());
    }
    let lower = path.to_lowercase();
    let mime = if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        "image/jpeg"
    };
    Ok(format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes)))
}

/// Écrit un calque (PNG rendu par l'interface) dans un fichier temporaire.
#[tauri::command(async)]
fn write_overlay(state: State<AppState>, data_url: String) -> Result<String, String> {
    let b64 = data_url.split_once(',').map(|(_, b)| b).ok_or("Calque invalide.")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("Calque invalide : {e}"))?;
    let dir = state.engine.lock().unwrap().log_dir().join("overlays");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let n = state.overlay_seq.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!("calque_{n}.png"));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn export_project(app: AppHandle, state: State<AppState>, project: Project) -> Result<String, String> {
    let (ffmpeg, log) = {
        let e = state.engine.lock().unwrap();
        (e.ffmpeg.clone(), e.log_dir().join("export.log"))
    };
    let result = export::run(&ffmpeg, &project, &log, &|p| {
        let _ = app.emit("export-progress", p);
    });
    let _ = std::fs::remove_dir_all(state.engine.lock().unwrap().log_dir().join("overlays"));
    result.map(|p| p.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn list_mics() -> Vec<String> {
    clipo_core::capture::list_mics()
}

// ───────────────────────────── Démarrage ─────────────────────────────

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Ouvrir Clipo", true, None::<&str>)?;
    let clip = MenuItem::with_id(app, "clip", "Enregistrer un clip", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Mettre en pause / reprendre le buffer", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open, &clip, &toggle, &sep, &quit])?;
    let mut tray = TrayIconBuilder::with_id("clipo")
        .tooltip("Clipo")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "clip" => save_clip(app),
            "toggle" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let mut cfg = app.state::<AppState>().engine.lock().unwrap().cfg.clone();
                    cfg.buffer_enabled = !cfg.buffer_enabled;
                    let _ = apply_config(&app, cfg);
                });
            }
            "quit" => {
                app.state::<AppState>().engine.lock().unwrap().stop_buffer();
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        save_clip(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            get_config,
            set_config,
            get_status,
            save_clip_now,
            list_clips,
            delete_clip,
            rename_clip,
            reveal,
            load_project,
            save_project,
            probe_media,
            read_data_url,
            write_overlay,
            export_project,
            list_mics
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            // ffmpeg est livré avec l'application ; à défaut, celui du PATH.
            let bundled = handle
                .path()
                .resolve("resources/ffmpeg.exe", BaseDirectory::Resource)
                .ok()
                .filter(|p| p.exists());
            let ffmpeg = bundled.unwrap_or_else(|| PathBuf::from("ffmpeg"));
            let cfg = Config::load().sanitized();
            let hotkey = cfg.hotkey.clone();
            let start = cfg.buffer_enabled;
            app.manage(AppState {
                engine: Mutex::new(Engine::new(cfg, ffmpeg)),
                saving: AtomicBool::new(false),
                overlay_seq: AtomicU32::new(0),
            });
            build_tray(&handle)?;
            if let Err(e) = register_hotkey(&handle, &hotkey) {
                eprintln!("[clipo] {e}");
            }
            if start {
                // La détection de l'encodeur prend une à deux secondes : hors du fil principal.
                let h = handle.clone();
                std::thread::spawn(move || {
                    let result = h.state::<AppState>().engine.lock().unwrap().start_buffer();
                    if let Err(e) = result {
                        let _ = h.emit("clip-error", e);
                    }
                    let _ = h.emit("status-changed", ());
                });
            }
            if !std::env::args().any(|a| a == "--minimized") {
                show_main(&handle);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("erreur au démarrage de Clipo");

    app.run(|app, event| match event {
        // Fermer la fenêtre ne quitte pas : Clipo reste dans la zone de notification.
        RunEvent::ExitRequested { api, code, .. } => {
            if code.is_none() {
                api.prevent_exit();
            }
        }
        RunEvent::Exit => {
            app.state::<AppState>().engine.lock().unwrap().stop_buffer();
        }
        _ => {}
    });
}
