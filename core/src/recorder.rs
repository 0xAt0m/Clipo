//! Buffer vidéo roulant : ffmpeg capture l'écran et écrit des segments MPEG-TS
//! de 2 secondes ; les plus anciens sont supprimés au fur et à mesure.

use crate::config::Config;
use crate::ffmpeg::{self, Pipeline};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

pub const SEGMENT_SECS: u64 = 2;

#[derive(Clone, Debug)]
pub struct Segment {
    pub path: PathBuf,
    pub run: u32,
    pub index: u32,
    pub created: SystemTime,
}

pub struct Recorder {
    dir: PathBuf,
    stop: Arc<AtomicBool>,
    /// Nombre de redémarrages consécutifs ratés (diagnostic).
    pub failures: Arc<AtomicU32>,
    thread: Option<JoinHandle<()>>,
}

fn spawn(ffmpeg: &Path, cfg: &Config, enc: Pipeline, dir: &Path, run: u32) -> std::io::Result<Child> {
    let log = File::create(dir.join("recorder.log"))?;
    let pattern = dir.join(format!("r{run:04}_%06d.ts"));
    ffmpeg::command(ffmpeg)
        .args(["-loglevel", "warning", "-y"])
        .args(ffmpeg::capture_args(cfg, enc))
        .args(["-an", "-f", "segment", "-segment_format", "mpegts"])
        .args(["-segment_time", &SEGMENT_SECS.to_string(), "-reset_timestamps", "0"])
        .arg(pattern)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
}

fn quit(child: &mut Child) {
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"q");
    }
    for _ in 0..30 {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn list_segments(dir: &Path) -> Vec<Segment> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return v };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // r0001_000042.ts
        let Some(stem) = name.strip_suffix(".ts").and_then(|s| s.strip_prefix('r')) else { continue };
        let Some((run, index)) = stem.split_once('_') else { continue };
        let (Ok(run), Ok(index)) = (run.parse(), index.parse()) else { continue };
        let Ok(meta) = e.metadata() else { continue };
        let Ok(created) = meta.created().or_else(|_| meta.modified()) else { continue };
        v.push(Segment { path: e.path(), run, index, created });
    }
    v.sort_by_key(|s| (s.run, s.index));
    v
}

impl Recorder {
    pub fn start(ffmpeg: PathBuf, cfg: Config, enc: Pipeline, dir: PathBuf) -> Recorder {
        let stop = Arc::new(AtomicBool::new(false));
        let failures = Arc::new(AtomicU32::new(0));
        let (stop2, failures2, dir2) = (stop.clone(), failures.clone(), dir.clone());
        let thread = std::thread::spawn(move || {
            let keep = Duration::from_secs(cfg.buffer_seconds as u64 + 3 * SEGMENT_SECS + 4);
            let mut run = 0u32;
            while !stop2.load(Ordering::Relaxed) {
                run += 1;
                let started = SystemTime::now();
                let mut child = match spawn(&ffmpeg, &cfg, enc, &dir2, run) {
                    Ok(c) => c,
                    Err(_) => {
                        failures2.fetch_add(1, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                };
                // Supervision : purge des vieux segments, relance si ffmpeg s'arrête
                // (changement de résolution, invite UAC, écran verrouillé…).
                loop {
                    if stop2.load(Ordering::Relaxed) {
                        quit(&mut child);
                        break;
                    }
                    if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                        let lived = started.elapsed().unwrap_or_default();
                        if lived > Duration::from_secs(10) {
                            failures2.store(0, Ordering::Relaxed);
                        } else {
                            failures2.fetch_add(1, Ordering::Relaxed);
                        }
                        // Attente croissante pour ne pas marteler le système.
                        let wait = failures2.load(Ordering::Relaxed).min(10) as u64;
                        for _ in 0..(1 + wait) * 10 {
                            if stop2.load(Ordering::Relaxed) {
                                break;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        break;
                    }
                    let now = SystemTime::now();
                    for s in list_segments(&dir2) {
                        let old = now.duration_since(s.created).map(|d| d > keep).unwrap_or(false);
                        if old || s.run + 1 < run {
                            let _ = std::fs::remove_file(&s.path);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
        Recorder { dir, stop, failures, thread: Some(thread) }
    }

    pub fn segments(&self) -> Vec<Segment> {
        list_segments(&self.dir)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        for s in list_segments(&self.dir) {
            let _ = std::fs::remove_file(&s.path);
        }
    }
}
