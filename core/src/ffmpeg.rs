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

/// Méthode de capture de l'écran.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub enum Grab {
    /// Windows Graphics Capture : fenêtre du jeu ou écran, images sur le GPU.
    Wgc,
    /// Desktop Duplication : les images restent sur le GPU.
    Dda,
    /// GDI : plus lourd, mais fonctionne partout (secours).
    Gdi,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct Pipeline {
    pub grab: Grab,
    pub enc: Encoder,
}

impl Pipeline {
    pub fn label(&self) -> String {
        match self.grab {
            Grab::Wgc | Grab::Dda => self.enc.label().to_string(),
            Grab::Gdi => format!("{} (capture de secours)", self.enc.label()),
        }
    }
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// Arguments d'entrée (capture) + filtre + encodeur pour le buffer.
/// `hwnd` : fenêtre du jeu à capturer seule (0 = l'écran choisi dans les réglages).
pub fn capture_args(cfg: &Config, pipe: Pipeline, hwnd: u64) -> Vec<String> {
    let enc = pipe.enc;
    // La capture de secours ne tient pas 60 images par seconde.
    let fps = if pipe.grab == Grab::Gdi { cfg.fps.min(30) } else { cfg.fps };
    let gop = (fps * 2).to_string();
    let br = format!("{}M", cfg.bitrate_mbps);
    let maxbr = format!("{}M", cfg.bitrate_mbps * 3 / 2);
    let buf = format!("{}M", cfg.bitrate_mbps * 2);
    let mut a: Vec<String> = Vec::new();
    let cursor = if cfg.capture_cursor { 1 } else { 0 };
    let scale = if cfg.height > 0 {
        format!("scale=-2:{}:flags=fast_bilinear,", cfg.height)
    } else {
        String::new()
    };

    if !cfg!(windows) {
        // Source de test hors Windows (développement).
        a.extend(s(&["-re", "-f", "lavfi", "-i"]));
        a.push(format!("testsrc2=size=1280x720:rate={fps}"));
    } else if pipe.grab == Grab::Wgc {
        let source = if hwnd != 0 { format!("hwnd={hwnd}") } else { format!("monitor_idx={}", cfg.monitor) };
        // Le redimensionnement se fait sur le GPU ; dimensions paires pour l'encodeur.
        let size = if cfg.height > 0 {
            let w = (cfg.height * 16 / 9) & !1;
            format!(":width={w}:height={}:resize_mode=scale_aspect", cfg.height)
        } else {
            ":width=-2:height=-2".to_string()
        };
        // La source ne livre une image que lorsque l'écran change : `fps` régularise.
        let grab = format!("gfxcapture={source}:capture_cursor={cursor}:max_framerate={fps}{size},fps={fps}");
        let filter = match enc {
            Encoder::Nvenc | Encoder::Amf => grab,
            Encoder::Qsv => format!("{grab},hwmap=derive_device=qsv,format=qsv"),
            Encoder::X264 => format!("{grab},hwdownload,format=bgra,format=yuv420p"),
        };
        a.extend(s(&["-f", "lavfi", "-i"]));
        a.push(filter);
    } else if pipe.grab == Grab::Dda {
        let grab = format!(
            "ddagrab=output_idx={}:framerate={fps}:draw_mouse={cursor}:output_fmt=8bit",
            cfg.monitor
        );
        // Sans redimensionnement, les images restent sur le GPU (zéro copie).
        let zero_copy = cfg.height == 0;
        let filter = match (enc, zero_copy) {
            (Encoder::Nvenc, true) | (Encoder::Amf, true) => grab,
            (Encoder::Qsv, true) => format!("{grab},hwmap=derive_device=qsv,format=qsv"),
            (Encoder::X264, _) => format!("{grab},hwdownload,format=bgra,{scale}format=yuv420p"),
            (_, false) => format!("{grab},hwdownload,format=bgra,{scale}format=nv12"),
        };
        a.extend(s(&["-f", "lavfi", "-i"]));
        a.push(filter);
    } else {
        a.extend(s(&["-f", "gdigrab", "-framerate"]));
        a.push(fps.to_string());
        a.extend(s(&["-draw_mouse", if cfg.capture_cursor { "1" } else { "0" }]));
        // Un seul écran, pas tout le bureau.
        if let Some((x, y, w, h)) = crate::capture::monitor_rect(cfg.monitor) {
            a.extend(s(&["-offset_x", &x.to_string(), "-offset_y", &y.to_string(), "-video_size", &format!("{w}x{h}")]));
        }
        a.extend(s(&["-i", "desktop", "-vf"]));
        // Dimensions paires obligatoires pour l'encodage 4:2:0.
        let fmt = if enc == Encoder::X264 { "yuv420p" } else { "nv12" };
        a.push(format!("crop=trunc(iw/2)*2:trunc(ih/2)*2,{scale}format={fmt}"));
    }

    match enc {
        Encoder::Nvenc => a.extend(s(&["-c:v", "h264_nvenc", "-preset", "p4", "-rc", "vbr", "-bf", "0"])),
        Encoder::Amf => a.extend(s(&[
            "-c:v", "h264_amf", "-usage", "transcoding", "-quality", "speed", "-rc", "vbr_peak",
            "-bf", "0",
        ])),
        Encoder::Qsv => a.extend(s(&["-c:v", "h264_qsv", "-preset", "veryfast", "-bf", "0"])),
        Encoder::X264 => a.extend(s(&["-c:v", "libx264", "-preset", "ultrafast", "-tune", "zerolatency"])),
    }
    a.extend(s(&["-b:v", &br, "-maxrate", &maxbr, "-bufsize", &buf, "-g", &gop]));
    a
}

/// Lance une commande avec un délai maximal ; vrai si elle réussit.
fn run_with_timeout(mut cmd: Command, secs: u64) -> bool {
    let Ok(mut child) = cmd.spawn() else { return false };
    for _ in 0..secs * 20 {
        match child.try_wait() {
            Ok(Some(st)) => return st.success(),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Essaie chaque combinaison capture + encodeur sur une seconde réelle, de la
/// plus légère à la plus compatible. Le détail des essais est écrit dans `log`.
pub fn detect_pipeline(ffmpeg: &Path, cfg: &Config, log: &Path, skip: usize) -> Option<Pipeline> {
    use std::io::Write;
    if !cfg!(windows) {
        return Some(Pipeline { grab: Grab::Wgc, enc: Encoder::X264 });
    }
    let _ = std::fs::write(log, "");
    for grab in [Grab::Wgc, Grab::Dda, Grab::Gdi].into_iter().skip(skip) {
        for enc in [Encoder::Nvenc, Encoder::Amf, Encoder::Qsv, Encoder::X264] {
            let pipe = Pipeline { grab, enc };
            let open = || std::fs::OpenOptions::new().append(true).create(true).open(log);
            if let Ok(mut f) = open() {
                let _ = writeln!(f, "=== essai {grab:?} + {enc:?}");
            }
            let mut cmd = command(ffmpeg);
            cmd.args(["-loglevel", "error"])
                .args(capture_args(cfg, pipe, 0))
                .args(["-t", "1", "-f", "null", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(open().map(Stdio::from).unwrap_or_else(|_| Stdio::null()));
            let ok = run_with_timeout(cmd, 12);
            if let Ok(mut f) = open() {
                let _ = writeln!(f, "=== {}", if ok { "OK" } else { "échec" });
            }
            if ok {
                return Some(pipe);
            }
        }
    }
    None
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
    /// Titre de chaque piste audio (vide si absent).
    #[serde(default)]
    pub audio_titles: Vec<String>,
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
    let mut in_audio = false;
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("Stream #") {
            in_audio = false;
        }
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
            info.audio_titles.push(String::new());
            in_audio = true;
        } else if in_audio && (l.starts_with("title") || l.starts_with("handler_name")) {
            if let (Some((key, v)), Some(last)) = (l.split_once(':'), info.audio_titles.last_mut()) {
                let v = v.trim();
                // « title » prime sur « handler_name » ; les noms génériques sont ignorés.
                let generic = v.is_empty() || v == "SoundHandler" || v.starts_with("ISO Media");
                if !generic && (key.trim() == "title" || last.is_empty()) {
                    *last = v.to_string();
                }
            }
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

/// Enveloppe sonore d'une piste : `per_sec` valeurs (0-255) par seconde.
pub fn waveform(ffmpeg: &Path, file: &Path, stream: u32, per_sec: u32) -> Result<Vec<u8>, String> {
    const RATE: u32 = 8000;
    let out = command(ffmpeg)
        .args(["-loglevel", "error", "-i"])
        .arg(file)
        .args(["-map", &format!("0:a:{stream}"), "-ac", "1", "-ar", &RATE.to_string(), "-f", "s16le", "-"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("ffmpeg introuvable : {e}"))?;
    let win = (RATE / per_sec.max(1)) as usize * 2;
    Ok(out
        .stdout
        .chunks(win)
        .map(|c| {
            let peak = c
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
                .max()
                .unwrap_or(0) as f32
                / 32768.0;
            // Courbe adoucie : les sons faibles restent visibles.
            (peak.sqrt() * 255.0).min(255.0) as u8
        })
        .collect())
}
