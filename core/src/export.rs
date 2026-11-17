//! Export d'un montage : coupes, volumes par piste, vidéos insérées, calques.

use crate::ffmpeg::{self, MediaInfo};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Piece {
    /// None = clip principal ; sinon chemin d'une vidéo ou image insérée.
    #[serde(default)]
    pub src: Option<String>,
    pub start: f64,
    pub end: f64,
    /// Volume de la vidéo insérée.
    #[serde(default = "one")]
    pub volume: f64,
}

fn one() -> f64 {
    1.0
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Overlay {
    /// Image PNG transparente à la taille de la vidéo, produite par l'interface.
    pub png: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Project {
    pub main: String,
    /// Nombre de pistes séparées du clip (0 = fichier ordinaire).
    #[serde(default)]
    pub track_count: usize,
    #[serde(default)]
    pub mix_first: bool,
    /// Volume par piste (ou volume unique si pas de pistes séparées).
    #[serde(default)]
    pub volumes: Vec<f64>,
    pub pieces: Vec<Piece>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
    pub output: String,
    /// Taille maximale du fichier en Mo (None = qualité d'origine).
    #[serde(default)]
    pub target_mb: Option<u32>,
}

fn is_image(path: &str) -> bool {
    let l = path.to_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".bmp"].iter().any(|e| l.ends_with(e))
}

pub struct Plan {
    /// Entrées, filtres et sélection des flux.
    pub args: Vec<String>,
    /// Encodage vidéo (sans le numéro de passe).
    pub video: Vec<String>,
    pub audio: Vec<String>,
    /// Vrai si l'encodage se fait en deux passes (taille cible).
    pub two_pass: bool,
    pub duration: f64,
}

/// Réglages d'encodage pour tenir dans `mb` mégaoctets : (débit vidéo kbit/s, débit audio, hauteur max, ips max).
fn budget(mb: u32, duration: f64) -> (u32, u32, u32, f64) {
    // Marge de 6 % pour l'enrobage du conteneur.
    let total_kbps = (mb as f64 * 8000.0 * 0.94) / duration.max(1.0);
    let audio = if total_kbps < 700.0 { 64 } else if total_kbps < 2500.0 { 96 } else { 128 };
    let video = (total_kbps - audio as f64).max(80.0) as u32;
    let (height, fps) = match video {
        0..=699 => (480, 30.0),
        700..=1799 => (720, 30.0),
        1800..=3999 => (720, 60.0),
        4000..=7999 => (1080, 60.0),
        _ => (4320, 120.0),
    };
    (video, audio, height, fps)
}

/// Construit la commande ffmpeg. `probe` renseigne chaque fichier source.
pub fn plan(p: &Project, probe: &dyn Fn(&str) -> Result<MediaInfo, String>) -> Result<Plan, String> {
    if p.pieces.is_empty() {
        return Err("Le montage est vide.".into());
    }
    let main = probe(&p.main)?;
    if !main.has_video || main.width == 0 {
        return Err("Le clip principal n'a pas de piste vidéo lisible.".into());
    }
    let mut w = main.width & !1;
    let mut h = main.height & !1;
    let mut fps = if main.fps > 1.0 { main.fps.min(120.0) } else { 60.0 };
    let planned: f64 = p.pieces.iter().map(|x| (x.end - x.start).max(0.0)).sum();
    let size = p.target_mb.map(|mb| budget(mb, planned));
    if let Some((_, _, max_h, max_fps)) = size {
        if h > max_h {
            w = ((w as f64 * max_h as f64 / h as f64) as u32) & !1;
            h = max_h;
        }
        fps = fps.min(max_fps);
    }

    let mut inputs: Vec<String> = vec!["-i".into(), p.main.clone()];
    let mut next_input = 1usize;
    let mut fc: Vec<String> = Vec::new();
    let mut total = 0.0;
    let vnorm = format!(
        "scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,fps={fps},format=yuv420p"
    );
    let anorm = "aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo";
    let vol = |i: usize| p.volumes.get(i).copied().unwrap_or(1.0).clamp(0.0, 4.0);

    for (k, piece) in p.pieces.iter().enumerate() {
        let d = piece.end - piece.start;
        if d <= 0.01 {
            return Err("Un segment du montage a une durée nulle.".into());
        }
        total += d;
        let silence = format!("anullsrc=r=48000:cl=stereo,atrim=duration={d:.3},{anorm}[a{k}]");
        match &piece.src {
            None => {
                fc.push(format!(
                    "[0:v:0]trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS,{vnorm}[v{k}]",
                    piece.start, piece.end
                ));
                let cut = format!("atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS", piece.start, piece.end);
                if p.track_count > 0 {
                    let first = if p.mix_first { 1 } else { 0 };
                    let mut labels = String::new();
                    for j in 0..p.track_count {
                        fc.push(format!("[0:a:{}]{cut},volume={:.3},{anorm}[a{k}t{j}]", first + j, vol(j)));
                        labels.push_str(&format!("[a{k}t{j}]"));
                    }
                    if p.track_count > 1 {
                        fc.push(format!(
                            "{labels}amix=inputs={}:normalize=0:duration=longest,alimiter=limit=0.97,{anorm},apad,atrim=duration={d:.3}[a{k}]",
                            p.track_count
                        ));
                    } else {
                        fc.push(format!("{labels}apad,atrim=duration={d:.3}[a{k}]"));
                    }
                } else if main.audio_streams > 0 {
                    fc.push(format!("[0:a:0]{cut},volume={:.3},{anorm},apad,atrim=duration={d:.3}[a{k}]", vol(0)));
                } else {
                    fc.push(silence);
                }
            }
            Some(src) => {
                let idx = next_input;
                next_input += 1;
                if is_image(src) {
                    inputs.extend(["-loop".into(), "1".into(), "-t".into(), format!("{d:.3}"), "-i".into(), src.clone()]);
                    fc.push(format!("[{idx}:v:0]setpts=PTS-STARTPTS,{vnorm}[v{k}]"));
                    fc.push(silence);
                } else {
                    let info = probe(src)?;
                    if !info.has_video {
                        return Err(format!("Pas de vidéo dans : {src}"));
                    }
                    inputs.extend(["-i".into(), src.clone()]);
                    fc.push(format!(
                        "[{idx}:v:0]trim=start={:.3}:end={:.3},setpts=PTS-STARTPTS,{vnorm}[v{k}]",
                        piece.start, piece.end
                    ));
                    if info.audio_streams > 0 {
                        fc.push(format!(
                            "[{idx}:a:0]atrim=start={:.3}:end={:.3},asetpts=PTS-STARTPTS,volume={:.3},{anorm},apad,atrim=duration={d:.3}[a{k}]",
                            piece.start,
                            piece.end,
                            piece.volume.clamp(0.0, 4.0)
                        ));
                    } else {
                        fc.push(silence);
                    }
                }
            }
        }
    }

    let n = p.pieces.len();
    let chain: String = (0..n).map(|k| format!("[v{k}][a{k}]")).collect();
    fc.push(format!("{chain}concat=n={n}:v=1:a=1[vc][ac]"));

    let mut vlabel = "vc".to_string();
    for (i, o) in p.overlays.iter().enumerate() {
        let idx = next_input;
        next_input += 1;
        inputs.extend(["-i".into(), o.png.clone()]);
        let out = format!("vo{i}");
        fc.push(format!(
            "[{vlabel}][{idx}:v:0]overlay=0:0:enable='between(t,{:.3},{:.3})'[{out}]",
            o.start, o.end
        ));
        vlabel = out;
    }

    let mut args: Vec<String> = vec!["-y".into()];
    args.extend(inputs);
    args.extend(["-filter_complex".into(), fc.join(";"), "-map".into(), format!("[{vlabel}]")]);
    let sv = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let (video, audio) = match size {
        Some((vk, ak, _, _)) => (
            [sv(&["-c:v", "libx264", "-preset", "medium", "-pix_fmt", "yuv420p", "-b:v"]), vec![format!("{vk}k")]].concat(),
            [sv(&["-c:a", "aac", "-b:a"]), vec![format!("{ak}k")]].concat(),
        ),
        None => (
            sv(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "18", "-pix_fmt", "yuv420p"]),
            sv(&["-c:a", "aac", "-b:a", "192k"]),
        ),
    };
    Ok(Plan { args, video, audio, two_pass: size.is_some(), duration: total })
}

fn run_pass(
    ffmpeg_path: &Path,
    args: &[String],
    log: &Path,
    duration: f64,
    progress: &dyn Fn(f64),
) -> Result<(), String> {
    let mut child = ffmpeg::command(ffmpeg_path)
        .args(["-loglevel", "error", "-nostats", "-progress", "pipe:1"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(log).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .spawn()
        .map_err(|e| format!("ffmpeg introuvable : {e}"))?;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
                if let Ok(us) = v.trim().parse::<f64>() {
                    progress((us / 1e6 / duration).clamp(0.0, 1.0));
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("L'export a échoué.\n{}", ffmpeg::log_tail(log, 4)));
    }
    Ok(())
}

/// Lance l'export et rapporte la progression (0..1).
pub fn run(ffmpeg_path: &Path, p: &Project, log: &Path, progress: &dyn Fn(f64)) -> Result<PathBuf, String> {
    let plan = plan(p, &|f| ffmpeg::probe(ffmpeg_path, Path::new(f)))?;
    let out = PathBuf::from(&p.output);
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let tail = |extra: &[&str]| -> Vec<String> {
        let mut v = plan.args.clone();
        v.extend(plan.video.iter().cloned());
        v.extend(extra.iter().map(|x| x.to_string()));
        v
    };
    let total = format!("{:.3}", plan.duration);
    let result = if plan.two_pass {
        // Deux passes : la taille du fichier respecte la cible.
        let stats = log.with_extension("passe").to_string_lossy().into_owned();
        let first = tail(&["-pass", "1", "-passlogfile", &stats, "-map", "[ac]", "-c:a", "pcm_s16le", "-t", &total, "-f", "null", "-"]);
        let r = run_pass(ffmpeg_path, &first, log, plan.duration, &|x| progress(x * 0.45)).and_then(|_| {
            let mut second = tail(&["-pass", "2", "-passlogfile", &stats, "-map", "[ac]"]);
            second.extend(plan.audio.iter().cloned());
            second.extend(["-movflags".into(), "+faststart".into(), "-t".into(), total.clone(), p.output.clone()]);
            run_pass(ffmpeg_path, &second, log, plan.duration, &|x| progress(0.45 + x * 0.55))
        });
        for suffix in ["-0.log", "-0.log.mbtree", "-0.log.temp", "-0.log.mbtree.temp"] {
            let _ = std::fs::remove_file(format!("{stats}{suffix}"));
        }
        r
    } else {
        let mut one = tail(&["-map", "[ac]"]);
        one.extend(plan.audio.iter().cloned());
        one.extend(["-movflags".into(), "+faststart".into(), "-t".into(), total.clone(), p.output.clone()]);
        run_pass(ffmpeg_path, &one, log, plan.duration, progress)
    };
    if let Err(e) = result {
        let _ = std::fs::remove_file(&out);
        return Err(e);
    }
    progress(1.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(audio: u32) -> MediaInfo {
        MediaInfo { duration: 30.0, width: 1921, height: 1080, fps: 60.0, audio_streams: audio, has_video: true, audio_titles: vec![] }
    }

    fn plan_for(p: &Project) -> Plan {
        plan(p, &|f| Ok(info(if f == "b.mp4" { 0 } else { 4 }))).unwrap()
    }

    #[test]
    fn plan_builds_cut_with_tracks_and_overlay() {
        let p = Project {
            main: "clip.mp4".into(),
            track_count: 3,
            mix_first: true,
            volumes: vec![1.0, 0.0, 9.0],
            pieces: vec![
                Piece { src: None, start: 0.0, end: 4.0, volume: 1.0 },
                Piece { src: Some("b.mp4".into()), start: 1.0, end: 2.0, volume: 0.5 },
                Piece { src: None, start: 7.0, end: 10.0, volume: 1.0 },
            ],
            overlays: vec![Overlay { png: "o.png".into(), start: 1.0, end: 3.0 }],
            output: "out.mp4".into(),
            target_mb: None,
        };
        let plan = plan(&p, &|f| Ok(info(if f == "b.mp4" { 0 } else { 4 }))).unwrap();
        assert!((plan.duration - 8.0).abs() < 1e-9);
        let fc = &plan.args[plan.args.iter().position(|a| a == "-filter_complex").unwrap() + 1];
        assert!(fc.contains("concat=n=3:v=1:a=1"));
        assert!(fc.contains("[0:a:1]") && fc.contains("[0:a:3]"), "pistes séparées après le mixage");
        assert!(fc.contains("volume=4.000"), "volume borné");
        assert!(fc.contains("scale=1920:1080"), "dimensions paires");
        assert!(fc.contains("anullsrc"), "vidéo insérée sans son = silence");
        assert!(fc.contains("[vc][2:v:0]overlay=0:0:enable='between(t,1.000,3.000)'"));
        assert!(!plan.two_pass);
        let small = plan_for(&Project { target_mb: Some(10), ..p.clone() });
        assert!(small.two_pass && small.video.contains(&"9272k".to_string()), "{:?}", small.video);
    }

    #[test]
    fn empty_project_is_rejected() {
        let p = Project { main: "a".into(), track_count: 0, mix_first: false, volumes: vec![], pieces: vec![], overlays: vec![], output: "o".into(), target_mb: None };
        assert!(plan(&p, &|_| Ok(info(1))).is_err());
    }
}
