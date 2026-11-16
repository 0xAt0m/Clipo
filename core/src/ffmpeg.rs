//! Lancement de ffmpeg, détection de l'encodeur matériel, analyse de fichiers.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::{Command, Stdio};

/// Commande ffmpeg sans fenêtre console sous Windows.
pub fn command(ffmpeg: &Path) -> Command {
    let mut c = Command::new(ffmpeg);
    c.arg("-hide_banner");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Encoder {
    Nvenc,
    Amf,
    Qsv,
    X264,
}

impl Encoder {
    pub fn label(&self) -> &'static str {
        match self {
            Encoder::Nvenc => "NVIDIA NVENC",
            Encoder::Amf => "AMD AMF",
            Encoder::Qsv => "Intel Quick Sync",
            Encoder::X264 => "Logiciel (x264)",
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Arguments d'entrée (capture d'écran) + filtre + encodeur pour le buffer.
pub fn capture_args(cfg: &Config, enc: Encoder) -> Vec<String> {
    let fps = cfg.fps;
    let gop = (fps * 2).to_string();
    let br = format!("{}M", cfg.bitrate_mbps);
    let maxbr = format!("{}M", cfg.bitrate_mbps * 3 / 2);
    let buf = format!("{}M", cfg.bitrate_mbps * 2);
    let mut a: Vec<String> = Vec::new();

    if cfg!(windows) {
        let grab = format!(
            "ddagrab=output_idx={}:framerate={}:draw_mouse={}",
            cfg.monitor,
            fps,
            if cfg.capture_cursor { 1 } else { 0 }
        );
        // Sans redimensionnement, les images restent sur le GPU (zéro copie).
        let zero_copy = cfg.height == 0;
        let filter = match (enc, zero_copy) {
            (Encoder::Nvenc, true) | (Encoder::Amf, true) => grab,
            (Encoder::Qsv, true) => format!("{grab},hwmap=derive_device=qsv,format=qsv"),
            (Encoder::X264, true) => format!("{grab},hwdownload,format=bgra"),
            (_, false) => format!(
                "{grab},hwdownload,format=bgra,scale=-2:{}:flags=fast_bilinear",
                cfg.height
            ),
        };
        a.extend(s(&["-f", "lavfi", "-i"]));
        a.push(filter);
    } else {
        // Source de test hors Windows (développement).
        a.extend(s(&["-re", "-f", "lavfi", "-i"]));
        a.push(format!("testsrc2=size=1280x720:rate={fps}"));
    }

    match enc {
        Encoder::Nvenc => a.extend(s(&[
            "-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-bf", "0", "-strict_gop", "1",
        ])),
        Encoder::Amf => a.extend(s(&[
            "-c:v", "h264_amf", "-usage", "transcoding", "-quality", "speed", "-rc", "vbr_peak",
            "-bf", "0",
        ])),
        Encoder::Qsv => a.extend(s(&["-c:v", "h264_qsv", "-preset", "veryfast", "-bf", "0"])),
        Encoder::X264 => a.extend(s(&[
            "-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency", "-pix_fmt",
            "yuv420p",
        ])),
    }
    a.extend(s(&["-b:v", &br, "-maxrate", &maxbr, "-bufsize", &buf, "-g", &gop]));
    a
}

/// Essaie chaque encodeur sur une demi-seconde de capture réelle.
pub fn detect_encoder(ffmpeg: &Path, cfg: &Config) -> Encoder {
    if !cfg!(windows) {
        return Encoder::X264;
    }
    for enc in [Encoder::Nvenc, Encoder::Amf, Encoder::Qsv] {
        let ok = command(ffmpeg)
            .args(["-loglevel", "error"])
            .args(capture_args(cfg, enc))
            .args(["-t", "0.5", "-f", "null", "-"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        if ok {
            return enc;
        }
    }
    Encoder::X264
}

pub fn is_available(ffmpeg: &Path) -> bool {
    command(ffmpeg)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub audio_streams: u32,
    pub has_video: bool,
}

/// Analyse un fichier en lisant la sortie de `ffmpeg -i` (évite d'embarquer ffprobe).
pub fn probe(ffmpeg: &Path, file: &Path) -> Result<MediaInfo, String> {
    let out = command(ffmpeg)
        .arg("-i")
        .arg(file)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg introuvable : {e}"))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let mut info = MediaInfo::default();
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("Duration: ") {
            let d = rest.split(',').next().unwrap_or("");
            let p: Vec<&str> = d.split(':').collect();
            if p.len() == 3 {
                let h: f64 = p[0].parse().unwrap_or(0.0);
                let m: f64 = p[1].parse().unwrap_or(0.0);
                let sec: f64 = p[2].parse().unwrap_or(0.0);
                info.duration = h * 3600.0 + m * 60.0 + sec;
            }
        } else if l.starts_with("Stream #") && l.contains("Audio:") {
            info.audio_streams += 1;
        } else if l.starts_with("Stream #") && l.contains("Video:") && !info.has_video {
            // Les images fixes (png/jpg) comptent comme vidéo d'une image.
            info.has_video = true;
            for part in l.split(',') {
                let t = part.trim();
                if info.width == 0 {
                    let tok = t.split_whitespace().next().unwrap_or("");
                    if let Some((w, h)) = tok.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                            if w > 0 && h > 0 {
                                info.width = w;
                                info.height = h;
                            }
                        }
                    }
                }
                if let Some(f) = t.strip_suffix(" fps") {
                    info.fps = f.trim().parse().unwrap_or(0.0);
                }
            }
        }
    }
    if !info.has_video && info.audio_streams == 0 {
        return Err("Fichier illisible".into());
    }
    Ok(info)
}

/// Dernières lignes d'un journal ffmpeg, pour les messages d'erreur.
pub fn log_tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let v: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}
