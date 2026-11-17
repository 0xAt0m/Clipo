//! Signal sonore de confirmation : deux notes brèves, la seconde un ton plus haut.

const RATE: u32 = 44_100;

fn note(out: &mut Vec<i16>, freq: f32, ms: u32, gain: f32) {
    let n = RATE * ms / 1000;
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        // Attaque de 6 ms puis extinction douce : pas de clic, son feutré.
        let attack = (t / 0.006).min(1.0);
        let decay = (-t * 14.0).exp();
        let tail = ((n - i) as f32 / (RATE as f32 * 0.01)).min(1.0);
        let w = std::f32::consts::TAU * freq * t;
        let v = w.sin() + 0.25 * (2.0 * w).sin();
        out.push((v * attack * decay * tail * gain * 32767.0) as i16);
    }
}

/// Fichier WAV du signal. `ok` : montant (la puis si) ; sinon descendant et plus grave.
pub fn chime(ok: bool) -> Vec<u8> {
    let mut pcm = Vec::new();
    let (a, b) = if ok { (880.0, 987.77) } else { (440.0, 392.0) };
    note(&mut pcm, a, 110, 0.16);
    pcm.extend(std::iter::repeat(0).take((RATE / 50) as usize));
    note(&mut pcm, b, 190, 0.16);
    wav(pcm)
}

/// Note unique et brève, distincte du signal de clip : aiguë quand le buffer
/// s'allume, grave quand il s'éteint.
pub fn blip(on: bool) -> Vec<u8> {
    let mut pcm = Vec::new();
    note(&mut pcm, if on { 1318.5 } else { 659.25 }, 140, 0.14);
    wav(pcm)
}

fn wav(pcm: Vec<i16>) -> Vec<u8> {
    let len = (pcm.len() * 2) as u32;
    let mut h = Vec::with_capacity(44 + len as usize);
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + len).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&RATE.to_le_bytes());
    h.extend_from_slice(&(RATE * 2).to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&len.to_le_bytes());
    for s in pcm {
        h.extend_from_slice(&s.to_le_bytes());
    }
    h
}

/// Écrit les sons intégrés et renvoie leurs chemins (succès, échec).
pub fn install(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let _ = std::fs::create_dir_all(dir);
    let ok = dir.join("clip.wav");
    let err = dir.join("erreur.wav");
    let _ = std::fs::write(&ok, chime(true));
    let _ = std::fs::write(&err, chime(false));
    (ok, err)
}

/// Écrit les sons d'activation et de désactivation du buffer et renvoie leurs chemins.
pub fn install_toggle(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let _ = std::fs::create_dir_all(dir);
    let on = dir.join("buffer-on.wav");
    let off = dir.join("buffer-off.wav");
    let _ = std::fs::write(&on, blip(true));
    let _ = std::fs::write(&off, blip(false));
    (on, off)
}
