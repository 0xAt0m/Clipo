//! Sauvegarde d'un clip à partir du buffer, et bibliothèque de clips.

use crate::audio::AudioReader;
use crate::config::Config;
use crate::ffmpeg;
use crate::recorder::{list_segments, SEGMENT_SECS};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct ClipMeta {
    /// Noms des pistes séparées, dans l'ordre.
    pub tracks: Vec<String>,
    /// Vrai si la première piste audio du mp4 est le mixage (les pistes séparées suivent).
    pub mix_first: bool,
    pub duration: f64,
    /// Nombre de pistes audio du fichier (0 = inconnu).
    #[serde(default)]
    pub audio_streams: u32,
}

#[derive(Serialize, Clone, Debug)]
pub struct TrackInfo {
    pub name: String,
    /// Fichier audio séparé utilisé pour la prévisualisation.
    pub file: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct ClipInfo {
    pub name: String,
    pub path: String,
    pub thumb: Option<String>,
    pub duration: f64,
    pub created_ms: u64,
    pub size: u64,
    pub tracks: Vec<TrackInfo>,
    pub mix_first: bool,
    /// Dossier d'origine quand le clip ne vient pas du dossier principal.
    pub source: Option<String>,
}

/// Dossier annexe d'un clip (pistes séparées, vignette, projet de montage).
pub fn sidecar_dir(clip: &Path) -> PathBuf {
    let stem = clip.file_stem().unwrap_or_default();
    clip.parent().unwrap_or(Path::new(".")).join(".clipo").join(stem)
}

/// Tout ce qu'il faut pour sauver un clip sans bloquer le reste de l'application.
pub struct ClipJob {
    pub ffmpeg: PathBuf,
    pub cfg: Config,
    pub tmp: PathBuf,
    pub audio: Option<AudioReader>,
}

impl ClipJob {
    pub fn run(&self) -> Result<ClipInfo, String> {
        let press = SystemTime::now();
        let want = Duration::from_secs(self.cfg.buffer_seconds as u64);

        // On attend la fin du segment en cours pour inclure l'instant de l'appui.
        let deadline = press + Duration::from_secs(2 * SEGMENT_SECS + 2);
        let mut segs = list_segments(&self.tmp);
        while SystemTime::now() < deadline {
            if segs.last().map(|s| s.created > press).unwrap_or(false) {
                break;
            }
            std::thread::sleep(Duration::from_millis(40));
            segs = list_segments(&self.tmp);
        }
        // Seule la dernière session ffmpeg a des horodatages continus.
        let run = segs.last().map(|s| s.run).ok_or("Le buffer est vide : rien à enregistrer.")?;
        segs.retain(|s| s.run == run);
        if segs.len() < 2 {
            return Err("Le buffer vient de démarrer, réessayez dans quelques secondes.".into());
        }
        // Le dernier segment est en cours d'écriture : il marque la fin du clip.
        let end = segs.last().unwrap().created;
        let from = press.checked_sub(want).unwrap_or(UNIX_EPOCH);
        let n = segs.len() - 1;
        let first = (0..n).find(|&i| segs[i + 1].created > from).unwrap_or(n - 1);
        let chosen = &segs[first..n];
        let start = chosen[0].created;
        let duration = end.duration_since(start).unwrap_or_default().as_secs_f64();

        let work = self.tmp.join(format!("clip_{}", millis(press)));
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let result = self.assemble(chosen.iter().map(|s| s.path.as_path()), start, end, duration, &work);
        let _ = std::fs::remove_dir_all(&work);
        result
    }

    fn assemble<'a>(
        &self,
        segments: impl Iterator<Item = &'a Path>,
        start: SystemTime,
        end: SystemTime,
        duration: f64,
        work: &Path,
    ) -> Result<ClipInfo, String> {
        // Les segments MPEG-TS se concatènent octet par octet.
        let ts = work.join("video.ts");
        {
            let mut out = File::create(&ts).map_err(|e| e.to_string())?;
            for p in segments {
                let data = std::fs::read(p).map_err(|e| format!("segment illisible : {e}"))?;
                out.write_all(&data).map_err(|e| e.to_string())?;
            }
        }
        let mut names = Vec::new();
        let mut wavs = Vec::new();
        if let Some(audio) = &self.audio {
            for (i, t) in audio.tracks.iter().enumerate() {
                let wav = work.join(format!("t{i}.wav"));
                audio.write_wav(i, start, end, self.cfg.audio_offset_ms, &wav)?;
                names.push(t.kind.label().to_string());
                wavs.push(wav);
            }
        }

        let clips_dir = PathBuf::from(&self.cfg.clips_dir);
        std::fs::create_dir_all(&clips_dir)
            .map_err(|e| format!("Dossier des clips inaccessible : {e}"))?;
        let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string();
        let mut out = clips_dir.join(format!("Clip_{stamp}.mp4"));
        let mut k = 2;
        while out.exists() {
            out = clips_dir.join(format!("Clip_{stamp}_{k}.mp4"));
            k += 1;
        }
        let side = sidecar_dir(&out);
        std::fs::create_dir_all(&side).map_err(|e| e.to_string())?;

        let n = wavs.len();
        let mix_first = n > 1;
        let mut cmd = ffmpeg::command(&self.ffmpeg);
        cmd.args(["-loglevel", "error", "-y", "-i"]).arg(&ts);
        for w in &wavs {
            cmd.arg("-i").arg(w);
        }
        if mix_first {
            let ins: String = (1..=n).map(|i| format!("[{i}:a]")).collect();
            cmd.arg("-filter_complex")
                .arg(format!("{ins}amix=inputs={n}:normalize=0,alimiter=limit=0.97[mix]"));
        }
        // Sortie 1 : le clip (vidéo copiée telle quelle, mixage + pistes séparées).
        cmd.args(["-map", "0:v:0"]);
        let mut a = 0;
        if mix_first {
            cmd.args(["-map", "[mix]", "-metadata:s:a:0", "title=Mixage", "-metadata:s:a:0", "handler_name=Mixage"]);
            a = 1;
        }
        for (i, name) in names.iter().enumerate() {
            cmd.args(["-map", &format!("{}:a", i + 1)]);
            cmd.arg(format!("-metadata:s:a:{}", a + i)).arg(format!("title={name}"));
            cmd.arg(format!("-metadata:s:a:{}", a + i)).arg(format!("handler_name={name}"));
        }
        cmd.args(["-c:v", "copy", "-c:a", "aac", "-b:a", "160k", "-movflags", "+faststart"]);
        cmd.arg(&out);
        // Sorties annexes : une piste par fichier pour l'éditeur, et la vignette.
        for i in 0..n {
            cmd.args(["-map", &format!("{}:a", i + 1), "-c:a", "aac", "-b:a", "160k"]);
            cmd.arg(side.join(format!("track{i}.m4a")));
        }
        cmd.args(["-map", "0:v:0", "-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"]);
        cmd.arg(side.join("thumb.jpg"));

        let log = work.join("mux.log");
        let status = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(&log).map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
            .status()
            .map_err(|e| format!("ffmpeg introuvable : {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_file(&out);
            let _ = std::fs::remove_dir_all(&side);
            return Err(format!("Échec de l'assemblage du clip.\n{}", ffmpeg::log_tail(&log, 4)));
        }
        let audio_streams = (n + usize::from(mix_first)) as u32;
        let meta = ClipMeta { tracks: names, mix_first, duration, audio_streams };
        let _ = std::fs::write(side.join("meta.json"), serde_json::to_string(&meta).unwrap_or_default());
        clip_info(&out).ok_or_else(|| "Clip introuvable après écriture.".to_string())
    }
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn clip_info(path: &Path) -> Option<ClipInfo> {
    let md = std::fs::metadata(path).ok()?;
    let side = sidecar_dir(path);
    let meta: ClipMeta = std::fs::read_to_string(side.join("meta.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let thumb = side.join("thumb.jpg");
    let tracks = meta
        .tracks
        .iter()
        .enumerate()
        .map(|(i, name)| TrackInfo {
            name: name.clone(),
            file: side.join(format!("track{i}.m4a")).to_string_lossy().into_owned(),
        })
        .collect();
    Some(ClipInfo {
        name: path.file_stem()?.to_string_lossy().into_owned(),
        path: path.to_string_lossy().into_owned(),
        thumb: thumb.exists().then(|| thumb.to_string_lossy().into_owned()),
        duration: meta.duration,
        created_ms: md.modified().map(millis).unwrap_or(0),
        size: md.len(),
        tracks,
        mix_first: meta.mix_first,
        source: None,
    })
}

fn is_video(p: &Path) -> bool {
    p.extension()
        .map(|x| matches!(x.to_string_lossy().to_lowercase().as_str(), "mp4" | "mkv" | "mov" | "webm"))
        .unwrap_or(false)
}

fn scan(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            let hidden = p.file_name().map(|n| n.to_string_lossy().starts_with('.')).unwrap_or(true);
            if depth > 0 && !hidden {
                scan(&p, depth - 1, out);
            }
        } else if is_video(&p) {
            out.push(p);
        }
    }
}

/// Clips du dossier principal et des dossiers supplémentaires (sous-dossiers
/// compris, par exemple un dossier par jeu), du plus récent au plus ancien.
pub fn list_clips(ffmpeg: &Path, dir: &Path, extra: &[String]) -> Vec<ClipInfo> {
    let mut v: Vec<ClipInfo> = Vec::new();
    let mut files = Vec::new();
    scan(dir, 0, &mut files);
    let main_count = files.len();
    for d in extra {
        scan(Path::new(d), 3, &mut files);
    }
    for (i, p) in files.iter().enumerate() {
        if let Some(mut info) = clip_info(p) {
            // Fichier venu d'ailleurs (ou export) : on crée durée et vignette une fois.
            if info.duration == 0.0 || info.thumb.is_none() {
                ensure_sidecar(ffmpeg, p, &mut info);
            }
            if i >= main_count {
                info.source = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
            }
            v.push(info);
        }
    }
    v.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
    v.dedup_by(|a, b| a.path == b.path);
    v
}

fn ensure_sidecar(ffmpeg: &Path, clip: &Path, info: &mut ClipInfo) {
    let side = sidecar_dir(clip);
    if std::fs::create_dir_all(&side).is_err() {
        return;
    }
    if info.duration == 0.0 {
        if let Ok(m) = ffmpeg::probe(ffmpeg, clip) {
            info.duration = m.duration;
            let meta = ClipMeta { tracks: vec![], mix_first: false, duration: m.duration, audio_streams: m.audio_streams };
            let _ = std::fs::write(side.join("meta.json"), serde_json::to_string(&meta).unwrap_or_default());
        }
    }
    if info.thumb.is_none() {
        let thumb = side.join("thumb.jpg");
        let ok = ffmpeg::command(ffmpeg)
            .args(["-loglevel", "error", "-y", "-i"])
            .arg(clip)
            .args(["-frames:v", "1", "-vf", "scale=480:-2", "-q:v", "4"])
            .arg(&thumb)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            info.thumb = Some(thumb.to_string_lossy().into_owned());
        }
    }
}

/// Prépare un clip pour l'éditeur. Pour un fichier à plusieurs pistes audio
/// enregistré par un autre logiciel (SteelSeries GG, OBS…), chaque piste est
/// extraite une fois afin d'être réglable séparément.
pub fn prepare_clip(ffmpeg: &Path, clip: &Path) -> Result<ClipInfo, String> {
    let mut info = clip_info(clip).ok_or("Clip introuvable.")?;
    ensure_sidecar(ffmpeg, clip, &mut info);
    if !info.tracks.is_empty() {
        return Ok(info);
    }
    let m = ffmpeg::probe(ffmpeg, clip)?;
    if m.audio_streams < 2 {
        return Ok(info);
    }
    // Une première piste intitulée « mix » contient déjà les autres : on ne la rejoue pas.
    let title = |i: usize| m.audio_titles.get(i).cloned().unwrap_or_default();
    let first = title(0).to_lowercase();
    let mix_first = ["mix", "combin", "master", "all audio"].iter().any(|k| first.contains(k));
    let skip = usize::from(mix_first);
    let side = sidecar_dir(clip);
    let mut cmd = ffmpeg::command(ffmpeg);
    cmd.args(["-loglevel", "error", "-y", "-i"]).arg(clip);
    let mut names = Vec::new();
    for (j, i) in (skip..m.audio_streams as usize).enumerate() {
        cmd.args(["-map", &format!("0:a:{i}"), "-vn", "-c:a", "aac", "-b:a", "160k"]);
        cmd.arg(side.join(format!("track{j}.m4a")));
        let t = title(i);
        names.push(if t.is_empty() { format!("Piste {}", i + 1) } else { t });
    }
    let ok = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        return Err("Impossible de séparer les pistes audio de ce fichier.".into());
    }
    let meta = ClipMeta { tracks: names, mix_first, duration: m.duration, audio_streams: m.audio_streams };
    std::fs::write(side.join("meta.json"), serde_json::to_string(&meta).unwrap_or_default())
        .map_err(|e| e.to_string())?;
    clip_info(clip).ok_or_else(|| "Clip introuvable.".to_string())
}

/// Chemin libre pour une copie montée d'un clip, dans le dossier des clips.
pub fn copy_path(clips_dir: &Path, name: &str) -> PathBuf {
    let mut out = clips_dir.join(format!("{name} (copie).mp4"));
    let mut k = 2;
    while out.exists() {
        out = clips_dir.join(format!("{name} (copie {k}).mp4"));
        k += 1;
    }
    out
}

pub fn delete_clip(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path).map_err(|e| format!("Suppression impossible : {e}"))?;
    let _ = std::fs::remove_dir_all(sidecar_dir(path));
    Ok(())
}

pub fn rename_clip(path: &Path, new_name: &str) -> Result<ClipInfo, String> {
    let clean: String = new_name
        .trim()
        .chars()
        .filter(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') && !c.is_control())
        .collect();
    let clean = clean.trim_matches(|c| c == '.' || c == ' ').to_string();
    if clean.is_empty() {
        return Err("Nom invalide.".into());
    }
    let ext = path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
    let target = path.with_file_name(format!("{clean}.{ext}"));
    if target == path {
        return clip_info(path).ok_or_else(|| "Clip introuvable.".to_string());
    }
    if target.exists() {
        return Err("Un clip porte déjà ce nom.".into());
    }
    let old_side = sidecar_dir(path);
    std::fs::rename(path, &target).map_err(|e| format!("Renommage impossible : {e}"))?;
    if old_side.exists() {
        let _ = std::fs::rename(&old_side, sidecar_dir(&target));
    }
    clip_info(&target).ok_or_else(|| "Clip introuvable.".to_string())
}
