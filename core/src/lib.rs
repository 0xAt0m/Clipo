//! Moteur de Clipo, indépendant de l'interface.

pub mod audio;
pub mod capture;
pub mod clip;
pub mod config;
pub mod export;
pub mod ffmpeg;
pub mod recorder;
pub mod sound;
pub mod watch;

use audio::AudioEngine;
use clip::ClipJob;
use config::Config;
use ffmpeg::Pipeline;
use recorder::Recorder;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use watch::AppWatcher;

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub buffering: bool,
    /// "game", "always" ou "manual".
    pub mode: String,
    /// Mode « jeu » : le buffer attend qu'un jeu soit lancé.
    pub waiting_game: bool,
    pub ffmpeg_ok: bool,
    pub encoder: Option<String>,
    pub voice_detected: bool,
    /// Le buffer vidéo n'arrive pas à démarrer (écran inaccessible, pilote…).
    pub video_trouble: bool,
    pub tracks: Vec<String>,
    /// Jeu isolé sur la piste Jeu (None = tout le son du PC sauf le vocal).
    pub game: Option<String>,
    /// Dernier message d'erreur de la capture, pour le diagnostic.
    pub error: Option<String>,
}

pub struct Engine {
    pub cfg: Config,
    pub ffmpeg: PathBuf,
    pipeline: Option<Pipeline>,
    ffmpeg_ok: bool,
    recorder: Option<Recorder>,
    audio: Option<AudioEngine>,
    tmp: PathBuf,
    start_error: Option<String>,
    watcher: AppWatcher,
    /// Mode « jeu » : moment où le jeu a disparu (l'arrêt attend quelques secondes).
    game_gone: Option<Instant>,
}

/// Délai avant d'arrêter le buffer quand le jeu se ferme : un clip demandé
/// juste avant la fermeture a le temps d'être écrit.
const GAME_GRACE: Duration = Duration::from_secs(10);

impl Engine {
    pub fn new(mut cfg: Config, ffmpeg: PathBuf) -> Engine {
        // Mode « au raccourci » : le buffer repart éteint à chaque lancement.
        if cfg.buffer_mode == "manual" {
            cfg.buffer_enabled = false;
        }
        let tmp = std::env::temp_dir().join("Clipo");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let ffmpeg_ok = ffmpeg::is_available(&ffmpeg);
        let _ = std::fs::create_dir_all(&cfg.clips_dir);
        let watcher = AppWatcher::start(&cfg.voice_app, &cfg.game_app);
        Engine { cfg, ffmpeg, pipeline: None, ffmpeg_ok, recorder: None, audio: None, tmp, start_error: None, watcher, game_gone: None }
    }

    pub fn log_dir(&self) -> PathBuf {
        self.tmp.clone()
    }

    pub fn start_buffer(&mut self) -> Result<(), String> {
        if self.recorder.is_some() {
            return Ok(());
        }
        if !self.ffmpeg_ok {
            return Err("ffmpeg est introuvable : réinstallez Clipo.".into());
        }
        let _ = std::fs::create_dir_all(&self.tmp);
        let pipe = match self.pipeline {
            Some(p) => p,
            None => {
                let log = self.tmp.join("detect.log");
                match ffmpeg::detect_pipeline(&self.ffmpeg, &self.cfg, &log, 0) {
                    Some(p) => {
                        self.pipeline = Some(p);
                        p
                    }
                    None => {
                        let msg = "Aucune méthode de capture de l'écran ne fonctionne sur ce PC.".to_string();
                        self.start_error = Some(msg.clone());
                        return Err(msg);
                    }
                }
            }
        };
        let audio = AudioEngine::start(&self.cfg, &self.tmp, self.watcher.apps.clone()).map_err(|e| {
            self.start_error = Some(e.clone());
            e
        })?;
        self.start_error = None;
        self.audio = Some(audio);
        let hwnd = self.watcher.apps.game_hwnd.clone();
        self.recorder = Some(Recorder::start(self.ffmpeg.clone(), self.cfg.clone(), pipe, self.tmp.clone(), hwnd));
        Ok(())
    }

    pub fn stop_buffer(&mut self) {
        if let Some(r) = self.recorder.take() {
            r.stop();
        }
        if let Some(a) = self.audio.take() {
            a.stop();
        }
    }

    pub fn is_buffering(&self) -> bool {
        self.recorder.is_some()
    }

    fn game_running(&self) -> bool {
        self.watcher.apps.game_pid.load(std::sync::atomic::Ordering::Relaxed) != 0
    }

    /// Le buffer doit-il tourner maintenant, d'après le mode choisi ?
    pub fn should_run(&self) -> bool {
        if !self.cfg.buffer_enabled {
            return false;
        }
        match self.cfg.buffer_mode.as_str() {
            "game" => self.game_running(),
            _ => true,
        }
    }

    /// Démarre ou arrête le buffer selon le mode. Renvoie vrai si l'état a changé.
    /// `busy` : un clip est en cours d'écriture, on ne coupe rien.
    pub fn update_buffer(&mut self, busy: bool) -> bool {
        let want = self.should_run();
        let running = self.is_buffering();
        if want {
            self.game_gone = None;
            // Après un échec, on ne réessaie qu'à un changement de réglage.
            if !running && self.start_error.is_none() {
                let _ = self.start_buffer();
                return true;
            }
            return false;
        }
        if !running || busy {
            return false;
        }
        // Jeu fermé : on attend un peu avant d'arrêter (sauf pause demandée).
        if self.cfg.buffer_enabled && self.cfg.buffer_mode == "game" {
            let since = *self.game_gone.get_or_insert_with(Instant::now);
            if since.elapsed() < GAME_GRACE {
                return false;
            }
        }
        self.game_gone = None;
        self.stop_buffer();
        true
    }

    /// Applique une nouvelle configuration ; relance le buffer si nécessaire.
    pub fn set_config(&mut self, cfg: Config) -> Result<(), String> {
        let cfg = cfg.sanitized();
        let old = std::mem::replace(&mut self.cfg, cfg);
        self.cfg.save()?;
        let _ = std::fs::create_dir_all(&self.cfg.clips_dir);
        let c = self.cfg.clone();
        let capture_changed = old.fps != c.fps
            || old.height != c.height
            || old.bitrate_mbps != c.bitrate_mbps
            || old.monitor != c.monitor
            || old.capture_cursor != c.capture_cursor
            || old.capture_window != c.capture_window;
        let apps_changed = old.voice_app != c.voice_app || old.game_app != c.game_app;
        let restart = capture_changed
            || apps_changed
            || old.buffer_seconds != c.buffer_seconds
            || old.mic != c.mic
            || old.record_game != c.record_game
            || old.record_voice != c.record_voice
            || old.record_mic != c.record_mic;
        if capture_changed {
            self.pipeline = None;
        }
        if self.is_buffering() && restart {
            self.stop_buffer();
        }
        if apps_changed {
            self.watcher = AppWatcher::start(&c.voice_app, &c.game_app);
        }
        // Un réglage modifié permet de retenter un démarrage qui avait échoué.
        self.start_error = None;
        if self.should_run() {
            if !self.is_buffering() {
                self.start_buffer()?;
            }
        } else if self.is_buffering() {
            self.game_gone = None;
            self.stop_buffer();
        }
        Ok(())
    }

    /// Prépare la sauvegarde d'un clip ; l'exécution se fait hors verrou.
    pub fn clip_job(&self) -> Result<ClipJob, String> {
        if self.recorder.is_none() {
            let msg = if !self.cfg.buffer_enabled {
                "Le buffer est désactivé."
            } else if self.cfg.buffer_mode == "game" {
                "Aucun jeu détecté : le buffer est en veille."
            } else {
                "Le buffer n'est pas encore prêt."
            };
            return Err(msg.into());
        }
        Ok(ClipJob {
            ffmpeg: self.ffmpeg.clone(),
            cfg: self.cfg.clone(),
            tmp: self.tmp.clone(),
            audio: self.audio.as_ref().map(|a| a.reader()),
        })
    }

    /// À appeler régulièrement : démarre ou arrête le buffer selon le mode, et
    /// bascule sur une capture de secours si la capture légère échoue.
    /// Renvoie vrai si l'état du buffer a changé.
    pub fn maintain(&mut self, busy: bool) -> bool {
        let changed = self.update_buffer(busy);
        self.fallback();
        changed
    }

    fn fallback(&mut self) {
        use std::sync::atomic::Ordering;
        let failing = self.recorder.as_ref().map(|r| r.failures.load(Ordering::Relaxed) >= 3).unwrap_or(false);
        let Some(pipe) = self.pipeline else { return };
        let rank = match pipe.grab {
            ffmpeg::Grab::Wgc => 0,
            ffmpeg::Grab::Dda => 1,
            ffmpeg::Grab::Gdi => return,
        };
        if !failing {
            return;
        }
        let _ = std::fs::copy(self.tmp.join("recorder.log"), self.tmp.join("recorder-precedent.log"));
        self.stop_buffer();
        let log = self.tmp.join("detect.log");
        self.pipeline = ffmpeg::detect_pipeline(&self.ffmpeg, &self.cfg, &log, rank + 1);
        if self.pipeline.is_some() {
            let _ = self.start_buffer();
        } else {
            self.start_error = Some("Aucune méthode de capture de l'écran ne fonctionne sur ce PC.".into());
        }
    }

    /// Journal de diagnostic : essais de capture puis dernier journal de l'enregistreur.
    pub fn diagnostics(&self) -> String {
        let read = |name: &str| {
            std::fs::read(self.tmp.join(name))
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        format!(
            "Clipo {}\nCapture : {:?}\n\n--- Détection ---\n{}\n\n--- Enregistreur (avant bascule) ---\n{}\n\n--- Fenêtre du jeu refusée ---\n{}\n\n--- Enregistreur ---\n{}",
            env!("CARGO_PKG_VERSION"),
            self.pipeline,
            read("detect.log").trim(),
            read("recorder-precedent.log").trim(),
            read("recorder-fenetre.log").trim(),
            read("recorder.log").trim()
        )
    }

    pub fn status(&self) -> Status {
        use std::sync::atomic::Ordering;
        let video_trouble = self.start_error.is_some()
            || self.recorder.as_ref().map(|r| r.failures.load(Ordering::Relaxed) >= 3).unwrap_or(false);
        let error = if video_trouble {
            let tail = ffmpeg::log_tail(&self.tmp.join("recorder.log"), 3);
            Some(self.start_error.clone().unwrap_or(tail))
        } else {
            None
        };
        let waiting_game = self.cfg.buffer_enabled && self.cfg.buffer_mode == "game" && !self.is_buffering();
        Status {
            buffering: self.is_buffering(),
            mode: self.cfg.buffer_mode.clone(),
            waiting_game,
            ffmpeg_ok: self.ffmpeg_ok,
            encoder: self.pipeline.map(|p| p.label()),
            voice_detected: self.watcher.apps.voice_detected(),
            video_trouble,
            tracks: self
                .audio
                .as_ref()
                .map(|a| a.tracks.iter().map(|t| t.kind.label().to_string()).collect())
                .unwrap_or_default(),
            game: self.watcher.apps.game(),
            error,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_buffer();
    }
}
