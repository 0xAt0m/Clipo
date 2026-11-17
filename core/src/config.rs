use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Dossier où sont rangés les clips.
    pub clips_dir: String,
    /// Durée conservée en mémoire tampon (secondes).
    pub buffer_seconds: u32,
    /// Raccourci global, ex. "Alt+F10".
    pub hotkey: String,
    pub fps: u32,
    /// Hauteur de sortie ; 0 = résolution native de l'écran (chemin le plus léger).
    pub height: u32,
    pub bitrate_mbps: u32,
    /// Index de l'écran capturé.
    pub monitor: u32,
    pub capture_cursor: bool,
    /// Capturer uniquement la fenêtre du jeu détecté (sinon l'écran entier).
    pub capture_window: bool,
    /// Exécutable de l'application vocale isolée sur sa propre piste.
    pub voice_app: String,
    /// Jeu isolé sur sa piste : "auto" (application en plein écran) ou nom d'exécutable.
    pub game_app: String,
    /// "default", "none" ou nom du périphérique.
    pub mic: String,
    pub record_game: bool,
    pub record_voice: bool,
    pub record_mic: bool,
    pub autostart: bool,
    pub buffer_enabled: bool,
    /// Décalage appliqué aux pistes audio (ms, positif = audio retardé).
    pub audio_offset_ms: i32,
    /// Fichier .wav joué à l'enregistrement d'un clip (vide = son intégré).
    pub sound_file: String,
    pub sound_enabled: bool,
    /// Dernier niveau de compression choisi à l'export : "original", "10", "50", "100".
    pub export_preset: String,
    /// Dossiers supplémentaires affichés dans la bibliothèque (clips SteelSeries, etc.).
    pub extra_dirs: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let clips = dirs::video_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Clipo");
        Config {
            clips_dir: clips.to_string_lossy().into_owned(),
            buffer_seconds: 60,
            hotkey: "Alt+F10".into(),
            fps: 60,
            height: 0,
            bitrate_mbps: 15,
            monitor: 0,
            capture_cursor: true,
            capture_window: true,
            voice_app: "Discord.exe".into(),
            game_app: "auto".into(),
            mic: "default".into(),
            record_game: true,
            record_voice: true,
            record_mic: true,
            autostart: false,
            buffer_enabled: true,
            audio_offset_ms: 0,
            sound_file: String::new(),
            sound_enabled: true,
            export_preset: "100".into(),
            extra_dirs: Vec::new(),
        }
    }
}

pub fn data_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Clipo")
}

impl Config {
    pub fn path() -> PathBuf {
        data_dir().join("config.json")
    }

    pub fn load() -> Config {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let p = Self::path();
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        let s = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(p, s).map_err(|e| e.to_string())
    }

    /// Valeurs bornées pour éviter les configurations absurdes.
    pub fn sanitized(mut self) -> Config {
        self.buffer_seconds = self.buffer_seconds.clamp(10, 600);
        self.fps = if self.fps >= 60 { 60 } else { 30 };
        self.bitrate_mbps = self.bitrate_mbps.clamp(2, 60);
        self.audio_offset_ms = self.audio_offset_ms.clamp(-1000, 1000);
        if self.game_app.trim().is_empty() {
            self.game_app = "auto".into();
        }
        if !["original", "10", "50", "100"].contains(&self.export_preset.as_str()) {
            self.export_preset = "100".into();
        }
        if self.hotkey.trim().is_empty() {
            self.hotkey = "Alt+F10".into();
        }
        self
    }
}
