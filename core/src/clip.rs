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
            cmd.args(["-map", "[mix]", "-metadata:s:a:0", "title=Mixage"]);
            a = 1;
        }
        for (i, name) in names.iter().enumerate() {
            cmd.args(["-map", &format!("{}:a", i + 1)]);
            cmd.arg(format!("-metadata:s:a:{}", a + i)).arg(format!("title={name}"));
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
        let meta = ClipMeta { tracks: names, mix_first, duration };
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
    })
}

/// Clips du dossier, du plus récent au plus ancien.
pub fn list_clips(ffmpeg: &Path, dir: &Path) -> Vec<ClipInfo> {
    let mut v: Vec<ClipInfo> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return v };
    for e in rd.flatten() {
        let p = e.path();
        let is_video = p
            .extension()
            .map(|x| matches!(x.to_string_lossy().to_lowercase().as_str(), "mp4" | "mkv" | "mov" | "webm"))
            .unwrap_or(false);
        if !is_video {
            continue;
        }
        if let Some(mut info) = clip_info(&p) {
            // Fichier venu d'ailleurs (ou export) : on crée durée et vignette une fois.
            if info.duration == 0.0 || info.thumb.is_none() {
                ensure_sidecar(ffmpeg, &p, &mut info);
            }
            v.push(info);
        }
    }
    v.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
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
            let meta = ClipMeta { tracks: vec![], mix_first: false, duration: m.duration };
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
