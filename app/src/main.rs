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
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

struct AppState {
    engine: Mutex<Engine>,
    saving: AtomicBool,
    overlay_seq: AtomicU32,
    /// Raccourci d'activation du buffer, lisible sans verrouiller le moteur.
    toggle_key: Mutex<String>,
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

/// Joue un fichier .wav (bloquant : à appeler hors du fil principal).
#[cfg(windows)]
fn play_wav(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "winmm")]
    extern "system" {
        fn PlaySoundW(sound: *const u16, module: isize, flags: u32) -> i32;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    // SND_FILENAME | SND_NODEFAULT (lecture synchrone)
    unsafe {
        PlaySoundW(wide.as_ptr(), 0, 0x0002_0000 | 0x0002);
    }
}

#[cfg(not(windows))]
fn play_wav(_path: &Path) {}

/// Signal de confirmation : fichier choisi par l'utilisateur, sinon son intégré.
fn beep(app: &AppHandle, ok: bool) {
    let cfg = app.state::<AppState>().engine.lock().unwrap().cfg.clone();
    if !cfg.sound_enabled {
        return;
    }
    let (built_ok, built_err) = clipo_core::sound::install(&clipo_core::config::data_dir().join("sons"));
    let custom = PathBuf::from(&cfg.sound_file);
    let file = if !ok {
        built_err
    } else if !cfg.sound_file.is_empty() && custom.exists() {
        custom
    } else {
        built_ok
    };
    play_wav(&file);
}

/// Note brève à l'activation ou à la désactivation du buffer.
fn blip(app: &AppHandle, on: bool) {
    if !app.state::<AppState>().engine.lock().unwrap().cfg.sound_enabled {
        return;
    }
    let (file_on, file_off) = clipo_core::sound::install_toggle(&clipo_core::config::data_dir().join("sons"));
    play_wav(if on { &file_on } else { &file_off });
}

/// Allume ou éteint le buffer (raccourci, menu, pastille), sans bloquer l'appelant.
fn toggle_buffer(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let mut cfg = app.state::<AppState>().engine.lock().unwrap().cfg.clone();
        cfg.buffer_enabled = !cfg.buffer_enabled;
        let on = cfg.buffer_enabled;
        match apply_config(&app, cfg) {
            Ok(_) => blip(&app, on),
            Err(e) => {
                beep(&app, false);
                let _ = app.emit("clip-error", e);
            }
        }
    });
}

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
                beep(&app, true);
                let _ = app.emit("clip-saved", &info);
            }
            Err(e) => {
                beep(&app, false);
                let _ = app.emit("clip-error", e);
            }
        }
    });
}

/// Enregistre les raccourcis globaux : clip, et activation du buffer (facultatif).
fn register_hotkeys(app: &AppHandle, clip: &str, toggle: &str) -> Result<(), String> {
    let shortcuts = app.global_shortcut();
    let _ = shortcuts.unregister_all();
    *app.state::<AppState>().toggle_key.lock().unwrap() = String::new();
    shortcuts
        .register(clip)
        .map_err(|e| format!("Raccourci « {clip} » indisponible : {e}"))?;
    if !toggle.is_empty() {
        shortcuts
            .register(toggle)
            .map_err(|e| format!("Raccourci « {toggle} » indisponible : {e}"))?;
        *app.state::<AppState>().toggle_key.lock().unwrap() = toggle.to_string();
    }
    Ok(())
}

/// Le raccourci pressé est-il celui qui allume ou éteint le buffer ?
fn is_toggle_key(app: &AppHandle, pressed: &Shortcut) -> bool {
    let toggle = app.state::<AppState>().toggle_key.lock().unwrap().clone();
    !toggle.is_empty() && toggle.parse::<Shortcut>().map(|s| s.id() == pressed.id()).unwrap_or(false)
}

fn apply_autostart(app: &AppHandle, on: bool) {
    let manager = app.autolaunch();
    let _ = if on { manager.enable() } else { manager.disable() };
}

/// Applique une configuration : moteur, raccourci, démarrage automatique.
fn apply_config(app: &AppHandle, cfg: Config) -> Result<Config, String> {
    let state = app.state::<AppState>();
    let (cfg, old, engine_result) = {
        let mut engine = state.engine.lock().unwrap();
        let old = (engine.cfg.hotkey.clone(), engine.cfg.toggle_hotkey.clone());
        let result = engine.set_config(cfg);
        (engine.cfg.clone(), old, result)
    };
    apply_autostart(app, cfg.autostart);
    let mut hotkey_result = Ok(());
    if (cfg.hotkey.clone(), cfg.toggle_hotkey.clone()) != old {
        hotkey_result = register_hotkeys(app, &cfg.hotkey, &cfg.toggle_hotkey);
        if hotkey_result.is_err() {
            // On garde les anciens raccourcis plutôt que de laisser l'app sans raccourci.
            let _ = register_hotkeys(app, &old.0, &old.1);
            let mut engine = state.engine.lock().unwrap();
            let mut fixed = engine.cfg.clone();
            fixed.hotkey = old.0;
            fixed.toggle_hotkey = old.1;
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

#[tauri::command]
fn toggle_buffer_now(app: AppHandle) {
    toggle_buffer(&app);
}

#[tauri::command(async)]
fn list_clips(state: State<AppState>) -> Vec<ClipInfo> {
    let (ffmpeg, dir, extra) = {
        let e = state.engine.lock().unwrap();
        (e.ffmpeg.clone(), PathBuf::from(&e.cfg.clips_dir), e.cfg.extra_dirs.clone())
    };
    clip::list_clips(&ffmpeg, &dir, &extra)
}

/// Prépare un clip pour l'éditeur (sépare les pistes d'un fichier venu d'un autre logiciel).
#[tauri::command(async)]
fn prepare_clip(state: State<AppState>, path: String) -> Result<ClipInfo, String> {
    let ffmpeg = state.engine.lock().unwrap().ffmpeg.clone();
    clip::prepare_clip(&ffmpeg, Path::new(&path))
}

/// Forme d'onde d'une piste audio (50 points par seconde, 0-255).
#[tauri::command(async)]
fn waveform(state: State<AppState>, path: String, stream: u32) -> Result<Vec<u8>, String> {
    let ffmpeg = state.engine.lock().unwrap().ffmpeg.clone();
    ffmpeg::waveform(&ffmpeg, Path::new(&path), stream, 50)
}

#[tauri::command(async)]
fn get_diagnostics(state: State<AppState>) -> String {
    state.engine.lock().unwrap().diagnostics()
}

#[tauri::command(async)]
fn test_sound(app: AppHandle) {
    beep(&app, true);
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

/// Chemin de sortie d'un export : « share » = fichier temporaire à partager
/// (effacé au prochain lancement), « copy » = nouvelle copie dans le dossier des clips.
#[tauri::command(async)]
fn plan_output(state: State<AppState>, kind: String, name: String) -> Result<String, String> {
    let e = state.engine.lock().unwrap();
    let path = if kind == "share" {
        let dir = e.log_dir().join("partage");
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        dir.join(format!("{name}.mp4"))
    } else {
        let dir = PathBuf::from(&e.cfg.clips_dir);
        std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        clip::copy_path(&dir, &name)
    };
    Ok(path.to_string_lossy().into_owned())
}

#[tauri::command(async)]
fn file_size(path: String) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[tauri::command(async)]
fn list_mics() -> Vec<String> {
    clipo_core::capture::list_mics()
}

// ───────────────────────────── Démarrage ─────────────────────────────

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Ouvrir Clipo", true, None::<&str>)?;
    let clip = MenuItem::with_id(app, "clip", "Enregistrer un clip", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Activer / désactiver le buffer", true, None::<&str>)?;
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
            "toggle" => toggle_buffer(app),
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
        .plugin(tauri_plugin_drag::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    if is_toggle_key(app, shortcut) {
                        toggle_buffer(app);
                    } else {
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
            toggle_buffer_now,
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
            list_mics,
            prepare_clip,
            waveform,
            get_diagnostics,
            test_sound,
            plan_output,
            file_size
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
            let (hotkey, toggle) = (cfg.hotkey.clone(), cfg.toggle_hotkey.clone());
            app.manage(AppState {
                engine: Mutex::new(Engine::new(cfg, ffmpeg)),
                saving: AtomicBool::new(false),
                overlay_seq: AtomicU32::new(0),
                toggle_key: Mutex::new(String::new()),
            });
            build_tray(&handle)?;
            if let Err(e) = register_hotkeys(&handle, &hotkey, &toggle) {
                eprintln!("[clipo] {e}");
            }
            // Surveillance : démarre le buffer quand un jeu se lance (selon le mode),
            // l'arrête après sa fermeture, et bascule sur la capture de secours si
            // la capture légère échoue. Le premier passage est immédiat ; la détection
            // de l'encodeur prend une à deux secondes, hors du fil principal.
            let h = handle.clone();
            std::thread::spawn(move || loop {
                let state = h.state::<AppState>();
                let busy = state.saving.load(Ordering::SeqCst);
                let changed = state.engine.lock().unwrap().maintain(busy);
                if changed {
                    let _ = h.emit("status-changed", ());
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            });
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
