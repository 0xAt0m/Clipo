//! Capture audio. Sous Windows : WASAPI, avec capture par application
//! (Windows 10 version 2004 ou plus récent), sans pilote virtuel.
//!  - Vocal : uniquement l'arbre de processus de l'application vocale ;
//!  - Jeu   : tout le son du système SAUF l'application vocale ;
//!  - Micro : périphérique d'entrée.

#[cfg(windows)]
use crate::audio::nap;
use crate::audio::{Kind, Shared, Track};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

#[cfg(windows)]
pub fn capture_loop(track: Arc<Track>, shared: Arc<Shared>) {
    let _ = wasapi::initialize_mta();
    while !shared.stop.load(Ordering::Relaxed) {
        let voice = shared.voice_pid.load(Ordering::Relaxed);
        if track.kind == Kind::Voice && voice == 0 {
            track.active.store(false, Ordering::Relaxed);
            nap(&shared, Duration::from_secs(1));
            continue;
        }
        if let Err(e) = win::run(&track, &shared, voice) {
            eprintln!("[clipo] capture {} : {e}", track.kind.label());
        }
        track.active.store(false, Ordering::Relaxed);
        nap(&shared, Duration::from_secs(1));
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use crate::audio::FRAME;
    use std::time::Instant;
    use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat};

    fn err<E: std::fmt::Debug>(e: E) -> String {
        format!("{e:?}")
    }

    /// Capture jusqu'à l'arrêt, une erreur, ou un changement de l'application vocale.
    pub fn run(track: &Track, shared: &Shared, voice: u32) -> Result<(), String> {
        let format = WaveFormat::new(16, 16, SampleType::Int, 48_000, 2, None);
        let mut client = match track.kind {
            Kind::Mic => {
                let en = DeviceEnumerator::new().map_err(err)?;
                let dev = if shared.mic == "default" || shared.mic.is_empty() {
                    en.get_default_device(Direction::Capture).map_err(err)?
                } else {
                    en.get_device_collection(Direction::Capture)
                        .map_err(err)?
                        .get_device_with_name(&shared.mic)
                        .map_err(err)?
                };
                dev.get_iaudioclient().map_err(err)?
            }
            Kind::Voice => AudioClient::new_application_loopback_client(voice, true).map_err(err)?,
            Kind::Game => {
                // Mode « exclusion » : tout sauf l'arbre du processus cible. Sans
                // application vocale, on exclut Clipo lui-même (qui est muet).
                let target = if voice != 0 { voice } else { std::process::id() };
                AudioClient::new_application_loopback_client(target, false).map_err(err)?
            }
        };
        let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 0 };
        client
            .initialize_client(&format, Direction::Capture, mode, None)
            .map_err(err)?;
        let event = client.set_get_eventhandle().map_err(err)?;
        let capture = client.get_audiocaptureclient().map_err(err)?;
        client.start_stream().map_err(err)?;
        track.active.store(true, Ordering::Relaxed);

        let mut buf = vec![0u8; 48_000 * FRAME];
        let result = loop {
            if shared.stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            if track.kind != Kind::Mic && shared.voice_pid.load(Ordering::Relaxed) != voice {
                break Ok(());
            }
            let mut failed = None;
            loop {
                match capture.get_next_packet_size() {
                    Ok(Some(n)) if n > 0 => {
                        let need = n as usize * FRAME;
                        if buf.len() < need {
                            buf.resize(need, 0);
                        }
                        match capture.read_from_device(&mut buf) {
                            Ok((frames, _)) => {
                                track.push(&buf[..frames as usize * FRAME], Instant::now())
                            }
                            Err(e) => {
                                failed = Some(err(e));
                                break;
                            }
                        }
                    }
                    Ok(_) => break,
                    Err(e) => {
                        failed = Some(err(e));
                        break;
                    }
                }
            }
            if let Some(e) = failed {
                break Err(e);
            }
            // Une application silencieuse n'émet aucun événement : l'expiration est normale.
            let _ = event.wait_for_event(200);
        };
        let _ = client.stop_stream();
        result
    }

    pub fn list_mics() -> Vec<String> {
        let _ = wasapi::initialize_mta();
        let mut v = Vec::new();
        if let Ok(en) = DeviceEnumerator::new() {
            if let Ok(col) = en.get_device_collection(Direction::Capture) {
                let n = col.get_nbr_devices().unwrap_or(0);
                for i in 0..n {
                    if let Ok(d) = col.get_device_at_index(i) {
                        if let Ok(name) = d.get_friendlyname() {
                            v.push(name);
                        }
                    }
                }
            }
        }
        v
    }
}

#[cfg(windows)]
pub fn list_mics() -> Vec<String> {
    // Fil dédié : l'initialisation COM ne doit pas toucher le fil appelant.
    std::thread::spawn(win::list_mics).join().unwrap_or_default()
}

/// Hors Windows (développement) : chaque piste reçoit une sinusoïde distincte.
#[cfg(not(windows))]
pub fn capture_loop(track: Arc<Track>, shared: Arc<Shared>) {
    use std::time::Instant;
    let freq = match track.kind {
        Kind::Game => 330.0f32,
        Kind::Voice => 550.0,
        Kind::Mic => 880.0,
    };
    track.active.store(true, Ordering::Relaxed);
    let mut phase = 0u64;
    let start = Instant::now();
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        let target = start.elapsed().as_micros() as u64 * 48 / 1000;
        let n = (target - phase) as usize;
        let mut buf = Vec::with_capacity(n * 4);
        for i in 0..n {
            let t = (phase + i as u64) as f32 / 48_000.0;
            let v = ((t * freq * std::f32::consts::TAU).sin() * 8000.0) as i16;
            buf.extend_from_slice(&v.to_le_bytes());
            buf.extend_from_slice(&v.to_le_bytes());
        }
        phase = target;
        track.push(&buf, Instant::now());
    }
}

#[cfg(not(windows))]
pub fn list_mics() -> Vec<String> {
    Vec::new()
}
