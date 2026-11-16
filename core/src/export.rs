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
}

fn is_image(path: &str) -> bool {
    let l = path.to_lowercase();
    [".png", ".jpg", ".jpeg", ".webp", ".bmp"].iter().any(|e| l.ends_with(e))
}

pub struct Plan {
    pub args: Vec<String>,
    pub duration: f64,
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
    let w = main.width & !1;
    let h = main.height & !1;
    let fps = if main.fps > 1.0 { main.fps.min(120.0) } else { 60.0 };

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
    args.extend([
        "-filter_complex".into(),
        fc.join(";"),
        "-map".into(),
        format!("[{vlabel}]"),
        "-map".into(),
        "[ac]".into(),
    ]);
    for s in [
        "-c:v", "libx264", "-preset", "veryfast", "-crf", "19", "-pix_fmt", "yuv420p", "-c:a", "aac",
        "-b:a", "192k", "-movflags", "+faststart", "-t",
    ] {
        args.push(s.into());
    }
    args.push(format!("{total:.3}"));
    args.push(p.output.clone());
    Ok(Plan { args, duration: total })
}

/// Lance l'export et rapporte la progression (0..1).
pub fn run(ffmpeg_path: &Path, p: &Project, log: &Path, progress: &dyn Fn(f64)) -> Result<PathBuf, String> {
    let plan = plan(p, &|f| ffmpeg::probe(ffmpeg_path, Path::new(f)))?;
    let out = PathBuf::from(&p.output);
    if let Some(d) = out.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
    }
    let mut child = ffmpeg::command(ffmpeg_path)
        .args(["-loglevel", "error", "-nostats", "-progress", "pipe:1"])
        .args(&plan.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(File::create(log).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
        .spawn()
        .map_err(|e| format!("ffmpeg introuvable : {e}"))?;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
                if let Ok(us) = v.trim().parse::<f64>() {
                    progress((us / 1e6 / plan.duration).clamp(0.0, 1.0));
                }
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if !status.success() {
        let _ = std::fs::remove_file(&out);
        return Err(format!("L'export a échoué.\n{}", ffmpeg::log_tail(log, 4)));
    }
    progress(1.0);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(audio: u32) -> MediaInfo {
        MediaInfo { duration: 30.0, width: 1921, height: 1080, fps: 60.0, audio_streams: audio, has_video: true }
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
        assert_eq!(plan.args.last().unwrap(), "out.mp4");
    }

    #[test]
    fn empty_project_is_rejected() {
        let p = Project { main: "a".into(), track_count: 0, mix_first: false, volumes: vec![], pieces: vec![], overlays: vec![], output: "o".into() };
        assert!(plan(&p, &|_| Ok(info(1))).is_err());
    }
}
