//! Capture audio. Sous Windows : WASAPI, avec capture par application
//! (Windows 10 version 2004 ou plus récent), sans pilote virtuel.
//!  - Vocal : uniquement l'arbre de processus de l'application vocale ;
//!  - Jeu   : uniquement l'arbre de processus du jeu détecté ; si aucun jeu
//!            n'est détecté, tout le son du système SAUF l'application vocale ;
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
        let game = shared.game_pid.load(Ordering::Relaxed);
        if let Err(e) = win::run(&track, &shared, voice, game) {
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
    pub fn run(track: &Track, shared: &Shared, voice: u32, game: u32) -> Result<(), String> {
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
            Kind::Game if game != 0 => {
                // Jeu détecté : on n'enregistre que lui (la musique à côté reste hors du clip).
                AudioClient::new_application_loopback_client(game, true).map_err(err)?
            }
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
            let voice_now = shared.voice_pid.load(Ordering::Relaxed);
            let game_now = shared.game_pid.load(Ordering::Relaxed);
            let changed = match track.kind {
                Kind::Mic => false,
                Kind::Voice => voice_now != voice,
                // Sans jeu détecté, la piste dépend de l'application vocale à exclure.
                Kind::Game => game_now != game || (game == 0 && voice_now != voice),
            };
            if changed {
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

/// Fenêtre au premier plan.
#[derive(Clone, Copy, Debug, Default)]
pub struct Foreground {
    pub hwnd: u64,
    pub pid: u32,
    /// La fenêtre couvre tout son écran (plein écran ou fenêtré sans bordure).
    pub fullscreen: bool,
}

#[cfg(windows)]
mod win32 {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct Rect {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct MonitorInfo {
        pub cb_size: u32,
        pub rc_monitor: Rect,
        pub rc_work: Rect,
        pub flags: u32,
    }
    pub type MonitorEnumProc = unsafe extern "system" fn(isize, isize, *mut Rect, isize) -> i32;
    #[link(name = "user32")]
    extern "system" {
        pub fn GetForegroundWindow() -> isize;
        pub fn GetWindowThreadProcessId(hwnd: isize, pid: *mut u32) -> u32;
        pub fn GetWindowRect(hwnd: isize, rect: *mut Rect) -> i32;
        pub fn IsWindow(hwnd: isize) -> i32;
        pub fn IsIconic(hwnd: isize) -> i32;
        pub fn MonitorFromWindow(hwnd: isize, flags: u32) -> isize;
        pub fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
        pub fn EnumDisplayMonitors(hdc: isize, clip: *const Rect, callback: MonitorEnumProc, data: isize) -> i32;
    }
}

#[cfg(windows)]
pub fn foreground() -> Option<Foreground> {
    use win32::*;
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let mut rect = Rect::default();
        let mut fullscreen = false;
        if GetWindowRect(hwnd, &mut rect) != 0 {
            // MONITOR_DEFAULTTONEAREST
            let monitor = MonitorFromWindow(hwnd, 2);
            let mut info = MonitorInfo { cb_size: std::mem::size_of::<MonitorInfo>() as u32, ..Default::default() };
            if monitor != 0 && GetMonitorInfoW(monitor, &mut info) != 0 {
                let m = info.rc_monitor;
                fullscreen = rect.left <= m.left && rect.top <= m.top && rect.right >= m.right && rect.bottom >= m.bottom;
            }
        }
        Some(Foreground { hwnd: hwnd as u64, pid, fullscreen })
    }
}

/// La fenêtre existe-t-elle encore ?
#[cfg(windows)]
pub fn window_alive(hwnd: u64) -> bool {
    unsafe { win32::IsWindow(hwnd as isize) != 0 }
}

/// La fenêtre est-elle réduite dans la barre des tâches ?
#[cfg(windows)]
pub fn window_minimized(hwnd: u64) -> bool {
    unsafe { win32::IsIconic(hwnd as isize) != 0 }
}

/// Position et taille (x, y, largeur, hauteur) d'un écran ; 0 = écran principal.
#[cfg(windows)]
pub fn monitor_rect(index: u32) -> Option<(i32, i32, i32, i32)> {
    use win32::*;
    unsafe extern "system" fn collect(_monitor: isize, _hdc: isize, rect: *mut Rect, data: isize) -> i32 {
        let list = &mut *(data as *mut Vec<Rect>);
        if !rect.is_null() {
            list.push(*rect);
        }
        1
    }
    let mut list: Vec<Rect> = Vec::new();
    unsafe {
        EnumDisplayMonitors(0, std::ptr::null(), collect, &mut list as *mut Vec<Rect> as isize);
    }
    // L'écran principal a son coin en (0, 0) ; les autres suivent de gauche à droite.
    list.sort_by_key(|r| (!(r.left == 0 && r.top == 0), r.left, r.top));
    list.get(index as usize)
        .or(list.first())
        .map(|r| (r.left, r.top, r.right - r.left, r.bottom - r.top))
}

#[cfg(not(windows))]
pub fn foreground() -> Option<Foreground> {
    None
}

#[cfg(not(windows))]
pub fn window_alive(_hwnd: u64) -> bool {
    false
}

#[cfg(not(windows))]
pub fn window_minimized(_hwnd: u64) -> bool {
    false
}

#[cfg(not(windows))]
pub fn monitor_rect(_index: u32) -> Option<(i32, i32, i32, i32)> {
    None
}
