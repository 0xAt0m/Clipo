//! Pistes audio : chaque source (jeu, vocal, micro) écrit en continu dans un
//! tampon circulaire sur disque, calé sur une horloge commune. Au moment du
//! clip, on relit exactement la fenêtre de temps de la vidéo.

use crate::config::Config;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

pub const RATE: u64 = 48_000;
/// Octets par trame : 16 bits stéréo.
pub const FRAME: usize = 4;
/// Écart toléré avant de combler par du silence (50 ms).
const GAP_TOL: u64 = RATE / 20;
/// Avance tolérée avant d'ignorer un paquet (200 ms).
const LEAD_TOL: u64 = RATE / 5;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Game,
    Voice,
    Mic,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Kind::Game => "Jeu",
            Kind::Voice => "Vocal",
            Kind::Mic => "Micro",
        }
    }
}

struct Ring {
    file: File,
    /// Capacité en trames.
    cap: u64,
    /// Nombre total de trames écrites depuis l'origine (position sur la ligne de temps).
    written: u64,
}

impl Ring {
    fn write_at(&mut self, frame: u64, data: &[u8]) -> std::io::Result<()> {
        let mut pos = frame % self.cap;
        let mut rest = data;
        while !rest.is_empty() {
            let room = ((self.cap - pos) as usize) * FRAME;
            let n = rest.len().min(room);
            self.file.seek(SeekFrom::Start(pos * FRAME as u64))?;
            self.file.write_all(&rest[..n])?;
            rest = &rest[n..];
            pos = 0;
        }
        Ok(())
    }

    fn silence_until(&mut self, frame: u64) {
        let start = self.written.max(frame.saturating_sub(self.cap));
        let zeros = vec![0u8; 16_384 * FRAME];
        let mut f = start;
        while f < frame {
            let n = ((frame - f) as usize).min(16_384);
            let _ = self.write_at(f, &zeros[..n * FRAME]);
            f += n as u64;
        }
        self.written = frame;
    }
}

pub struct Track {
    pub kind: Kind,
    epoch: Instant,
    ring: Mutex<Ring>,
    /// Vrai dès qu'une source alimente réellement la piste.
    pub active: AtomicBool,
}

impl Track {
    fn new(kind: Kind, epoch: Instant, path: &Path, seconds: u64) -> std::io::Result<Track> {
        let cap = seconds * RATE;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)?;
        file.set_len(cap * FRAME as u64)?;
        Ok(Track {
            kind,
            epoch,
            ring: Mutex::new(Ring { file, cap, written: 0 }),
            active: AtomicBool::new(false),
        })
    }

    fn frames_at(&self, t: Instant) -> u64 {
        (t.saturating_duration_since(self.epoch).as_micros() as u64) * RATE / 1_000_000
    }

    /// Ajoute un paquet PCM (16 bits stéréo 48 kHz) reçu à l'instant `arrival`.
    pub fn push(&self, data: &[u8], arrival: Instant) {
        let n = (data.len() / FRAME) as u64;
        if n == 0 {
            return;
        }
        let end = self.frames_at(arrival);
        let start = end.saturating_sub(n);
        let mut r = self.ring.lock().unwrap();
        if start > r.written + GAP_TOL {
            r.silence_until(start);
        } else if r.written > start + LEAD_TOL {
            return;
        }
        let at = r.written;
        if r.write_at(at, &data[..(n as usize) * FRAME]).is_ok() {
            r.written += n;
        }
    }

    /// Lit `count` trames à partir de la trame `start` ; ce qui n'existe pas est du silence.
    pub fn read(&self, start: i64, count: u64) -> Vec<u8> {
        let mut out = vec![0u8; count as usize * FRAME];
        let mut r = self.ring.lock().unwrap();
        let lo = r.written.saturating_sub(r.cap) as i64;
        let hi = r.written as i64;
        let a = start.max(lo);
        let b = (start + count as i64).min(hi);
        if b <= a {
            return out;
        }
        let mut f = a as u64;
        let end = b as u64;
        while f < end {
            let pos = f % r.cap;
            let n = (end - f).min(r.cap - pos);
            let off = ((f as i64 - start) as usize) * FRAME;
            let len = n as usize * FRAME;
            if r.file.seek(SeekFrom::Start(pos * FRAME as u64)).is_err()
                || r.file.read_exact(&mut out[off..off + len]).is_err()
            {
                break;
            }
            f += n;
        }
        out
    }
}

/// État partagé entre les fils de capture.
pub struct Shared {
    pub stop: AtomicBool,
    /// PID racine de l'application vocale (0 = absente).
    pub voice_pid: AtomicU32,
    /// PID du jeu isolé (0 = aucun jeu détecté : la piste Jeu prend alors tout sauf le vocal).
    pub game_pid: AtomicU32,
    pub game_name: Mutex<String>,
    /// Fenêtre du jeu (0 = aucune), partagée avec l'enregistreur vidéo.
    pub game_hwnd: Arc<AtomicU64>,
    pub mic: String,
}

pub struct AudioEngine {
    epoch: Instant,
    epoch_sys: SystemTime,
    pub tracks: Vec<Arc<Track>>,
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    files: Vec<PathBuf>,
}

impl AudioEngine {
    pub fn start(cfg: &Config, dir: &Path, game_hwnd: Arc<AtomicU64>) -> Result<AudioEngine, String> {
        let epoch = Instant::now();
        let epoch_sys = SystemTime::now();
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            voice_pid: AtomicU32::new(0),
            game_pid: AtomicU32::new(0),
            game_name: Mutex::new(String::new()),
            game_hwnd,
            mic: cfg.mic.clone(),
        });
        let mut kinds = Vec::new();
        if cfg.record_game {
            kinds.push(Kind::Game);
        }
        if cfg.record_voice {
            kinds.push(Kind::Voice);
        }
        if cfg.record_mic && cfg.mic != "none" {
            kinds.push(Kind::Mic);
        }
        let seconds = cfg.buffer_seconds as u64 + 30;
        let mut tracks = Vec::new();
        let mut threads = Vec::new();
        let mut files = Vec::new();

        // Surveille la présence de l'application vocale.
        {
            let shared = shared.clone();
            let (voice, game) = (cfg.voice_app.clone(), cfg.game_app.clone());
            threads.push(std::thread::spawn(move || watch_apps(shared, voice, game)));
        }

        for (i, kind) in kinds.into_iter().enumerate() {
            let path = dir.join(format!("audio{i}.pcm"));
            let track = Arc::new(
                Track::new(kind, epoch, &path, seconds).map_err(|e| format!("tampon audio : {e}"))?,
            );
            files.push(path);
            tracks.push(track.clone());
            let shared = shared.clone();
            threads.push(std::thread::spawn(move || {
                crate::capture::capture_loop(track, shared)
            }));
        }
        Ok(AudioEngine { epoch, epoch_sys, tracks, shared, threads, files })
    }

    pub fn voice_detected(&self) -> bool {
        self.shared.voice_pid.load(Ordering::Relaxed) != 0
    }

    /// Nom du jeu actuellement isolé sur la piste Jeu.
    pub fn game(&self) -> Option<String> {
        if self.shared.game_pid.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some(self.shared.game_name.lock().unwrap().clone())
    }

    /// Accès en lecture, utilisable sans verrouiller le moteur.
    pub fn reader(&self) -> AudioReader {
        AudioReader { epoch_sys: self.epoch_sys, tracks: self.tracks.clone() }
    }

    pub fn stop(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let _ = self.epoch;
        self.tracks.clear();
        for f in &self.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

#[derive(Clone)]
pub struct AudioReader {
    epoch_sys: SystemTime,
    pub tracks: Vec<Arc<Track>>,
}

impl AudioReader {
    /// Extrait la fenêtre [start, end] d'une piste et l'écrit en WAV.
    pub fn write_wav(
        &self,
        track: usize,
        start: SystemTime,
        end: SystemTime,
        offset_ms: i32,
        out: &Path,
    ) -> Result<(), String> {
        let to_frames = |t: SystemTime| -> i64 {
            let us = match t.duration_since(self.epoch_sys) {
                Ok(d) => d.as_micros() as i64,
                Err(e) => -(e.duration().as_micros() as i64),
            };
            (us - offset_ms as i64 * 1000) * RATE as i64 / 1_000_000
        };
        let a = to_frames(start);
        let b = to_frames(end).max(a);
        let data = self.tracks[track].read(a, (b - a) as u64);
        let mut f = File::create(out).map_err(|e| e.to_string())?;
        let len = data.len() as u32;
        let mut h = Vec::with_capacity(44);
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(36 + len).to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&(RATE as u32).to_le_bytes());
        h.extend_from_slice(&(RATE as u32 * FRAME as u32).to_le_bytes());
        h.extend_from_slice(&(FRAME as u16).to_le_bytes());
        h.extend_from_slice(&16u16.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&len.to_le_bytes());
        f.write_all(&h).and_then(|_| f.write_all(&data)).map_err(|e| e.to_string())
    }
}

/// Attente interruptible.
pub fn nap(shared: &Shared, total: Duration) {
    let step = Duration::from_millis(100);
    let mut left = total;
    while !shared.stop.load(Ordering::Relaxed) && !left.is_zero() {
        let d = left.min(step);
        std::thread::sleep(d);
        left -= d;
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
fn watch_apps(shared: Arc<Shared>, voice_exe: String, game_exe: String) {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let Ok(mut sys) = System::new() else { return };
    let voice = stem_of(&voice_exe);
    let game = stem_of(&game_exe);
    let auto = game.is_empty() || game == "auto";
    while !shared.stop.load(Ordering::Relaxed) {
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
        nap(&shared, Duration::from_secs(2));
    }
    shared.game_hwnd.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(frames: usize, value: i16) -> Vec<u8> {
        let mut v = Vec::with_capacity(frames * FRAME);
        for _ in 0..frames * 2 {
            v.extend_from_slice(&value.to_le_bytes());
        }
        v
    }

    fn sample(buf: &[u8], frame: usize) -> i16 {
        i16::from_le_bytes([buf[frame * FRAME], buf[frame * FRAME + 1]])
    }

    #[test]
    fn silence_fills_gaps_and_ring_wraps() {
        let dir = std::env::temp_dir().join(format!("clipo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let epoch = Instant::now();
        let t = Track::new(Kind::Game, epoch, &dir.join("a.pcm"), 2).unwrap();
        // 10 ms de son arrivant à t = 10 ms : trames 0..480.
        t.push(&pcm(480, 1000), epoch + Duration::from_millis(10));
        // Paquet contigu, légèrement en retard : pas de trou.
        t.push(&pcm(480, 2000), epoch + Duration::from_millis(25));
        // Long silence puis reprise à t = 1 s.
        t.push(&pcm(480, 3000), epoch + Duration::from_millis(1000));
        let all = t.read(0, 48_000);
        assert_eq!(sample(&all, 0), 1000);
        assert_eq!(sample(&all, 479), 1000);
        assert_eq!(sample(&all, 480), 2000);
        assert_eq!(sample(&all, 959), 2000);
        assert_eq!(sample(&all, 960), 0);
        assert_eq!(sample(&all, 47_519), 0);
        assert_eq!(sample(&all, 47_520), 3000);
        assert_eq!(sample(&all, 47_999), 3000);
        // Au-delà de la capacité (2 s) : les vieilles données sont remplacées.
        t.push(&pcm(480, 4000), epoch + Duration::from_millis(2500));
        let late = t.read(119_520, 480);
        assert_eq!(sample(&late, 0), 4000);
        let gone = t.read(0, 480);
        assert_eq!(sample(&gone, 0), 0, "hors fenêtre = silence");
        // Lecture avant l'origine et après la fin : silence, sans panique.
        let edge = t.read(-100, 200);
        assert_eq!(edge.len(), 200 * FRAME);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
