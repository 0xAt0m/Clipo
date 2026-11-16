//! Moteur de Clipo, indépendant de l'interface.

pub mod audio;
pub mod capture;
pub mod clip;
pub mod config;
pub mod export;
pub mod ffmpeg;
pub mod recorder;

use audio::AudioEngine;
use clip::ClipJob;
use config::Config;
use ffmpeg::Encoder;
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
}

pub struct Engine {
    pub cfg: Config,
    pub ffmpeg: PathBuf,
    encoder: Option<Encoder>,
    ffmpeg_ok: bool,
    recorder: Option<Recorder>,
    audio: Option<AudioEngine>,
    tmp: PathBuf,
}

impl Engine {
    pub fn new(cfg: Config, ffmpeg: PathBuf) -> Engine {
        let tmp = std::env::temp_dir().join("Clipo");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let ffmpeg_ok = ffmpeg::is_available(&ffmpeg);
        Engine { cfg, ffmpeg, encoder: None, ffmpeg_ok, recorder: None, audio: None, tmp }
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
        let enc = match self.encoder {
            Some(e) => e,
            None => {
                let e = ffmpeg::detect_encoder(&self.ffmpeg, &self.cfg);
                self.encoder = Some(e);
                e
            }
        };
        self.audio = Some(AudioEngine::start(&self.cfg, &self.tmp)?);
        self.recorder = Some(Recorder::start(self.ffmpeg.clone(), self.cfg.clone(), enc, self.tmp.clone()));
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
        let c = self.cfg.clone();
        let capture_changed = old.fps != c.fps
            || old.height != c.height
            || old.bitrate_mbps != c.bitrate_mbps
            || old.monitor != c.monitor
            || old.capture_cursor != c.capture_cursor;
        let restart = capture_changed
            || old.buffer_seconds != c.buffer_seconds
            || old.voice_app != c.voice_app
            || old.mic != c.mic
            || old.record_game != c.record_game
            || old.record_voice != c.record_voice
            || old.record_mic != c.record_mic;
        if old.monitor != c.monitor || old.height != c.height {
            self.encoder = None;
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

    pub fn status(&self) -> Status {
        use std::sync::atomic::Ordering;
        Status {
            buffering: self.is_buffering(),
            ffmpeg_ok: self.ffmpeg_ok,
            encoder: self.encoder.map(|e| e.label().to_string()),
            voice_detected: self.audio.as_ref().map(|a| a.voice_detected()).unwrap_or(false),
            video_trouble: self
                .recorder
                .as_ref()
                .map(|r| r.failures.load(Ordering::Relaxed) >= 3)
                .unwrap_or(false),
            tracks: self
                .audio
                .as_ref()
                .map(|a| a.tracks.iter().map(|t| t.kind.label().to_string()).collect())
                .unwrap_or_default(),
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_buffer();
    }
}
