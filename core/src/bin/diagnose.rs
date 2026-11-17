//! Diagnostic de la capture : essaie chaque méthode et affiche le journal.
//! Usage : diagnose <chemin de ffmpeg>
use clipo_core::{config::Config, ffmpeg, recorder};
use std::path::PathBuf;

fn main() {
    let ff = PathBuf::from(std::env::args().nth(1).unwrap_or("ffmpeg".into()));
    let dir = std::env::temp_dir().join("clipo-diagnostic");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config::default();
    let log = dir.join("detect.log");
    let pipe = ffmpeg::detect_pipeline(&ff, &cfg, &log, 0);
    println!("Méthode retenue : {pipe:?}");
    println!("{}", std::fs::read_to_string(&log).unwrap_or_default());
    let Some(pipe) = pipe else { std::process::exit(1) };
    println!("Arguments : {:?}", ffmpeg::capture_args(&cfg, pipe, 0));
    let rec = recorder::Recorder::start(ff, cfg, pipe, dir.clone(), Default::default());
    std::thread::sleep(std::time::Duration::from_secs(7));
    let segs = rec.segments();
    println!("Segments après 7 s : {}", segs.len());
    println!("--- recorder.log ---\n{}", std::fs::read_to_string(dir.join("recorder.log")).unwrap_or_default());
    rec.stop();
    if segs.len() < 2 {
        std::process::exit(2);
    }
}
