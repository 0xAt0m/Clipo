//! Moteur de Clipo, indépendant de l'interface.

pub mod audio;
pub mod capture;
pub mod clip;
pub mod config;
pub mod export;
pub mod ffmpeg;
pub mod recorder;
pub mod sound;

use audio::AudioEngine;
use clip::ClipJob;
use config::Config;
use ffmpeg::Pipeline;
use recorder::Recorder;
use serde::Serialize;
use std::path::PathBuf;

#[derive(Serialize, Clone, Debug)]
pub struct Status {
    pub buffering: bool,
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
    game_hwnd: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Engine {
    pub fn new(cfg: Config, ffmpeg: PathBuf) -> Engine {
        let tmp = std::env::temp_dir().join("Clipo");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let ffmpeg_ok = ffmpeg::is_available(&ffmpeg);
        let _ = std::fs::create_dir_all(&cfg.clips_dir);
        Engine { cfg, ffmpeg, pipeline: None, ffmpeg_ok, recorder: None, audio: None, tmp, start_error: None, game_hwnd: Default::default() }
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
        self.start_error = None;
        self.audio = Some(AudioEngine::start(&self.cfg, &self.tmp, self.game_hwnd.clone())?);
        self.recorder = Some(Recorder::start(self.ffmpeg.clone(), self.cfg.clone(), pipe, self.tmp.clone(), self.game_hwnd.clone()));
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
        let restart = capture_changed
            || old.buffer_seconds != c.buffer_seconds
            || old.voice_app != c.voice_app
            || old.game_app != c.game_app
            || old.mic != c.mic
            || old.record_game != c.record_game
            || old.record_voice != c.record_voice
            || old.record_mic != c.record_mic;
        if capture_changed {
            self.pipeline = None;
        }
        let running = self.is_buffering();
        if running && (restart || !c.buffer_enabled) {
            self.stop_buffer();
        }
        if c.buffer_enabled && !self.is_buffering() {
            self.start_buffer()?;
        }
        Ok(())
    }

    /// Prépare la sauvegarde d'un clip ; l'exécution se fait hors verrou.
    pub fn clip_job(&self) -> Result<ClipJob, String> {
        if self.recorder.is_none() {
            return Err("Le buffer est en pause.".into());
        }
        Ok(ClipJob {
            ffmpeg: self.ffmpeg.clone(),
            cfg: self.cfg.clone(),
            tmp: self.tmp.clone(),
            audio: self.audio.as_ref().map(|a| a.reader()),
        })
    }

    /// Si la capture légère échoue en conditions réelles, bascule une fois sur la
    /// capture de secours. À appeler régulièrement.
    pub fn maintain(&mut self) {
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
        Status {
            buffering: self.is_buffering(),
            ffmpeg_ok: self.ffmpeg_ok,
            encoder: self.pipeline.map(|p| p.label()),
            voice_detected: self.audio.as_ref().map(|a| a.voice_detected()).unwrap_or(false),
            video_trouble,
            tracks: self
                .audio
                .as_ref()
                .map(|a| a.tracks.iter().map(|t| t.kind.label().to_string()).collect())
                .unwrap_or_default(),
            game: self.audio.as_ref().and_then(|a| a.game()),
            error,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_buffer();
    }
}
