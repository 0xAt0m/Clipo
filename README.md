# Clipo

Clips de jeu et montage de base pour Windows 10/11, en version minimaliste.

- **Un raccourci** (Alt + F10 par défaut) enregistre les X dernières secondes ou minutes.
- **Pistes son séparées** : Jeu, Vocal (Discord), Micro — sans pilote ni câble virtuel.
- **Montage simple** : volume par piste, coupes (y compris au milieu), texte, image, vidéo insérée.
- **Léger** : en fond, seuls un petit processus et l'encodeur matériel du GPU tournent. La fenêtre (et son moteur web) est détruite dès qu'on la ferme.

## Installer

1. Onglet **Actions** du dépôt → dernier build réussi → artefact **Clipo-installateur**.
   (Ou onglet **Releases** si une version `v*` a été publiée.)
2. Lancer `Clipo_x.y.z_x64-setup.exe`. Windows SmartScreen affiche un avertissement
   (l'installateur n'est pas signé) : « Informations complémentaires » → « Exécuter quand même ».

Pour publier une version : `git tag v0.1.0 && git push --tags`.

## Utiliser

| Action | Comment |
|---|---|
| Enregistrer un clip | Raccourci global, bouton « Enregistrer un clip », ou clic droit sur l'icône de la zone de notification. Un son Windows confirme. |
| Mettre en pause | Clic sur la pastille « Actif » ou menu de l'icône. |
| Monter | Cliquer un clip. **Couper** (S) à la tête de lecture, cliquer un morceau, **Supprimer** (Suppr). Ctrl+Z annule. |
| Régler le son | Curseur et bouton muet par piste, à droite de l'aperçu. |
| Ajouter | **+ Texte**, **+ Image** (calques à déplacer à la souris), **+ Vidéo** (insérée à la tête de lecture). |
| Exporter | Crée `<nom>_montage_<heure>.mp4` à côté du clip. |

Les clips sont des `.mp4` standards : première piste audio = mixage (lisible partout),
pistes suivantes = Jeu / Vocal / Micro. Le dossier `.clipo` à côté des clips contient les
vignettes, les pistes pour l'aperçu et les projets de montage.

### Discord

Rien à configurer dans Discord. Windows fournit directement à Clipo le son de l'application
indiquée dans Réglages → Son → Application (`Discord.exe`). La piste « Jeu » reçoit tout le
reste du son du PC.

## Fonctionnement

```
core/   moteur (Rust, sans interface)
  recorder.rs  ffmpeg capture l'écran (ddagrab) → encodeur GPU → segments de 2 s, purge continue
  capture.rs   WASAPI : capture par application (vocal), « tout sauf » (jeu), micro
  audio.rs     tampon circulaire par piste, calé sur une horloge commune
  clip.rs      assemblage du clip (vidéo copiée sans réencodage) + bibliothèque
  export.rs    montage → commande ffmpeg
app/    application Tauri : zone de notification, raccourci global, commandes
ui/     interface (HTML/CSS/JS sans dépendance ni étape de build)
```

Encodeur choisi automatiquement : NVIDIA NVENC, AMD AMF, Intel Quick Sync, sinon x264.

## Limites connues

- **Windows 10 version 2004 minimum** (capture audio par application).
- Le buffer vidéo est écrit sur disque dans `%TEMP%\Clipo` (≈ 110 Mo par minute à 15 Mbit/s).
- Résolution « Native » = chemin sans copie, le plus léger. 1080p/720p passent par le processeur.
- Certains jeux en plein écran exclusif ou protégés par anti-triche peuvent bloquer la capture
  d'écran ou le raccourci : utiliser le mode fenêtré sans bordure.
- Si le son paraît décalé de l'image : Réglages → Décalage du son.
- L'aperçu du montage plafonne le volume à 100 % ; l'amplification (jusqu'à 200 %) s'applique à l'export.

## Développer

Prérequis : Rust, `cargo install tauri-cli --version "^2"`, et un `ffmpeg.exe` (build GPL de
[BtbN](https://github.com/BtbN/FFmpeg-Builds/releases)) copié dans `app/resources/`.

```
cargo test -p clipo-core      # tests du moteur
cd app && cargo tauri dev     # application en mode développement
cd app && cargo tauri build   # installateur dans target/release/bundle/nsis
```

ffmpeg est distribué sous licence GPL ; Clipo l'appelle comme programme séparé.
