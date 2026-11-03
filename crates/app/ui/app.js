// AuraPlay 前端逻辑(原生 JS,无构建链)
// 事件:engine(status/track_ended)、library_changed、peaks_ready
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const dialog = window.__TAURI__.dialog;

const S = {
  data: null,            // Persisted
  pinyin: {},            // id -> pinyin key
  peaks: {},             // id -> PeakPair[]
  sessions: [],          // {engine_idx, lib_id}
  engine: null,          // 最新 EngineStatus
  tab: 'library',
  group: '全部',
  search: '',
  mini: false,
};

const $ = (id) => document.getElementById(id);
const fmt = (s) => {
  if (s == null || !isFinite(s)) return '--:--';
  const m = Math.floor(s / 60), r = Math.floor(s % 60);
  return `${m}:${String(r).padStart(2, '0')}`;
};

// ---------- 初始化 ----------
async function boot() {
  const all = await invoke('get_all');
  S.data = all.data; S.pinyin = all.pinyin || {};
  renderGroups(); renderTab(); renderSettings();
  $('master').value = S.data.settings.master_gain;
  $('metro-on').checked = S.data.settings.metro_on;
  $('metro-bpm').value = S.data.settings.bpm;
  $('rundown-enabled').checked = S.data.settings.rundown_enabled;
  invoke('devices_list').then(list => {
    window.__devices = list;
    if (S.tab === 'settings') renderSettings(); // 设备列表晚到时重绘
  });
}

listen('engine', (e) => {
  const { evt, sessions } = e.payload;
  if (!evt) return;
  S.sessions = sessions || S.sessions;
  if (evt.type === 'status') {
    S.engine = evt;
    updatePlayingUI();
    renderFooter();
  } else if (evt.type === 'track_ended') {
    toast(`「${evt.name}」播完`);
  }
});
listen('library_changed', async () => { await reload(); });
listen('peaks_ready', async (e) => {
  const id = e.payload;
  S.peaks[id] = await invoke('get_peaks', { id });
  drawAllWaves();
});

async function reload() {
  const all = await invoke('get_all');
  S.data = all.data; S.pinyin = all.pinyin || {};
  renderGroups();   // 分组 chips 随库刷新(增/删分组后立即可见)
  renderTab();
}

// ---------- 页签 ----------
$('tabs').addEventListener('click', (e) => {
  const btn = e.target.closest('.tab'); if (!btn) return;
  S.tab = btn.dataset.tab;
  document.querySelectorAll('.tab').forEach(t => t.classList.toggle('active', t === btn));
  document.querySelectorAll('.page').forEach(p => p.classList.toggle('active', p.id === `page-${S.tab}`));
  renderTab();
});

function renderTab() {
  switch (S.tab) {
    case 'library': renderLibrary(); break;
    case 'rundown': renderRundown(); break;
    case 'schedule': renderTasks(); break;
    case 'cards': renderCards(); break;
    case 'settings': renderSettings(); break;
  }
}

// ---------- 素材库 ----------
$('btn-open').onclick = async () => {
  const res = await dialog.open({
    multiple: true,
    filters: [{ name: '音频', extensions: ['mp3','wav','flac','ogg','oga','m4a','aac','aiff','opus','caf'] }],
  });
  if (!res) return;
  const paths = Array.isArray(res) ? res : [res];
  invoke('add_files', { paths });
  toast(`已加入 ${paths.length} 个文件(解码中…)`);
};

let recording = false;
$('btn-record').onclick = async () => {
  if (!recording) {
    try { await invoke('record_start'); recording = true; $('btn-record').textContent = '⏹ 停止录音'; $('btn-record').classList.add('danger'); }
    catch (e) { toast(String(e)); }
  } else {
    try { const r = await invoke('record_stop'); toast(`录音完成:${r.name} ${r.duration.toFixed(1)}s`); }
    catch (e) { toast(String(e)); }
    recording = false; $('btn-record').textContent = '🎙 开始录音'; $('btn-record').classList.remove('danger');
  }
};

$('search').oninput = (e) => { S.search = e.target.value.trim().toLowerCase(); renderLibrary(); };
$('btn-add-group').onclick = async () => {
  const raw = await uiPrompt('新分组名称:');
  const name = raw?.trim();
  if (!name) return;
  const ok = await invoke('add_group', { name });
  if (!ok) { toast(`分组「${name}」已存在`); return; }
  S.group = name;
  await reload();
  toast(`已创建分组「${name}」`);
};

function renderGroups() {
  const el = $('groups'); el.innerHTML = '';
  const groups = ['全部', ...(S.data.groups || [])];
  for (const g of groups) {
    const chip = document.createElement('span');
    chip.className = 'chip' + (S.group === g ? ' active' : '');
    chip.textContent = g;
    if (g !== '全部' && g !== '默认分组') {
      const x = document.createElement('span'); x.className = 'x'; x.textContent = '×';
      x.onclick = async (ev) => {
        ev.stopPropagation();
        if (!(await uiConfirm(`删除分组「${g}」?(组内素材移回默认分组)`))) return;
        await invoke('remove_group', { name: g });
        if (S.group === g) S.group = '全部';
        await reload();
      };
      chip.appendChild(x);
    }
    chip.onclick = () => { S.group = g; renderGroups(); renderLibrary(); };
    if (g !== '全部') {
      const play = document.createElement('span');
      play.className = 'x'; play.textContent = '▶'; play.title = '整组齐放';
      play.onclick = async (ev) => { ev.stopPropagation(); const n = await invoke('play_group', { group: g }); if (!n) toast('该组没有文件素材'); };
      chip.appendChild(play);
    }
    el.appendChild(chip);
  }
}

function visibleLibrary() {
  let list = S.data.library.filter(t => t.kind === 'file');
  if (S.group !== '全部') list = list.filter(t => t.group === S.group);
  if (S.search) {
    list = list.filter(t =>
      t.name.toLowerCase().includes(S.search) ||
      (S.pinyin[t.id] || '').includes(S.search));
  }
  return list;
}

function sessionFor(libId) {
  return S.sessions.find(s => s.lib_id === libId);
}

function renderLibrary() {
  const wrap = $('cards'); wrap.innerHTML = '';
  wrap.className = 'cards ' + (S.data.settings.card_size || 'big');
  const list = visibleLibrary();
  $('lib-empty').hidden = list.length > 0;

  for (const t of list) {
    const card = document.createElement('div');
    card.className = 'card'; card.id = `lib-${t.id}`;
    card.innerHTML = `
      <div class="row1">
        <span class="name">${esc(t.name)}</span>
        <span class="badge grp">${esc(t.group)}</span>
        <span class="badge st">未加载</span>
        <span class="badge loopb" hidden>循环</span>
        <span class="badge revb" hidden>倒放</span>
      </div>
      <div class="time" title="点击输入入/出点(秒)"></div>
      <div class="wave"><canvas></canvas><div class="handle in"></div><div class="handle out"></div></div>
      <div class="ctrls">
        <button class="mini primary play">▶</button>
        <button class="mini pauseb">⏸</button>
        <button class="mini fo" title="2秒淡出">淡出</button>
        <button class="mini fi" title="淡入并播">淡入</button>
        <label class="inline">音量<input class="gain" type="range" min="0" max="2" step="0.05" value="${t.gain}"></label>
        <select class="speed">
          ${[0.5,0.75,1,1.25,1.5,2].map(v=>`<option value="${v}" ${v===t.speed?'selected':''}>${v}×</option>`).join('')}
        </select>
        <label class="inline"><input class="loopchk" type="checkbox" ${t.loop_on?'checked':''}>循环</label>
        <label class="inline"><input class="revchk" type="checkbox">倒放</label>
        <span class="spacer"></span>
      </div>
      <div class="ctrls">
        <select class="endact">
          <option value="stop">播完停止</option>
          <option value="loop">播完循环</option>
          <option value="next">播完接下一条</option>
        </select>
        <input class="hotkey" placeholder="全局热键" value="${t.hotkey||''}" title="如 CmdOrCtrl+F1">
        <button class="mini addrd">＋节目单</button>
        <button class="mini exp">导出</button>
        <button class="mini danger del">删除</button>
      </div>`;
    wireCard(card, t);
    wrap.appendChild(card);
    loadPeaks(t.id);
  }
  updatePlayingUI();
}

function wireCard(card, t) {
  const send = (op, a = 0, b = 0) => {
    const sess = sessionFor(t.id);
    if (sess) invoke('track_cmd', { index: sess.engine_idx, op, a: +a, b: +b });
  };
  card.querySelector('.play').onclick = () => invoke('play_id', { id: t.id });
  card.querySelector('.time').onclick = async () => {
    const cur = `${t.cue_in ?? 0} ${t.cue_out ?? t.duration}`;
    const v = await uiPrompt('入点 出点(秒,空格分隔;输入 0 0 清除):', cur);
    if (v == null) return;
    const [a, b] = v.trim().split(/[\s,]+/).map(Number);
    if (!isFinite(a) || !isFinite(b) || a < 0 || b <= a || b > t.duration) { toast('数值无效'); return; }
    const clear = a === 0 && b >= t.duration - 0.01;
    t.cue_in = clear ? null : a; t.cue_out = clear ? null : b;
    const sess = sessionFor(t.id);
    if (sess) invoke('track_cmd', { index: sess.engine_idx, op: clear ? 'cue_clear' : 'cue', a: t.cue_in ?? 0, b: t.cue_out ?? 0 });
    await invoke('update_meta', { meta: t });
    drawWave(t.id); toast(clear ? '已清除裁剪' : `入 ${fmt(a)} → 出 ${fmt(b)}`);
  };
  card.querySelector('.pauseb').onclick = () => invoke('stop_id', { id: t.id });
  card.querySelector('.fo').onclick = () => send('fade', 0, 2);
  card.querySelector('.fi').onclick = () => { invoke('play_id', { id: t.id }); send('fade', 1, 2); };

  card.querySelector('.gain').onchange = async (e) => {
    t.gain = +e.target.value; send('gain', t.gain); await invoke('update_meta', { meta: t });
  };
  card.querySelector('.speed').onchange = async (e) => {
    t.speed = +e.target.value; send('speed', t.speed); await invoke('update_meta', { meta: t });
  };
  card.querySelector('.loopchk').onchange = async (e) => {
    t.loop_on = e.target.checked; send('loop', t.loop_on ? 1 : 0);
    if (t.loop_on && t.end_action !== 'loop') { t.end_action = 'loop'; }
    await invoke('update_meta', { meta: t }); renderLibrary();
  };
  card.querySelector('.revchk').onchange = (e) => send('reverse', e.target.checked ? 1 : 0);
  const endSel = card.querySelector('.endact');
  endSel.value = t.end_action;
  endSel.onchange = async (e) => {
    t.end_action = e.target.value;
    if (t.end_action === 'loop') t.loop_on = true;
    if (t.end_action !== 'loop') t.loop_on = false;
    send('loop', t.loop_on ? 1 : 0);
    await invoke('update_meta', { meta: t }); renderLibrary();
  };
  card.querySelector('.hotkey').onchange = async (e) => {
    t.hotkey = e.target.value || null;
    await invoke('update_meta', { meta: t }); toast('热键已保存');
  };
  card.querySelector('.addrd').onclick = async () => {
    if (!S.data.rundown.includes(t.id)) {
      S.data.rundown.push(t.id);
      await invoke('rundown_set', { ids: S.data.rundown, enabled: S.data.rundown_enabled });
      toast(`已加入节目单(${S.data.rundown.length})`);
    } else toast('已在节目单中');
  };
  card.querySelector('.exp').onclick = async () => {
    const path = await dialog.save({
      defaultPath: `${t.name}(裁剪).wav`,
      filters: [{ name: 'WAV', extensions: ['wav'] }],
    });
    if (!path) return;
    try { const dur = await invoke('export_wav', { id: t.id, path }); toast(`已导出 ${dur.toFixed(1)}s`); }
    catch (e) { toast(String(e)); }
  };
  card.querySelector('.del').onclick = async () => {
    if (!(await uiConfirm(`删除「${t.name}」?(文件不会被删)`))) return;
    await invoke('remove_id', { id: t.id }); await reload();
  };

  wireWave(card.querySelector('.wave'), t, send);
  card.addEventListener('dblclick', () => {
    if (S.data.settings.double_click_play) invoke('play_id', { id: t.id });
  });
}

// ---------- 波形 ----------
async function loadPeaks(id) {
  if (S.peaks[id]) { drawWave(id); return; }
  const p = await invoke('get_peaks', { id });
  if (p) { S.peaks[id] = p; drawWave(id); }
  // null → 等 peaks_ready 事件
}

function waveMeta(t) {
  const card = document.getElementById(`lib-${t.id}`); if (!card) return null;
  const wave = card.querySelector('.wave'); const canvas = card.querySelector('canvas');
  return { card, wave, canvas, ctx: canvas.getContext('2d') };
}

function drawWave(id) {
  const t = S.data.library.find(x => x.id === id); if (!t) return;
  const m = waveMeta(t); if (!m) return;
  const peaks = S.peaks[id]; if (!peaks) return;
  const { canvas, ctx } = m;
  const w = canvas.clientWidth || 320, h = canvas.clientHeight || 56;
  canvas.width = w * 2; canvas.height = h * 2; ctx.scale(2, 2);
  ctx.clearRect(0, 0, w, h);

  const cs = getComputedStyle(document.body);
  const dur = t.duration || 1;
  const cin = t.cue_in || 0, cout = t.cue_out || dur;

  // 波形
  const style = cs.getPropertyValue('--accent2').trim() || '#7c5cff';
  ctx.fillStyle = style;
  const n = peaks.length;
  for (let x = 0; x < w; x++) {
    const i = Math.floor(x / w * n);
    const p = peaks[i]; if (!p) continue;
    const sec = x / w * dur;
    const inRange = sec >= cin && sec <= cout;
    ctx.globalAlpha = inRange ? 0.95 : 0.25;
    const top = (1 - Math.min(1, p.max)) * h / 2;
    const bot = (1 + Math.max(-1, p.min)) * h / 2;
    ctx.fillRect(x, top, 1, Math.max(1, bot - top));
  }
  ctx.globalAlpha = 1;
  drawWaveOverlay(id);
}

function drawWaveOverlay(id) {
  const t = S.data.library.find(x => x.id === id); if (!t) return;
  const m = waveMeta(t); if (!m) return;
  const { ctx, wave } = m;
  const w = wave.clientWidth, h = wave.clientHeight;
  const dur = t.duration || 1;
  const cin = t.cue_in || 0, cout = t.cue_out || dur;
  ctx.globalAlpha = 1;

  // 裁剪柄位置
  m.wave.querySelector('.handle.in').style.left = (cin / dur * w - 4) + 'px';
  m.wave.querySelector('.handle.out').style.left = (cout / dur * w - 4) + 'px';

  // 播放头
  const sess = sessionFor(t.id);
  if (sess && S.engine) {
    const et = S.engine.tracks[sess.engine_idx];
    if (et) {
      const x = et.position_secs / dur * w;
      ctx.fillStyle = '#22d3ee';
      ctx.fillRect(x - 1, 0, 2, h);
    }
  }
}

function drawAllWaves() { Object.keys(S.peaks).forEach(drawWave); }

function wireWave(wave, t, send) {
  let drag = null; // {kind:'in'|'out'|'seek'}
  const secOf = (e) => {
    const r = wave.getBoundingClientRect();
    return Math.max(0, Math.min(t.duration, (e.clientX - r.left) / r.width * t.duration));
  };
  wave.addEventListener('pointerdown', (e) => {
    const r = wave.getBoundingClientRect();
    const xin = (t.cue_in || 0) / t.duration * r.width;
    const xout = (t.cue_out || t.duration) / t.duration * r.width;
    if (Math.abs(e.clientX - r.left - xin) < 8) drag = { kind: 'in' };
    else if (Math.abs(e.clientX - r.left - xout) < 8) drag = { kind: 'out' };
    else drag = { kind: 'seek' };
    wave.setPointerCapture(e.pointerId);
  });
  wave.addEventListener('pointermove', (e) => {
    if (!drag || drag.kind === 'seek') return;
    const s = secOf(e);
    if (drag.kind === 'in') t.cue_in = Math.min(s, (t.cue_out || t.duration) - 0.05);
    else t.cue_out = Math.max(s, (t.cue_in || 0) + 0.05);
    drawWave(t.id);
  });
  wave.addEventListener('pointerup', async (e) => {
    if (!drag) return;
    if (drag.kind === 'seek') {
      const s = secOf(e);
      const sess = sessionFor(t.id);
      if (sess) invoke('track_cmd', { index: sess.engine_idx, op: 'seek', a: s, b: 0 });
    } else {
      send('cue', t.cue_in || 0, t.cue_out || 0);
      await invoke('update_meta', { meta: t });
      toast(`入 ${fmt(t.cue_in)} → 出 ${fmt(t.cue_out)}`);
    }
    drag = null;
  });
}

// ---------- 播放状态刷新 ----------
function updatePlayingUI() {
  if (!S.engine || !S.data) return;
  const byIdx = new Map(S.sessions.map(s => [s.engine_idx, s.lib_id]));
  for (const t of S.data.library) {
    const card = document.getElementById(`lib-${t.id}`); if (!card) continue;
    const sess = sessionFor(t.id);
    const et = sess ? S.engine.tracks[sess.engine_idx] : null;
    const playing = et?.playing;
    card.classList.toggle('playing', !!playing);
    const st = card.querySelector('.st');
    if (!et) { st.textContent = '未加载'; st.classList.remove('on'); }
    else { st.textContent = playing ? '播放中' : (et.ended ? '播完' : '暂停'); st.classList.toggle('on', !!playing); }
    card.querySelector('.loopb').hidden = !(t.loop_on || et?.looping);
    card.querySelector('.revb').hidden = !et?.reverse;
    const time = card.querySelector('.time');
    if (et) time.textContent = `${fmt(et.position_secs)} / ${fmt(et.duration_secs)}${t.cue_in!=null||t.cue_out!=null?` · 裁剪 ${fmt(et.trimmed_secs)}`:''}`;
    else time.textContent = fmt(t.duration);
    drawWaveOverlay(t.id);
  }
  // 节目单当前项高亮
  const cur = S.engine.tracks.findIndex(x => x.playing);
  document.querySelectorAll('#rundown li').forEach((li, i) => {
    const id = +li.dataset.id; const sess = sessionFor(id);
    li.classList.toggle('current', sess && sess.engine_idx === cur);
  });
}

function renderFooter() {
  if (!S.engine) return;
  const playing = S.engine.tracks.filter(t => t.playing);
  $('foot-playing').textContent = playing.length
    ? `播放中 ${playing.length} 轨:${playing.slice(0, 3).map(t => t.name).join('、')}${playing.length > 3 ? '…' : ''}`
    : '空闲';
  $('master').value = S.engine.master_gain;
}

// ---------- 节目单 ----------
$('rundown-enabled').onchange = async (e) => {
  await invoke('rundown_set', { ids: S.data.rundown, enabled: e.target.checked });
};
$('btn-rundown-play').onclick = () => invoke('rundown_play');
$('btn-rundown-stopall').onclick = () => invoke('stop_all');

function renderRundown() {
  const ol = $('rundown'); ol.innerHTML = '';
  const ids = S.data.rundown;
  $('rundown-empty').hidden = ids.length > 0;
  ids.forEach((id, i) => {
    const t = S.data.library.find(x => x.id === id); if (!t) return;
    const li = document.createElement('li');
    li.dataset.id = id;
    li.innerHTML = `<span class="idx">${String(i + 1).padStart(2, '0')}</span>
      <span class="name">${esc(t.name)}</span>
      <span class="dim">${fmt(t.duration)}</span>
      <button class="mini up">↑</button><button class="mini down">↓</button>
      <button class="mini danger rm">移除</button>`;
    const move = async (delta) => {
      const j = i + delta; if (j < 0 || j >= ids.length) return;
      [ids[i], ids[j]] = [ids[j], ids[i]];
      await invoke('rundown_set', { ids, enabled: S.data.rundown_enabled });
      renderRundown();
    };
    li.querySelector('.up').onclick = () => move(-1);
    li.querySelector('.down').onclick = () => move(1);
    li.querySelector('.rm').onclick = async () => {
      ids.splice(i, 1);
      await invoke('rundown_set', { ids, enabled: S.data.rundown_enabled });
      renderRundown();
    };
    ol.appendChild(li);
  });
  updatePlayingUI();
}

// ---------- 定时 ----------
$('btn-task-add').onclick = async () => {
  S.data.tasks.push({ id: Date.now(), time: '20:00', days: [], action: 'play_rundown', enabled: true });
  await invoke('tasks_save', { tasks: S.data.tasks });
  renderTasks();
};

const DAY_NAMES = { 1: '一', 2: '二', 3: '三', 4: '四', 5: '五', 6: '六', 7: '日' };

function renderTasks() {
  const wrap = $('tasks'); wrap.innerHTML = '';
  for (const task of S.data.tasks) {
    const div = document.createElement('div');
    div.className = 'task';
    const isRundown = task.action === 'play_rundown';
    div.innerHTML = `
      <input type="time" value="${task.time}" />
      ${[1,2,3,4,5,6,7].map(d => `<label class="inline"><input type="checkbox" data-day="${d}" ${task.days.includes(d)?'checked':''}>${DAY_NAMES[d]}</label>`).join('')}
      <select class="act">
        <option value="rundown" ${isRundown?'selected':''}>播放节目单</option>
        <option value="track" ${!isRundown?'selected':''}>播放素材</option>
      </select>
      <select class="track" ${isRundown?'hidden':''}>
        ${S.data.library.filter(t=>t.kind==='file').map(t=>`<option value="${t.id}" ${task.action?.play_track===t.id?'selected':''}>${esc(t.name)}</option>`).join('')}
      </select>
      <label class="inline"><input type="checkbox" class="en" ${task.enabled?'checked':''}>启用</label>
      <span class="spacer" style="flex:1"></span>
      <button class="mini danger rm">删除</button>`;
    const time = div.querySelector('input[type=time]');
    const save = async () => {
      task.time = time.value;
      task.days = [...div.querySelectorAll('[data-day]:checked')].map(c => +c.dataset.day);
      task.enabled = div.querySelector('.en').checked;
      const act = div.querySelector('.act').value;
      task.action = act === 'rundown' ? 'play_rundown' : { play_track: +div.querySelector('.track').value };
      await invoke('tasks_save', { tasks: S.data.tasks });
      toast('已保存');
    };
    time.onchange = save;
    div.querySelectorAll('input,select').forEach(el => { if (el !== time) el.onchange = save; });
    div.querySelector('.rm').onclick = async () => {
      S.data.tasks = S.data.tasks.filter(t => t !== task);
      await invoke('tasks_save', { tasks: S.data.tasks });
      renderTasks();
    };
    wrap.appendChild(div);
  }
}

// ---------- 文字卡 ----------
const CARD_TEMPLATES = {
  '空白': '',
  '婚礼开场': `【开场】\n尊敬的各位来宾,大家好!\n欢迎来到 XX 先生与 XX 女士的婚礼现场……\n\n【要点】新人入场 / 主持人站位 / 音乐 cue`,
  '中场串词': `【串词】\n刚才的一曲《 》,让我们……\n接下来有请……\n\n【备用】如果冷场:……`,
  '答谢退场': `【答谢】\n感谢各位今天的到来\n新人敬酒安排:先主桌,后亲友\n\n【结束】礼成,恭送宾客`,
};
$('btn-card-add').onclick = async () => {
  const keys = Object.keys(CARD_TEMPLATES);
  const tpl = await uiSelect('选择模板', keys.map(k => ({ value: k, label: k })));
  if (!tpl) return;
  const title = $('card-title').value.trim() || `${tpl} ${S.data.library.filter(t => t.kind === 'text').length + 1}`;
  const id = await invoke('add_text_card', { title, text: CARD_TEMPLATES[tpl] });
  $('card-title').value = '';
  await reload(); switchTab('cards');
  toast('已创建,点击卡片编辑内容');
};

function renderCards() {
  const wrap = $('textcards'); wrap.innerHTML = '';
  const cards = S.data.library.filter(t => t.kind === 'text');
  if (!cards.length) { wrap.innerHTML = '<div class="empty"><div class="big">还没有文字卡</div></div>'; return; }
  for (const t of cards) {
    const div = document.createElement('div');
    div.className = 'card';
    const active = S.data.active_script === t.id;
    div.innerHTML = `
      <div class="row1"><span class="name">${esc(t.name)}</span>
        ${active ? '<span class="badge on">司仪宝使用中</span>' : ''}</div>
      <textarea class="txt">${esc(t.text)}</textarea>
      <div class="ctrls">
        <input class="tname" value="${esc(t.name)}" style="flex:1" placeholder="标题">
        <button class="mini save">保存</button>
        <button class="mini primary use">${active ? '刷新到手机' : '设为司仪宝'}</button>
        <button class="mini danger del">删除</button>
      </div>`;
    div.querySelector('.save').onclick = async () => {
      t.text = div.querySelector('.txt').value;
      t.name = div.querySelector('.tname').value || t.name;
      await invoke('update_meta', { meta: t }); renderCards(); toast('已保存');
    };
    div.querySelector('.use').onclick = async () => {
      await invoke('script_set', { id: t.id });
      S.data.active_script = t.id; renderCards();
      toast('手机端 /script 已切换(15 秒内自动刷新)');
    };
    div.querySelector('.del').onclick = async () => {
      await invoke('remove_id', { id: t.id }); await reload();
    };
    wrap.appendChild(div);
  }
}

// ---------- 设置 ----------
function renderSettings() {
  const s = S.data.settings;
  const el = $('settings');
  el.innerHTML = `
    <h3>播放</h3>
    <div class="setrow"><label>主题</label>
      <select id="s-theme"><option value="dark" ${s.theme==='dark'?'selected':''}>深色</option><option value="light" ${s.theme==='light'?'selected':''}>浅色</option></select></div>
    <div class="setrow"><label>默认淡入淡出</label><input id="s-fade" type="number" step="0.5" min="0" value="${s.default_fade}" style="width:80px"> 秒</div>
    <div class="setrow"><label>双击卡片即播放</label><input id="s-dbl" type="checkbox" ${s.double_click_play?'checked':''}></div>
    <div class="setrow"><label>卡片视图</label>
      <select id="s-cardsize"><option value="big" ${s.card_size==='big'?'selected':''}>大卡片</option><option value="small" ${s.card_size==='small'?'selected':''}>小卡片</option><option value="list" ${s.card_size==='list'?'selected':''}>列表</option></select></div>
    <div class="setrow"><label>主音量</label><input id="s-master" type="range" min="0" max="2" step="0.05" value="${s.master_gain}"></div>
    <h3>控台 / MTC</h3>
    <div class="setrow"><label>发送 MTC 时间码</label><input id="s-mtc" type="checkbox" ${s.mtc_enabled?'checked':''}>
      <select id="s-fps"><option value="24" ${s.mtc_fps===24?'selected':''}>24 fps</option><option value="25" ${s.mtc_fps===25?'selected':''}>25 fps</option><option value="30" ${s.mtc_fps===30?'selected':''}>30 fps</option></select>
      <span class="dim">MIDI 端口「AuraPlay MTC」</span></div>
    <div class="setrow"><label>输出设备</label>
      <select id="s-device"><option value="">系统默认</option>
        ${(window.__devices||[]).map(d=>`<option value="${esc(d)}" ${s.device===d?'selected':''}>${esc(d)}</option>`).join('')}</select>
      <span class="dim">切换会清空当前播放</span></div>
    <h3>全局快捷键</h3>
    ${['play_all:全部播放','pause_all:全部暂停','fadeout_all:全部淡出','metronome:节拍器'].map(([k,label])=>`
      <div class="setrow"><label>${label}</label><input class="hk" data-k="${k}" value="${s.hotkeys[k]||''}" placeholder="如 CmdOrCtrl+Alt+P"></div>`).join('')}
    <h3>手机端</h3>
    <div class="setrow"><label>端口</label><input id="s-port" type="number" value="${s.remote_port}" style="width:90px">
      <span class="dim">改端口后重启生效;手机与电脑同一 Wi-Fi</span></div>
    <div style="margin-top:16px"><button class="primary" id="s-save">保存设置</button></div>`;

  $('s-save').onclick = async () => {
    const settings = {
      theme: $('s-theme').value,
      card_size: $('s-cardsize').value,
      default_fade: +$('s-fade').value,
      double_click_play: $('s-dbl').checked,
      master_gain: +$('s-master').value,
      mtc_enabled: $('s-mtc').checked,
      mtc_fps: +$('s-fps').value,
      metro_on: $('metro-on').checked,
      bpm: +$('metro-bpm').value,
      metro_gain: s.metro_gain,
      device: $('s-device').value || null,
      remote_port: +$('s-port').value,
      hotkeys: Object.fromEntries([...document.querySelectorAll('.hk')].filter(h=>h.value).map(h=>[h.dataset.k,h.value])),
    };
    S.data.settings = settings;
    document.body.dataset.theme = settings.theme;
    await invoke('settings_save', { settings });
    if (settings.device !== s.device) await invoke('switch_device', { name: settings.device });
    toast('设置已保存');
  };
}

// ---------- 底部 ----------
$('master').oninput = (e) => invoke('master_cmd', { gain: +e.target.value });
$('metro-on').onchange = (e) => invoke('metronome_cmd', { on: e.target.checked, bpm: +$('metro-bpm').value, gain: 0.6 });
$('metro-bpm').onchange = () => invoke('metronome_cmd', { on: $('metro-on').checked, bpm: +$('metro-bpm').value, gain: 0.6 });
$('btn-fadeout').onclick = () => invoke('all_cmd', { op: 'fade', a: 0, b: 2 });
$('btn-stopall').onclick = () => invoke('stop_all');
$('btn-top').onclick = async (e) => {
  window.__top = !window.__top;
  await invoke('window_top', { on: window.__top });
  e.target.style.opacity = window.__top ? 1 : 0.5;
};
$('btn-mini').onclick = async () => {
  S.mini = !S.mini;
  await invoke('window_mini', { mini: S.mini });
};
$('btn-remote').onclick = async () => {
  const info = await invoke('remote_info');
  $('remote-qr').innerHTML = info.svg;
  $('remote-url').textContent = info.url;
  $('remote-modal').hidden = false;
};
// 点背景或 Esc 关闭弹窗
$('remote-modal').addEventListener('click', (e) => {
  if (e.target.id === 'remote-modal') e.target.hidden = true;
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape') $('remote-modal').hidden = true;
});

// ---------- 自绘对话框(WKWebView 无原生 prompt/confirm) ----------
function uiDialog({ title, value = '', options = null }) {
  return new Promise((resolve) => {
    const m = $('prompt-modal');
    const input = $('prompt-input');
    const select = $('prompt-select');
    $('prompt-title').textContent = title;
    if (options) {
      input.hidden = true; select.hidden = false; select.innerHTML = '';
      for (const o of options) {
        const opt = document.createElement('option');
        opt.value = o.value; opt.textContent = o.label;
        select.appendChild(opt);
      }
    } else {
      input.hidden = false; select.hidden = true; input.value = value;
    }
    m.hidden = false;
    const target = options ? select : input;
    setTimeout(() => target.focus(), 30);
    const done = (val) => {
      m.hidden = true;
      $('prompt-ok').onclick = $('prompt-cancel').onclick = null;
      document.removeEventListener('keydown', esc);
      resolve(val);
    };
    const esc = (e) => { if (e.key === 'Escape') done(null); };
    document.addEventListener('keydown', esc);
    $('prompt-ok').onclick = () => done(options ? select.value : input.value);
    $('prompt-cancel').onclick = () => done(null);
    input.onkeydown = (e) => { if (e.key === 'Enter') done(input.value); };
  });
}
const uiPrompt = (title, value = '') => uiDialog({ title, value });
const uiConfirm = (title) => uiDialog({ title }).then((v) => v !== null);
const uiSelect = (title, options) => uiDialog({ title, options });

// ---------- 工具 ----------
function esc(s) { return String(s ?? '').replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;'); }
let toastTimer;
function toast(msg) {
  let el = document.getElementById('toast');
  if (!el) { el = document.createElement('div'); el.id = 'toast'; el.className = 'toast'; document.body.appendChild(el); }
  el.textContent = msg; el.classList.add('show');
  clearTimeout(toastTimer); toastTimer = setTimeout(() => el.classList.remove('show'), 2000);
}
function switchTab(name) { document.querySelector(`.tab[data-tab="${name}"]`)?.click(); }

// 全局热键挂起:输入框聚焦时不抢键
let hkSuspended = false;
document.addEventListener('focusin', (e) => {
  const tag = e.target.tagName;
  if (!hkSuspended && (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT')) {
    hkSuspended = true; invoke('hotkeys_suspend', { on: true });
  }
});
document.addEventListener('focusout', () => {
  if (hkSuspended) {
    setTimeout(() => {
      const tag = document.activeElement?.tagName;
      if (tag !== 'INPUT' && tag !== 'TEXTAREA' && tag !== 'SELECT') {
        hkSuspended = false; invoke('hotkeys_suspend', { on: false });
      }
    }, 100);
  }
});

// toast 样式(动态注入,style.css 之外兜底)
const style = document.createElement('style');
style.textContent = `.toast{position:fixed;left:50%;bottom:64px;transform:translateX(-50%);background:var(--panel2);border:1px solid var(--line);border-radius:99px;padding:6px 16px;font-size:12px;opacity:0;transition:opacity .3s;pointer-events:none;z-index:99}
.toast.show{opacity:1}`;
document.head.appendChild(style);

boot();
