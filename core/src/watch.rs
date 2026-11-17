//! Suivi permanent et léger du jeu et de l'application vocale.
//! Tourne même quand le buffer est arrêté : c'est lui qui permet de ne lancer
//! la capture que pendant une partie.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Applications suivies, lues par la capture audio et l'enregistreur vidéo.
#[derive(Default)]
pub struct Apps {
    /// PID racine de l'application vocale (0 = absente).
    pub voice_pid: AtomicU32,
    /// PID du jeu détecté (0 = aucun jeu).
    pub game_pid: AtomicU32,
    pub game_name: Mutex<String>,
    /// Fenêtre du jeu (0 = aucune).
    pub game_hwnd: Arc<AtomicU64>,
}

impl Apps {
    pub fn game(&self) -> Option<String> {
        if self.game_pid.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(self.game_name.lock().unwrap().clone())
    }

    pub fn voice_detected(&self) -> bool {
        self.voice_pid.load(Ordering::Relaxed) != 0
    }
}

pub struct AppWatcher {
    pub apps: Arc<Apps>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AppWatcher {
    pub fn start(voice_app: &str, game_app: &str) -> AppWatcher {
        let apps = Arc::new(Apps::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (a, s) = (apps.clone(), stop.clone());
        let (voice, game) = (voice_app.to_string(), game_app.to_string());
        let thread = std::thread::spawn(move || watch_apps(a, s, voice, game));
        AppWatcher { apps, stop, thread: Some(thread) }
    }
}

impl Drop for AppWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Applications qui passent en plein écran sans être des jeux.
const NOT_GAMES: &[&str] = &[
    "explorer", "clipo", "dwm", "chrome", "msedge", "firefox", "brave", "opera", "vivaldi", "vlc",
    "spotify", "applicationframehost", "searchhost", "lockapp", "textinputhost", "shellexperiencehost",
    "startmenuexperiencehost", "obs64", "mpc-hc64", "wmplayer", "code", "devenv", "discord",
];

/// Lanceurs de jeux : un programme démarré par l'un d'eux est un jeu, même en fenêtré.
const LAUNCHERS: &[&str] = &[
    "steam", "epicgameslauncher", "upc", "ubisoftconnect", "uplay", "battle.net", "riotclientservices",
    "leagueclient", "eadesktop", "ealauncher", "origin", "galaxyclient", "rockstarservice", "launcher",
    "bethesda.net_launcher", "gamingservices", "xboxpcapp", "playnite.desktopapp", "heroic",
];

/// Programmes annexes des lanceurs (boutique, overlay, anti-triche…), qui ne sont pas des jeux.
const LAUNCHER_PARTS: &[&str] = &[
    "steamwebhelper", "steamservice", "gameoverlayui", "gameoverlayui64", "steamerrorreporter", "epicwebhelper",
    "uplaywebcore", "upc", "ubisoftconnect", "battle.net", "riotclientux", "riotclientuxrender",
    "riotclientcrashhandler", "leagueclientux", "leagueclientuxrender", "eadesktop", "eabackgroundservice",
    "galaxyclient helper", "crashreportclient", "unitycrashhandler64", "easyanticheat", "vgtray", "cefsharp.browsersubprocess",
];

fn stem_of(name: &str) -> String {
    let n = name.to_lowercase();
    n.trim_end_matches(".exe").to_string()
}

/// Suit l'application vocale et le jeu.
/// Un jeu est l'application au premier plan qui a été démarrée par un lanceur
/// (Steam, Ubisoft Connect, Epic, Battle.net, Riot, EA…) ou qui occupe tout l'écran.
/// Il reste « le jeu » tant qu'il tourne, même si l'on passe sur une autre fenêtre.
fn watch_apps(shared: Arc<Apps>, stop: Arc<AtomicBool>, voice_exe: String, game_exe: String) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(mut sys) = System::new() else { return };
    let voice = stem_of(&voice_exe);
    let game = stem_of(&game_exe);
    let auto = game.is_empty() || game == "auto";
    while !stop.load(Ordering::Relaxed) {
        sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
        let name_of = |pid: u32| sys.process(Pid::from_u32(pid)).map(|p| stem_of(&p.name().to_string_lossy()));
        // Racine d'une application : son parent n'est pas la même application.
        let root_of = |want: &str| -> u32 {
            if want.is_empty() {
                return 0;
            }
            for p in sys.processes().values() {
                if stem_of(&p.name().to_string_lossy()) != want {
                    continue;
                }
                let parent_same = p.parent().and_then(|pp| name_of(pp.as_u32())).map(|n| n == want).unwrap_or(false);
                if !parent_same {
                    return p.pid().as_u32();
                }
            }
            0
        };
        let from_launcher = |pid: u32| -> bool {
            let mut cur = sys.process(Pid::from_u32(pid)).and_then(|p| p.parent());
            for _ in 0..6 {
                let Some(pp) = cur else { return false };
                match name_of(pp.as_u32()) {
                    Some(n) if LAUNCHERS.contains(&n.as_str()) => return true,
                    Some(_) => cur = sys.process(pp).and_then(|p| p.parent()),
                    None => return false,
                }
            }
            false
        };
        shared.voice_pid.store(root_of(&voice), Ordering::Relaxed);

        let mut pid = shared.game_pid.load(Ordering::Relaxed);
        let mut hwnd = shared.game_hwnd.load(Ordering::Relaxed);
        if pid != 0 && sys.process(Pid::from_u32(pid)).is_none() {
            pid = 0;
        }
        let front = crate::capture::foreground();
        if auto {
            if let Some(f) = front {
                if let Some(name) = name_of(f.pid) {
                    let excluded = f.pid == std::process::id()
                        || name == voice
                        || NOT_GAMES.contains(&name.as_str())
                        || LAUNCHERS.contains(&name.as_str())
                        || LAUNCHER_PARTS.contains(&name.as_str());
                    if !excluded && (f.fullscreen || from_launcher(f.pid)) {
                        pid = f.pid;
                        hwnd = f.hwnd;
                    }
                }
            }
        } else {
            pid = root_of(&game);
            // La fenêtre du jeu est repérée quand il passe au premier plan.
            if let Some(f) = front {
                let same_app = name_of(f.pid).map(|n| n == game).unwrap_or(false);
                if pid != 0 && same_app {
                    hwnd = f.hwnd;
                }
            }
        }
        if pid == 0 || (hwnd != 0 && !crate::capture::window_alive(hwnd)) {
            hwnd = 0;
        }
        if pid != 0 {
            if let Some(p) = sys.process(Pid::from_u32(pid)) {
                *shared.game_name.lock().unwrap() = p.name().to_string_lossy().into_owned();
            }
        }
        shared.game_pid.store(pid, Ordering::Relaxed);
        shared.game_hwnd.store(hwnd, Ordering::Relaxed);
        for _ in 0..20 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    shared.game_pid.store(0, Ordering::Relaxed);
    shared.voice_pid.store(0, Ordering::Relaxed);
    shared.game_hwnd.store(0, Ordering::Relaxed);
}

