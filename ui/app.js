'use strict';
const TAURI = window.__TAURI__;
const invoke = TAURI.core.invoke;
const fileSrc = TAURI.core.convertFileSrc;
const listen = TAURI.event.listen;
const dialog = TAURI.dialog;

const $ = (s, r = document) => r.querySelector(s);
const el = (tag, cls, text) => {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
};
const clamp = (v, a, b) => Math.min(b, Math.max(a, v));
let uidSeq = Date.now();
const uid = () => (uidSeq++).toString(36);

function fmt(t, tenths = true) {
  t = Math.max(0, t);
  const m = Math.floor(t / 60), s = t - m * 60;
  return tenths ? `${m}:${s.toFixed(1).padStart(4, '0')}` : `${m}:${String(Math.floor(s)).padStart(2, '0')}`;
}
function fmtBuffer(sec) {
  return sec < 60 ? `${sec} dernières secondes` : sec === 60 ? 'la dernière minute' : `${sec / 60} dernières minutes`;
}
function prettyKey(k) {
  return k.split('+').map(p => p.replace(/^Key|^Digit/, '').replace(/^Numpad/, 'Num ')).join(' + ');
}
function toast(msg, opts = {}) {
  const t = el('div', 'toast' + (opts.error ? ' err' : ''));
  t.append(el('span', null, msg));
  if (opts.action) {
    const a = el('a', null, opts.action.label);
    a.onclick = opts.action.run;
    t.append(a);
  }
  $('#toasts').append(t);
  setTimeout(() => t.remove(), opts.error ? 7000 : 4000);
}
const fail = e => toast(String(e), { error: true });

// ───────────────────────────── État général ─────────────────────────────
let cfg = null;
let status = null;
let clips = [];

async function refreshStatus() {
  try {
    [cfg, status] = await Promise.all([invoke('get_config'), invoke('get_status')]);
  } catch (e) { return; }
  const pill = $('#status');
  const key = prettyKey(cfg.hotkey);
  pill.className = 'pill';
  let text;
  if (!status.ffmpeg_ok) { pill.classList.add('warn'); text = 'ffmpeg introuvable'; }
  else if (status.video_trouble) { pill.classList.add('warn'); text = "Capture de l'écran impossible"; }
  else if (status.buffering) { pill.classList.add('on'); text = `Actif · ${fmt(cfg.buffer_seconds, false)} · ${key}`; }
  else text = cfg.buffer_enabled ? 'Démarrage…' : 'En pause';
  $('#status-text').textContent = text;
  $('#btn-clip').disabled = !status.buffering;
  $('#empty-key').textContent = key;
  $('#empty-len').textContent = (cfg.buffer_seconds === 60 ? '' : 'les ') + fmtBuffer(cfg.buffer_seconds);
  if (!$('#settings').hidden) {
    $('#s-encoder').textContent = status.encoder ? `Encodeur : ${status.encoder}` +
      (cfg.record_voice ? ` · ${cfg.voice_app} ${status.voice_detected ? 'détecté' : 'non détecté'}` : '') : '';
  }
}

async function setConfig(patch) {
  try {
    cfg = await invoke('set_config', { cfg: { ...cfg, ...patch } });
  } catch (e) {
    fail(e);
    cfg = await invoke('get_config');
    if (!$('#settings').hidden) fillSettings();
  }
  refreshStatus();
}

$('#status').onclick = () => cfg && setConfig({ buffer_enabled: !cfg.buffer_enabled });
$('#btn-clip').onclick = () => invoke('save_clip_now');

// ───────────────────────────── Bibliothèque ─────────────────────────────
async function refreshLibrary(freshPath) {
  try { clips = await invoke('list_clips'); } catch (e) { fail(e); return; }
  const grid = $('#grid');
  grid.textContent = '';
  $('#empty').hidden = clips.length > 0;
  for (const c of clips) {
    const card = el('div', 'card' + (c.path === freshPath ? ' fresh' : ''));
    card.tabIndex = 0;
    const thumb = el('div', 'thumb');
    if (c.thumb) thumb.style.backgroundImage = `url("${fileSrc(c.thumb)}")`;
    thumb.append(el('span', 'dur', fmt(c.duration, false)));
    const actions = el('div', 'actions');
    const bRename = el('button', 'btn', 'Renommer');
    const bShow = el('button', 'btn', 'Dossier');
    const bDel = el('button', 'btn danger', 'Supprimer');
    actions.append(bRename, bShow, bDel);
    thumb.append(actions);
    const meta = el('div', 'meta');
    const name = el('div', 'name', c.name);
    const date = el('div', 'date', new Date(c.created_ms).toLocaleString('fr-FR', { dateStyle: 'medium', timeStyle: 'short' }) +
      (c.tracks.length ? ` · ${c.tracks.length} pistes` : ''));
    meta.append(name, date);
    card.append(thumb, meta);
    card.onclick = () => openEditor(c);
    card.onkeydown = e => { if (e.key === 'Enter' && e.target === card) openEditor(c); };
    bShow.onclick = e => { e.stopPropagation(); invoke('reveal', { path: c.path }); };
    bDel.onclick = async e => {
      e.stopPropagation();
      const ok = await dialog.confirm(`Supprimer définitivement « ${c.name} » ?`, { title: 'Clipo', kind: 'warning' });
      if (!ok) return;
      try { await invoke('delete_clip', { path: c.path }); } catch (err) { fail(err); }
      refreshLibrary();
    };
    bRename.onclick = e => {
      e.stopPropagation();
      const input = el('input');
      input.type = 'text';
      input.value = c.name;
      name.textContent = '';
      name.append(input);
      input.onclick = ev => ev.stopPropagation();
      input.focus();
      input.select();
      let done = false;
      const commit = async save => {
        if (done) return;
        done = true;
        if (save && input.value.trim() && input.value.trim() !== c.name) {
          try { await invoke('rename_clip', { path: c.path, name: input.value }); } catch (err) { fail(err); }
        }
        refreshLibrary();
      };
      input.onkeydown = ev => {
        ev.stopPropagation();
        if (ev.key === 'Enter') commit(true);
        if (ev.key === 'Escape') commit(false);
      };
      input.onblur = () => commit(true);
    };
    grid.append(card);
  }
}

// ───────────────────────────── Éditeur ─────────────────────────────
const video = $('#video');
let ed = null;

const pieceLen = p => p.end - p.start;
const total = () => ed.project.pieces.reduce((a, p) => a + pieceLen(p), 0);
const offsetOf = i => ed.project.pieces.slice(0, i).reduce((a, p) => a + pieceLen(p), 0);
function locate(t) {
  const ps = ed.project.pieces;
  let off = 0;
  for (let i = 0; i < ps.length; i++) {
    const len = pieceLen(ps[i]);
    if (t < off + len || i === ps.length - 1) {
      return { i, p: ps[i], off, local: ps[i].start + clamp(t - off, 0, len) };
    }
    off += len;
  }
}

function extVideo(path) {
  let v = ed.ext.get(path);
  if (!v) {
    v = el('video');
    v.playsInline = true;
    v.preload = 'auto';
    v.src = fileSrc(path);
    v.style.visibility = 'hidden';
    $('#stage').insertBefore(v, $('#overlays'));
    ed.ext.set(path, v);
  }
  return v;
}
const elFor = p => (p.src ? extVideo(p.src) : video);
const allVideos = () => [video, ...ed.ext.values()];

function showOnly(v) {
  for (const o of allVideos()) {
    o.style.visibility = o === v ? 'visible' : 'hidden';
    if (o !== v) o.pause();
  }
}

function applyVolumes() {
  const pr = ed.project;
  const vol = i => (pr.muted[i] ? 0 : clamp(pr.volumes[i] ?? 1, 0, 1));
  if (ed.audios.length) {
    video.muted = true;
    ed.audios.forEach((a, i) => { a.volume = vol(i); });
  } else {
    video.muted = false;
    video.volume = vol(0);
  }
  for (const p of pr.pieces) if (p.src) extVideo(p.src).volume = clamp(p.volume ?? 1, 0, 1);
}

function syncAudio() {
  const p = ed.project.pieces[ed.cur];
  const onMain = p && !p.src;
  for (const a of ed.audios) {
    if (!onMain || !ed.playing) { if (!a.paused) a.pause(); continue; }
    if (Math.abs(a.currentTime - video.currentTime) > 0.12) a.currentTime = video.currentTime;
    if (a.paused && !video.paused) a.play().catch(() => {});
  }
}

function seek(t) {
  ed.T = clamp(t, 0, total());
  const loc = locate(ed.T);
  ed.cur = loc.i;
  const v = elFor(loc.p);
  showOnly(v);
  v.currentTime = loc.local;
  if (!loc.p.src) for (const a of ed.audios) a.currentTime = loc.local;
  if (ed.playing) v.play().catch(() => {});
  syncAudio();
}

function startPiece(i) {
  ed.cur = i;
  const p = ed.project.pieces[i];
  const v = elFor(p);
  showOnly(v);
  v.currentTime = p.start;
  if (!p.src) for (const a of ed.audios) a.currentTime = p.start;
  ed.T = offsetOf(i);
  v.play().catch(() => {});
}

function play() {
  if (ed.T >= total() - 0.05) ed.T = 0;
  ed.playing = true;
  seek(ed.T);
  $('#play').textContent = '❚❚';
}
function pause() {
  if (!ed) return;
  ed.playing = false;
  for (const v of allVideos()) v.pause();
  for (const a of ed.audios) a.pause();
  $('#play').textContent = '▶';
}

function tick() {
  if (!ed) return;
  if (ed.playing) {
    const ps = ed.project.pieces;
    const p = ps[ed.cur];
    const v = elFor(p);
    if (v.currentTime >= p.end - 0.015 || v.ended) {
      if (ed.cur + 1 < ps.length) startPiece(ed.cur + 1);
      else { pause(); ed.T = total(); }
    } else if (!v.seeking) {
      ed.T = offsetOf(ed.cur) + Math.max(0, v.currentTime - p.start);
    }
    syncAudio();
  }
  const tot = total();
  const lane = $('#tl-pieces');
  $('#playhead').style.left = `${lane.offsetLeft + (tot ? ed.T / tot : 0) * lane.clientWidth - 1}px`;
  $('#time').textContent = `${fmt(ed.T)} / ${fmt(tot)}`;
  for (const node of $('#overlays').children) {
    const o = ed.project.overlays.find(x => x.id === node.dataset.id);
    node.style.display = o && ed.T >= o.start && ed.T < o.end ? '' : 'none';
  }
  ed.raf = requestAnimationFrame(tick);
}

// Modifications : chaque action passe par ici (annulation + sauvegarde).
function mutate(fn) {
  ed.undo.push(JSON.stringify(ed.project));
  if (ed.undo.length > 60) ed.undo.shift();
  fn();
  changed();
}
function changed() {
  renderTimeline();
  renderOverlays();
  renderInspector();
  applyVolumes();
  scheduleSave();
}
function scheduleSave() {
  clearTimeout(ed.saveTimer);
  const clipPath = ed.clip.path, json = JSON.stringify(ed.project);
  ed.saveTimer = setTimeout(() => invoke('save_project', { clipPath, json }).catch(() => {}), 400);
}
function undo() {
  const prev = ed.undo.pop();
  if (!prev) return;
  ed.project = JSON.parse(prev);
  ed.sel = null;
  changed();
  seek(Math.min(ed.T, total()));
}

function splitAt(t) {
  const loc = locate(t);
  if (loc.local - loc.p.start < 0.1 || loc.p.end - loc.local < 0.1) return false;
  const a = { ...loc.p, id: uid(), end: loc.local };
  const b = { ...loc.p, id: uid(), start: loc.local };
  ed.project.pieces.splice(loc.i, 1, a, b);
  return true;
}

function doSplit() {
  const loc = locate(ed.T);
  if (loc.local - loc.p.start < 0.1 || loc.p.end - loc.local < 0.1) {
    toast('Placez la tête de lecture à l\'intérieur d\'un morceau.');
    return;
  }
  mutate(() => { splitAt(ed.T); ed.sel = null; });
  seek(ed.T);
}

function doDelete() {
  if (!ed.sel) { toast('Cliquez d\'abord un morceau ou un calque.'); return; }
  if (ed.sel.type === 'ov') {
    mutate(() => {
      ed.project.overlays = ed.project.overlays.filter(o => o.id !== ed.sel.id);
      ed.sel = null;
    });
    return;
  }
  if (ed.project.pieces.length < 2) { toast('Il doit rester au moins un morceau.'); return; }
  pause();
  const i = ed.project.pieces.findIndex(p => p.id === ed.sel.id);
  if (i < 0) return;
  const at = offsetOf(i);
  mutate(() => { ed.project.pieces.splice(i, 1); ed.sel = null; });
  seek(Math.min(at, total()));
}

async function addVideo() {
  const path = await dialog.open({ multiple: false, filters: [{ name: 'Vidéo', extensions: ['mp4', 'mkv', 'mov', 'webm', 'avi'] }] });
  if (!path) return;
  let info;
  try { info = await invoke('probe_media', { path }); } catch (e) { fail(e); return; }
  if (!info.has_video || !info.duration) { toast('Ce fichier ne contient pas de vidéo lisible.', { error: true }); return; }
  pause();
  const piece = { id: uid(), src: path, start: 0, end: info.duration, volume: 1 };
  const at = ed.T;
  mutate(() => {
    splitAt(at);
    const loc = locate(at);
    // Après la coupe, la tête est au début d'un morceau, sauf en toute fin de montage.
    const idx = at - loc.off < 0.1 ? loc.i : loc.i + 1;
    ed.project.pieces.splice(idx, 0, piece);
    ed.sel = { type: 'piece', id: piece.id };
  });
  seek(offsetOf(ed.project.pieces.indexOf(piece)));
}

function addText() {
  const o = { id: uid(), type: 'text', text: 'Texte', color: '#ffffff', size: 0.09, x: 0.5, y: 0.82,
    start: ed.T, end: Math.min(ed.T + 3, Math.max(total(), ed.T + 0.5)) };
  mutate(() => { ed.project.overlays.push(o); ed.sel = { type: 'ov', id: o.id }; });
  const ta = $('#insp-body textarea');
  if (ta) { ta.focus(); ta.select(); }
}

async function addImage() {
  const path = await dialog.open({ multiple: false, filters: [{ name: 'Image', extensions: ['png', 'jpg', 'jpeg', 'webp', 'gif', 'bmp'] }] });
  if (!path) return;
  try { await loadImage(path); } catch (e) { fail(e); return; }
  const o = { id: uid(), type: 'image', src: path, w: 0.3, x: 0.5, y: 0.5,
    start: ed.T, end: Math.min(ed.T + 3, Math.max(total(), ed.T + 0.5)) };
  mutate(() => { ed.project.overlays.push(o); ed.sel = { type: 'ov', id: o.id }; });
}

// Les images passent par des URL de données : pas de restriction d'origine à l'export.
async function loadImage(path) {
  if (ed.images.has(path)) return ed.images.get(path);
  const url = await invoke('read_data_url', { path });
  const img = new Image();
  await new Promise((res, rej) => { img.onload = res; img.onerror = () => rej('Image illisible.'); img.src = url; });
  ed.images.set(path, img);
  return img;
}

// ── Rendu
function renderTimeline() {
  const lane = $('#tl-pieces');
  lane.textContent = '';
  for (const p of ed.project.pieces) {
    const d = el('div', 'piece' + (p.src ? ' ext' : '') + (ed.sel?.type === 'piece' && ed.sel.id === p.id ? ' sel' : ''), fmt(pieceLen(p)));
    d.style.flex = `${pieceLen(p)} 1 0`;
    lane.append(d);
  }
  const ovs = $('#tl-overlays');
  ovs.textContent = '';
  const tot = total();
  for (const o of ed.project.overlays) {
    const b = el('div', 'ovbar' + (ed.sel?.type === 'ov' && ed.sel.id === o.id ? ' sel' : ''), o.type === 'text' ? o.text.split('\n')[0] : 'Image');
    b.style.left = `${(o.start / tot) * 100}%`;
    b.style.width = `${(Math.max(0.05, o.end - o.start) / tot) * 100}%`;
    const grip = el('div', 'grip');
    b.append(grip);
    b.onpointerdown = e => {
      e.stopPropagation();
      const resize = e.target === grip;
      const x0 = e.clientX, s0 = o.start, e0 = o.end, w = ovs.clientWidth;
      let pushed = false;
      select({ type: 'ov', id: o.id });
      drag(e, ev => {
        const dt = ((ev.clientX - x0) / w) * total();
        if (Math.abs(ev.clientX - x0) < 3 && !pushed) return;
        if (!pushed) { ed.undo.push(JSON.stringify(ed.project)); pushed = true; }
        const cur = ed.project.overlays.find(x => x.id === o.id);
        if (!cur) return;
        if (resize) cur.end = clamp(e0 + dt, s0 + 0.2, total());
        else {
          const len = e0 - s0;
          cur.start = clamp(s0 + dt, 0, Math.max(0, total() - len));
          cur.end = cur.start + len;
        }
        const node = [...ovs.children].find(n => n.classList.contains('sel'));
        if (node) {
          node.style.left = `${(cur.start / total()) * 100}%`;
          node.style.width = `${((cur.end - cur.start) / total()) * 100}%`;
        }
      }, () => { if (pushed) changed(); });
    };
    ovs.append(b);
  }
}

function drag(e, onMove, onEnd) {
  e.preventDefault();
  const move = ev => onMove(ev);
  const up = () => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    if (onEnd) onEnd();
  };
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

function select(sel) {
  ed.sel = sel;
  renderTimeline();
  for (const n of $('#overlays').children) n.classList.toggle('sel', sel?.type === 'ov' && sel.id === n.dataset.id);
  renderInspector();
}

function renderOverlays() {
  const box = $('#overlays');
  box.textContent = '';
  for (const o of ed.project.overlays) {
    const n = el('div', 'ov ' + o.type + (ed.sel?.type === 'ov' && ed.sel.id === o.id ? ' sel' : ''));
    n.dataset.id = o.id;
    n.style.left = `${o.x * 100}%`;
    n.style.top = `${o.y * 100}%`;
    if (o.type === 'text') {
      n.textContent = o.text;
      n.style.color = o.color;
      n.style.fontSize = `${o.size * 100}cqh`;
    } else {
      n.style.width = `${o.w * 100}%`;
      const img = el('img');
      loadImage(o.src).then(i => { img.src = i.src; }).catch(() => {});
      n.append(img);
    }
    n.onpointerdown = e => {
      e.stopPropagation();
      select({ type: 'ov', id: o.id });
      const r = box.getBoundingClientRect();
      const x0 = e.clientX, y0 = e.clientY, ox = o.x, oy = o.y;
      let pushed = false;
      drag(e, ev => {
        if (!pushed) {
          if (Math.abs(ev.clientX - x0) + Math.abs(ev.clientY - y0) < 3) return;
          ed.undo.push(JSON.stringify(ed.project));
          pushed = true;
        }
        const cur = ed.project.overlays.find(x => x.id === o.id);
        if (!cur) return;
        cur.x = clamp(ox + (ev.clientX - x0) / r.width, 0, 1);
        cur.y = clamp(oy + (ev.clientY - y0) / r.height, 0, 1);
        n.style.left = `${cur.x * 100}%`;
        n.style.top = `${cur.y * 100}%`;
      }, () => { if (pushed) scheduleSave(); });
    };
    box.append(n);
  }
}

function renderTracks() {
  const box = $('#tracks');
  box.textContent = '';
  const names = ed.clip.tracks.length ? ed.clip.tracks.map(t => t.name) : ['Son du clip'];
  names.forEach((name, i) => {
    const pr = ed.project;
    const row = el('div', 'track' + (pr.muted[i] ? ' muted' : ''));
    const val = el('span', 'tval', `${Math.round((pr.volumes[i] ?? 1) * 100)} %`);
    const line = el('div', 'line');
    const mute = el('button', 'btn mute', pr.muted[i] ? '🔇' : '🔊');
    mute.title = 'Couper / rétablir cette piste';
    const range = el('input');
    range.type = 'range'; range.min = 0; range.max = 2; range.step = 0.05; range.value = pr.volumes[i] ?? 1;
    range.setAttribute('aria-label', `Volume ${name}`);
    range.title = "Au-delà de 100 %, l'amplification s'entend dans le fichier exporté";
    let pushed = false;
    range.oninput = () => {
      if (!pushed) { ed.undo.push(JSON.stringify(pr)); pushed = true; }
      pr.volumes[i] = Number(range.value);
      val.textContent = `${Math.round(pr.volumes[i] * 100)} %`;
      applyVolumes();
      scheduleSave();
    };
    range.onchange = () => { pushed = false; };
    mute.onclick = () => mutate(() => { pr.muted[i] = !pr.muted[i]; });
    line.append(mute, range);
    row.append(el('span', 'tname', name), val, line);
    box.append(row);
  });
  const sel = ed.sel?.type === 'piece' && ed.project.pieces.find(p => p.id === ed.sel.id && p.src);
  if (sel) {
    const row = el('div', 'track');
    const range = el('input');
    range.type = 'range'; range.min = 0; range.max = 2; range.step = 0.05; range.value = sel.volume ?? 1;
    const val = el('span', 'tval', `${Math.round((sel.volume ?? 1) * 100)} %`);
    range.oninput = () => { sel.volume = Number(range.value); val.textContent = `${Math.round(sel.volume * 100)} %`; applyVolumes(); scheduleSave(); };
    const line = el('div', 'line');
    line.append(range);
    row.append(el('span', 'tname', 'Vidéo insérée'), val, line);
    box.append(row);
  }
}

function renderInspector() {
  const o = ed.sel?.type === 'ov' && ed.project.overlays.find(x => x.id === ed.sel.id);
  const panel = $('#inspector');
  // Le volume d'une vidéo insérée s'affiche dans le panneau Son.
  renderTracks();
  if (!o) { panel.hidden = true; panel.dataset.id = ''; return; }
  if (panel.dataset.id === o.id && !panel.hidden) return;
  panel.hidden = false;
  panel.dataset.id = o.id;
  $('#insp-title').textContent = o.type === 'text' ? 'Texte' : 'Image';
  const body = $('#insp-body');
  body.textContent = '';
  let pushed = false;
  const edit = fn => {
    if (!pushed) { ed.undo.push(JSON.stringify(ed.project)); pushed = true; }
    fn();
    renderOverlays();
    renderTimeline();
    scheduleSave();
  };
  const field = (label, input, inline) => {
    const f = el('label', 'field' + (inline ? ' inline' : ''));
    f.append(el('span', null, label), input);
    body.append(f);
    return input;
  };
  if (o.type === 'text') {
    const ta = field('Contenu', el('textarea'));
    ta.value = o.text;
    ta.oninput = () => edit(() => { o.text = ta.value; });
    const size = field('Taille', el('input'));
    size.type = 'range'; size.min = 0.03; size.max = 0.3; size.step = 0.005; size.value = o.size;
    size.oninput = () => edit(() => { o.size = Number(size.value); });
    const color = field('Couleur', el('input'), true);
    color.type = 'color'; color.value = o.color;
    color.oninput = () => edit(() => { o.color = color.value; });
  } else {
    const size = field('Taille', el('input'));
    size.type = 'range'; size.min = 0.05; size.max = 1; size.step = 0.01; size.value = o.w;
    size.oninput = () => edit(() => { o.w = Number(size.value); });
  }
  const start = field('Début (s)', el('input'), true);
  start.type = 'number'; start.min = 0; start.step = 0.1; start.value = o.start.toFixed(1);
  const dur = field('Durée (s)', el('input'), true);
  dur.type = 'number'; dur.min = 0.2; dur.step = 0.1; dur.value = (o.end - o.start).toFixed(1);
  const timing = () => edit(() => {
    const s = clamp(Number(start.value) || 0, 0, total());
    const d = Math.max(0.2, Number(dur.value) || 0.2);
    o.start = s;
    o.end = Math.min(total(), s + d);
  });
  start.onchange = timing;
  dur.onchange = timing;
  const del = el('button', 'btn danger', 'Supprimer le calque');
  del.onclick = doDelete;
  body.append(del);
}

// ── Navigation dans la timeline
$('#timeline').onpointerdown = e => {
  if (!ed) return;
  const lane = $('#tl-pieces');
  const at = ev => {
    const r = lane.getBoundingClientRect();
    return clamp((ev.clientX - r.left) / r.width, 0, 1) * total();
  };
  const t = at(e);
  const loc = locate(t);
  const overPieces = e.target.closest('#tl-pieces');
  select(overPieces ? { type: 'piece', id: loc.p.id } : null);
  seek(t);
  drag(e, ev => seek(at(ev)));
};
$('#overlays').onpointerdown = () => ed && select(null);

$('#play').onclick = () => (ed.playing ? pause() : play());
$('#t-split').onclick = doSplit;
$('#t-delete').onclick = doDelete;
$('#t-undo').onclick = undo;
$('#t-text').onclick = addText;
$('#t-image').onclick = addImage;
$('#t-video').onclick = addVideo;
$('#ed-back').onclick = closeEditor;
$('#ed-export').onclick = doExport;

window.addEventListener('keydown', e => {
  if (!ed || !$('#settings').hidden || !$('#exporting').hidden) return;
  if (e.target.matches('input, textarea, select')) return;
  if (e.code === 'Space') { e.preventDefault(); ed.playing ? pause() : play(); }
  else if (e.code === 'KeyS' && !e.ctrlKey) doSplit();
  else if (e.key === 'Delete' || e.key === 'Backspace') doDelete();
  else if (e.code === 'KeyZ' && e.ctrlKey) undo();
  else if (e.key === 'ArrowLeft') seek(ed.T - (e.shiftKey ? 1 / 30 : 1));
  else if (e.key === 'ArrowRight') seek(ed.T + (e.shiftKey ? 1 / 30 : 1));
  else if (e.key === 'Escape') select(null);
});

async function openEditor(clip) {
  let saved = null;
  try { saved = JSON.parse(await invoke('load_project', { clipPath: clip.path })); } catch (e) { /* pas de projet */ }
  ed = { clip, project: null, sel: null, T: 0, cur: 0, playing: false, undo: [], audios: [], ext: new Map(), images: new Map() };
  $('#library').hidden = true;
  $('#editor').hidden = false;
  $('#ed-title').textContent = clip.name;
  video.src = fileSrc(clip.path);
  await new Promise(res => {
    video.onloadedmetadata = res;
    video.onerror = res;
  });
  if (!ed) return;
  if (!video.videoWidth) {
    toast('Impossible de lire ce clip.', { error: true });
    closeEditor();
    return;
  }
  const duration = isFinite(video.duration) ? video.duration : clip.duration;
  const stage = $('#stage');
  stage.style.aspectRatio = `${video.videoWidth} / ${video.videoHeight}`;
  stage.style.setProperty('--ar', video.videoWidth / video.videoHeight);
  ed.audios = clip.tracks.map(t => {
    const a = new Audio(fileSrc(t.file));
    a.preload = 'auto';
    return a;
  });
  const n = Math.max(1, clip.tracks.length);
  const fresh = { volumes: Array(n).fill(1), muted: Array(n).fill(false), pieces: [{ id: uid(), src: null, start: 0, end: duration, volume: 1 }], overlays: [] };
  const valid = saved && Array.isArray(saved.pieces) && saved.pieces.length &&
    saved.pieces.every(p => p.src || p.end <= duration + 0.05);
  ed.project = valid ? { ...fresh, ...saved } : fresh;
  for (const p of ed.project.pieces) if (!p.src) p.end = Math.min(p.end, duration);
  for (const o of ed.project.overlays) if (o.type === 'image') loadImage(o.src).catch(() => {});
  renderTimeline();
  renderOverlays();
  renderInspector();
  applyVolumes();
  seek(0);
  tick();
}

function closeEditor() {
  if (!ed) return;
  pause();
  cancelAnimationFrame(ed.raf);
  clearTimeout(ed.saveTimer);
  invoke('save_project', { clipPath: ed.clip.path, json: JSON.stringify(ed.project) }).catch(() => {});
  for (const v of ed.ext.values()) { v.removeAttribute('src'); v.load(); v.remove(); }
  for (const a of ed.audios) { a.removeAttribute('src'); a.load(); }
  video.removeAttribute('src');
  video.load();
  ed = null;
  $('#editor').hidden = true;
  $('#library').hidden = false;
  refreshLibrary();
}

// ── Export
function renderOverlayPng(o, W, H) {
  const c = document.createElement('canvas');
  c.width = W;
  c.height = H;
  const g = c.getContext('2d');
  if (o.type === 'text') {
    const px = o.size * H;
    g.font = `700 ${px}px "Segoe UI", Arial, sans-serif`;
    g.textAlign = 'center';
    g.textBaseline = 'middle';
    g.lineJoin = 'round';
    g.lineWidth = px * 0.12;
    g.strokeStyle = '#000';
    g.fillStyle = o.color;
    const lines = o.text.split('\n');
    const lh = px * 1.15;
    lines.forEach((line, i) => {
      const y = o.y * H + (i - (lines.length - 1) / 2) * lh;
      g.strokeText(line, o.x * W, y);
      g.fillText(line, o.x * W, y);
    });
  } else {
    const img = ed.images.get(o.src);
    if (!img) return null;
    const w = o.w * W, h = w * (img.naturalHeight / img.naturalWidth);
    g.drawImage(img, o.x * W - w / 2, o.y * H - h / 2, w, h);
  }
  return c.toDataURL('image/png');
}

async function doExport() {
  pause();
  const W = video.videoWidth & ~1, H = video.videoHeight & ~1;
  const tot = total();
  const pr = ed.project;
  $('#export-bar').style.width = '0%';
  $('#exporting').hidden = false;
  try {
    const overlays = [];
    for (const o of pr.overlays) {
      const start = clamp(o.start, 0, tot), end = clamp(o.end, 0, tot);
      if (end - start < 0.05) continue;
      if (o.type === 'image') await loadImage(o.src);
      const dataUrl = renderOverlayPng(o, W, H);
      if (!dataUrl) continue;
      overlays.push({ png: await invoke('write_overlay', { dataUrl }), start, end });
    }
    const path = ed.clip.path;
    const sep = path.includes('\\') ? '\\' : '/';
    const dir = path.replace(/[\\/][^\\/]*$/, '');
    const d = new Date();
    const stamp = [d.getHours(), d.getMinutes(), d.getSeconds()].map(x => String(x).padStart(2, '0')).join('');
    const project = {
      main: path,
      track_count: ed.clip.tracks.length,
      mix_first: ed.clip.mix_first,
      volumes: pr.volumes.map((v, i) => (pr.muted[i] ? 0 : v)),
      pieces: pr.pieces.map(p => ({ src: p.src || null, start: p.start, end: p.end, volume: p.volume ?? 1 })),
      overlays,
      output: `${dir}${sep}${ed.clip.name}_montage_${stamp}.mp4`,
    };
    const out = await invoke('export_project', { project });
    toast('Montage exporté.', { action: { label: 'Afficher', run: () => invoke('reveal', { path: out }) } });
  } catch (e) {
    fail(e);
  }
  $('#exporting').hidden = true;
}

// ───────────────────────────── Réglages ─────────────────────────────
function fillSettings() {
  $('#s-buffer').value = String(cfg.buffer_seconds);
  $('#s-hotkey').textContent = prettyKey(cfg.hotkey);
  $('#s-dir').textContent = cfg.clips_dir;
  $('#s-dir').title = cfg.clips_dir;
  $('#s-game').checked = cfg.record_game;
  $('#s-voice').checked = cfg.record_voice;
  $('#s-voiceapp').value = cfg.voice_app;
  $('#s-mic-on').checked = cfg.record_mic;
  $('#s-height').value = String(cfg.height);
  $('#s-fps').value = String(cfg.fps);
  $('#s-bitrate').value = cfg.bitrate_mbps;
  $('#s-bitrate-val').textContent = `${cfg.bitrate_mbps} Mbit/s`;
  $('#s-monitor').value = String(cfg.monitor);
  $('#s-cursor').checked = cfg.capture_cursor;
  $('#s-autostart').checked = cfg.autostart;
  $('#s-offset').value = cfg.audio_offset_ms;
  $('#s-offset-val').textContent = `${cfg.audio_offset_ms} ms`;
}

async function openSettings() {
  if (!cfg) return;
  $('#settings').hidden = false;
  fillSettings();
  refreshStatus();
  const mic = $('#s-mic');
  mic.textContent = '';
  mic.append(new Option('Micro par défaut', 'default'));
  try {
    for (const name of await invoke('list_mics')) mic.append(new Option(name, name));
  } catch (e) { /* liste indisponible */ }
  if (cfg.mic !== 'default' && ![...mic.options].some(o => o.value === cfg.mic)) mic.append(new Option(cfg.mic, cfg.mic));
  mic.value = cfg.mic;
}

function bindSettings() {
  $('#btn-settings').onclick = openSettings;
  $('#settings').onclick = e => { if (e.target.id === 'settings' || e.target.closest('[data-close]')) $('#settings').hidden = true; };
  const on = (id, key, read) => { $(id).onchange = () => setConfig({ [key]: read($(id)) }); };
  on('#s-buffer', 'buffer_seconds', e => Number(e.value));
  on('#s-game', 'record_game', e => e.checked);
  on('#s-voice', 'record_voice', e => e.checked);
  on('#s-voiceapp', 'voice_app', e => e.value.trim());
  on('#s-mic-on', 'record_mic', e => e.checked);
  on('#s-mic', 'mic', e => e.value);
  on('#s-height', 'height', e => Number(e.value));
  on('#s-fps', 'fps', e => Number(e.value));
  on('#s-bitrate', 'bitrate_mbps', e => Number(e.value));
  on('#s-monitor', 'monitor', e => Number(e.value));
  on('#s-cursor', 'capture_cursor', e => e.checked);
  on('#s-autostart', 'autostart', e => e.checked);
  on('#s-offset', 'audio_offset_ms', e => Number(e.value));
  $('#s-bitrate').oninput = () => { $('#s-bitrate-val').textContent = `${$('#s-bitrate').value} Mbit/s`; };
  $('#s-offset').oninput = () => { $('#s-offset-val').textContent = `${$('#s-offset').value} ms`; };
  $('#s-dir').onclick = async e => {
    e.preventDefault();
    const dir = await dialog.open({ directory: true, defaultPath: cfg.clips_dir });
    if (dir) { await setConfig({ clips_dir: dir }); fillSettings(); refreshLibrary(); }
  };
  // Saisie du raccourci : on attend une combinaison se terminant par une vraie touche.
  const hk = $('#s-hotkey');
  hk.onclick = e => {
    e.preventDefault();
    hk.classList.add('rec');
    hk.textContent = 'Appuyez…';
    const stop = () => { hk.classList.remove('rec'); window.removeEventListener('keydown', onKey, true); fillSettings(); };
    const onKey = async ev => {
      ev.preventDefault();
      ev.stopPropagation();
      if (ev.key === 'Escape') { stop(); return; }
      if (['Control', 'Shift', 'Alt', 'Meta'].includes(ev.key)) return;
      const ok = /^(Key[A-Z]|Digit\d|F\d{1,2}|Numpad\d|Home|End|Insert|Delete|PageUp|PageDown|Pause|ScrollLock|PrintScreen)$/.test(ev.code);
      if (!ok) return;
      const parts = [];
      if (ev.ctrlKey) parts.push('Ctrl');
      if (ev.altKey) parts.push('Alt');
      if (ev.shiftKey) parts.push('Shift');
      if (!parts.length && !/^F\d/.test(ev.code)) { toast('Ajoutez Ctrl, Alt ou Maj à la touche.'); return; }
      parts.push(ev.code);
      window.removeEventListener('keydown', onKey, true);
      hk.classList.remove('rec');
      await setConfig({ hotkey: parts.join('+') });
      fillSettings();
    };
    window.addEventListener('keydown', onKey, true);
  };
}

// ───────────────────────────── Démarrage ─────────────────────────────
async function init() {
  bindSettings();
  await refreshStatus();
  await refreshLibrary();
  setInterval(refreshStatus, 3000);
  listen('clip-saved', e => {
    toast('Clip enregistré.');
    if (!ed) refreshLibrary(e.payload.path);
  });
  listen('clip-error', e => fail(e.payload));
  listen('status-changed', refreshStatus);
  listen('export-progress', e => { $('#export-bar').style.width = `${Math.round(e.payload * 100)}%`; });
}
init();
