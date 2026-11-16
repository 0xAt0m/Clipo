//! Test de bout en bout du moteur (hors Windows : mire vidéo + sinusoïdes).
use clipo_core::{clip, config::Config, export, ffmpeg, Engine};
use std::path::{Path, PathBuf};

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or("/tmp/clipo-test".into()));
    let _ = std::fs::remove_dir_all(&out);
    let ff = PathBuf::from("ffmpeg");
    let mut cfg = Config::default();
    cfg.clips_dir = out.to_string_lossy().into_owned();
    cfg.buffer_seconds = 10;
    cfg.fps = 30;
    let mut eng = Engine::new(cfg, ff.clone());
    eng.start_buffer().expect("start");
    println!("status: {:?}", eng.status());
    std::thread::sleep(std::time::Duration::from_secs(15));
    let job = eng.clip_job().expect("job");
    let t = std::time::Instant::now();
    let info = job.run().expect("clip");
    println!("clip en {:?}: {:#?}", t.elapsed(), info);
    let m = ffmpeg::probe(&ff, Path::new(&info.path)).unwrap();
    println!("probe clip: {m:?}");
    assert_eq!(m.audio_streams, 4);
    assert!((m.duration - info.duration).abs() < 0.3, "durée {} vs {}", m.duration, info.duration);
    assert!(info.duration >= 10.0 && info.duration <= 13.0);
    eng.stop_buffer();

    // Montage : on retire le milieu, on baisse une piste, on coupe une autre.
    let project = export::Project {
        main: info.path.clone(),
        track_count: info.tracks.len(),
        mix_first: info.mix_first,
        volumes: vec![1.0, 0.0, 0.5],
        pieces: vec![
            export::Piece { src: None, start: 0.0, end: 3.0, volume: 1.0 },
            export::Piece { src: Some(info.path.clone()), start: 1.0, end: 2.5, volume: 0.8 },
            export::Piece { src: None, start: 6.0, end: 9.0, volume: 1.0 },
        ],
        overlays: std::env::args().nth(2).map(|png| vec![export::Overlay { png, start: 1.0, end: 4.0 }]).unwrap_or_default(),
        output: out.join("export.mp4").to_string_lossy().into_owned(),
    };
    let r = export::run(&ff, &project, &out.join("export.log"), &|p| print!("\r{:.0}%", p * 100.0)).expect("export");
    let m = ffmpeg::probe(&ff, &r).unwrap();
    println!("\nprobe export: {m:?}");
    assert!((m.duration - 7.5).abs() < 0.2);
    let list = clip::list_clips(&ff, &out);
    println!("bibliothèque: {:?}", list.iter().map(|c| (&c.name, c.duration, c.thumb.is_some())).collect::<Vec<_>>());
    let renamed = clip::rename_clip(Path::new(&info.path), "Mon clip: test?").unwrap();
    println!("renommé: {} / pistes {:?}", renamed.path, renamed.tracks.iter().map(|t| Path::new(&t.file).exists()).collect::<Vec<_>>());
    println!("OK");
}
