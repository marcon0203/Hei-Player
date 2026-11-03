//! Hei Player 桌面客户端:Tauri 2 壳 + playback 引擎。
//!
//! 布局:
//! - owner.rs    引擎属主线程(Stream 非 Send)+ 事件广播
//! - state.rs    共享状态(素材库/节目单/定时/设置 + 会话映射)
//! - midi.rs     MTC 时间码输出(虚拟 MIDI 端口)
//! - recorder.rs 麦克风录音
//! - remote.rs   手机端 HTTP(遥控/上传/司仪宝)
//! - schedule.rs 定时播放
//! - ui/         前端(原生 HTML/JS,无构建链)

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod midi;
mod owner;
mod recorder;
mod remote;
mod schedule;
mod state;
mod store;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use playback::{AudioData, Command, MasterCmd, TrackCmd};
use state::App;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut};

use store::{ScheduleTask, Settings, TrackKind, TrackMeta};

// ---------- 命令字符串解析(桌面/手机共用) ----------

/// 前端/手机端操作字符串 → TrackCmd。a/b 为通用数值参数。
pub fn parse_op(op: &str, a: f64, b: f64) -> Option<TrackCmd> {
    match op {
        "play" => Some(TrackCmd::Play),
        "pause" => Some(TrackCmd::Pause),
        "toggle" => Some(TrackCmd::Toggle),
        "gain" => Some(TrackCmd::SetGain(a as f32)),
        "fade" => Some(TrackCmd::FadeTo { target: a as f32, secs: b }),
        "speed" => Some(TrackCmd::SetSpeed(a as f32)),
        "reverse" => Some(TrackCmd::SetReverse(a > 0.5)),
        "loop" => Some(TrackCmd::LoopAll(a > 0.5)),
        "seek" => Some(TrackCmd::SeekSecs(a)),
        "cue" => Some(TrackCmd::SetCue { cue_in: Some(a), cue_out: (b > 0.0).then_some(b) }),
        "cue_clear" => Some(TrackCmd::SetCue { cue_in: None, cue_out: None }),
        "playcue" => Some(TrackCmd::PlayFromCue),
        _ => None,
    }
}

fn pinyin_key(s: &str) -> String {
    use pinyin::ToPinyin;
    let mut out = String::new();
    // to_pinyin() 与 chars 一一对应:汉字 → Some(拼音),其他 → None
    for (c, py) in s.chars().zip(s.to_pinyin()) {
        match py {
            Some(p) => {
                out.push_str(p.plain());
                out.push(' ');
            }
            None => out.push(c.to_ascii_lowercase()),
        }
    }
    out
}

fn main() {
    let home = std::env::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let data_dir = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.clone())
            .join("HeiPlayer")
    } else {
        home.join("Library/Application Support/HeiPlayer")
    };
    // 旧版 AuraPlay 数据迁移(一次性)
    let old_dir = home.join("Library/Application Support/AuraPlay");
    if !data_dir.exists() && old_dir.exists() {
        let _ = std::fs::create_dir_all(&data_dir);
        for entry in std::fs::read_dir(&old_dir).into_iter().flatten().flatten() {
            let _ = std::fs::copy(entry.path(), data_dir.join(entry.file_name()));
        }
        eprintln!("[migrate] 已迁移 AuraPlay → HeiPlayer 数据");
    }
    let _ = std::fs::create_dir_all(data_dir.join("uploads"));
    let persisted = store::Persisted::load(&data_dir);

    let hub = owner::EventHub::default();
    let (owner_tx, engines) = owner::spawn_owner(persisted.settings.device.clone(), hub.clone());

    let mtc_enabled = Arc::new(AtomicBool::new(persisted.settings.mtc_enabled));
    let remote_port = persisted.settings.remote_port;

    // 唯一 App 实例:桌面与手机端共享
    let app = App::new(
        persisted.clone(),
        hub.clone(),
        engines.clone(),
        data_dir.clone(),
        remote_port,
        mtc_enabled.clone(),
        owner_tx,
    );
    remote::spawn_server(Arc::clone(&app), remote_port);

    if let Err(e) = midi::spawn_mtc(mtc_enabled.clone(), persisted.settings.mtc_fps, hub.subscribe())
    {
        eprintln!("[mtc] 启动失败(无 MIDI 环境?忽略):{e:#}");
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, _event| handle_hotkey(app, shortcut))
                .build(),
        )
        .setup(move |tauri_app| {
            tauri_app.manage(Arc::clone(&app));

            // 事件泵:引擎事件 → 会话同步 + 连播推进 + 前端 emit
            let pump_app = Arc::clone(&app);
            let pump_handle = tauri_app.handle().clone();
            let evt_rx = hub.subscribe();
            std::thread::spawn(move || {
                for evt in evt_rx.iter() {
                    match &evt {
                        playback::Event::Status(st) => {
                            let bound = pump_app.sync_sessions(st.tracks.len());
                            for (idx, lib_id) in bound {
                                let meta =
                                    pump_app.core.lock().data.track(lib_id).cloned();
                                if let Some(m) = meta {
                                    pump_app.apply_meta_to_track(idx, &m);
                                    owner::send(
                                        &pump_app.engines,
                                        Command::Track {
                                            index: idx,
                                            cmd: TrackCmd::PlayFromCue,
                                        },
                                    );
                                }
                            }
                        }
                        playback::Event::TrackEnded { index, .. } => {
                            pump_app.on_track_ended(*index);
                        }
                    }
                    let sessions = pump_app.core.lock().sessions.clone();
                    let _ = pump_handle.emit(
                        "engine",
                        serde_json::json!({ "evt": &evt, "sessions": sessions }),
                    );
                }
            });

            // 300ms 状态轮询
            let tick_slot = engines.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(300));
                owner::send(&tick_slot, Command::Status);
            });

            // 定时播放
            schedule::spawn(Arc::clone(&app));

            // 全局快捷键
            register_hotkeys(tauri_app.handle(), &app);

            // 启动时套用持久化的引擎级设置
            let s = &persisted.settings;
            owner::send(&engines, Command::Master(MasterCmd::SetMasterGain(s.master_gain)));
            owner::send(
                &engines,
                Command::Master(MasterCmd::Metronome {
                    on: s.metro_on,
                    bpm: s.bpm,
                    gain: s.metro_gain,
                }),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_all,
            add_files,
            add_text_card,
            update_meta,
            remove_id,
            add_group,
            remove_group,
            play_id,
            play_group,
            hotkeys_suspend,
            playcue_id,
            stop_id,
            stop_all,
            all_cmd,
            track_cmd,
            master_cmd,
            metronome_cmd,
            get_peaks,
            export_wav,
            record_start,
            record_stop,
            rundown_set,
            rundown_play,
            tasks_save,
            settings_save,
            devices_list,
            switch_device,
            remote_info,
            window_mini,
            window_top,
            script_set,
        ])
        .run(tauri::generate_context!())
        .expect("tauri 运行失败");
}

fn send_app(app: &App, cmd: Command) {
    owner::send(&app.engines, cmd);
}

// ---------- 全局快捷键 ----------

fn default_hotkeys() -> HashMap<String, String> {
    HashMap::from([
        ("play_all".into(), "CmdOrCtrl+Alt+P".into()),
        ("pause_all".into(), "CmdOrCtrl+Alt+S".into()),
        ("fadeout_all".into(), "CmdOrCtrl+Alt+F".into()),
        ("metronome".into(), "CmdOrCtrl+Alt+M".into()),
    ])
}

/// 构建 Shortcut → 动作 的映射(设置覆盖默认;素材热键 action = "play:<id>")。
fn hotkey_map(app: &App) -> HashMap<Shortcut, String> {
    let core = app.core.lock();
    let mut pairs: Vec<(String, String)> = default_hotkeys().into_iter().collect();
    for (k, v) in &core.data.settings.hotkeys {
        pairs.push((k.clone(), v.clone()));
    }
    for t in &core.data.library {
        if let Some(a) = &t.hotkey {
            pairs.push((format!("play:{}", t.id), a.clone()));
        }
    }
    let mut map = HashMap::new();
    for (action, accel) in pairs {
        if let Ok(s) = accel.parse::<Shortcut>() {
            map.insert(s, action);
        }
    }
    map
}

fn register_hotkeys(handle: &AppHandle, app: &App) {
    let gs = handle.global_shortcut();
    let _ = gs.unregister_all();
    for s in hotkey_map(app).keys() {
        let _ = gs.register(*s);
    }
}

fn handle_hotkey(app_handle: &AppHandle, shortcut: &Shortcut) {
    let Some(app) = app_handle.try_state::<Arc<App>>() else { return };
    let Some(action) = hotkey_map(&app).get(shortcut).cloned() else { return };
    match action.as_str() {
        "play_all" => send_app(&app, Command::All(TrackCmd::Play)),
        "pause_all" => send_app(&app, Command::All(TrackCmd::Pause)),
        "fadeout_all" => {
            send_app(&app, Command::All(TrackCmd::FadeTo { target: 0.0, secs: 2.0 }))
        }
        "metronome" => {
            let on = !app.core.lock().data.settings.metro_on;
            let bpm = app.core.lock().data.settings.bpm;
            send_app(&app, Command::Master(MasterCmd::Metronome { on, bpm, gain: 0.6 }));
        }
        a if a.starts_with("play:") => {
            if let Ok(id) = a[5..].parse::<u64>() {
                play_with_auto_fade(&app, id);
            }
        }
        a if a.starts_with("group:") => {
            let g = a[6..].to_string();
            let ids: Vec<u64> = {
                let core = app.core.lock();
                core.data
                    .library
                    .iter()
                    .filter(|t| t.kind == TrackKind::File && t.group == g)
                    .map(|t| t.id)
                    .collect()
            };
            for id in ids {
                play_with_auto_fade(&app, id);
            }
        }
        _ => {}
    }
}

// ---------- Tauri 命令 ----------

#[tauri::command]
async fn get_all(app: State<'_, Arc<App>>) -> Result<serde_json::Value, String> {
    let core = app.core.lock();
    let peaks_ready: Vec<u64> = core.peaks.keys().copied().collect();
    Ok(serde_json::json!({ "data": core.data, "peaks_ready": peaks_ready, "pinyin": pinyin_of_all(&core.data) }))
}

fn pinyin_of_all(data: &store::Persisted) -> HashMap<u64, String> {
    data.library
        .iter()
        .map(|t| (t.id, pinyin_key(&t.name)))
        .collect()
}

#[tauri::command]
async fn add_files(handle: AppHandle, app: State<'_, Arc<App>>, paths: Vec<String>) -> Result<usize, String> {
    let n = paths.len();
    let app2: Arc<App> = Arc::clone(&app);
    std::thread::spawn(move || {
        app2.add_files_blocking(paths, "默认分组".into());
        let _ = handle.emit("library_changed", ());
    });
    Ok(n)
}

#[tauri::command]
fn add_text_card(app: State<Arc<App>>, title: String, text: String) -> u64 {
    let mut core = app.core.lock();
    let id = core.data.next_id();
    core.data.library.push(TrackMeta {
        id,
        kind: TrackKind::Text,
        name: title,
        group: "文字卡".into(),
        text,
        ..Default::default()
    });
    let _ = core.data.save(&app.data_dir);
    id
}

#[tauri::command]
async fn update_meta(handle: AppHandle, app: State<'_, Arc<App>>, meta: TrackMeta) -> Result<bool, String> {
    {
        let mut core = app.core.lock();
        match core.data.library.iter_mut().find(|t| t.id == meta.id) {
            Some(t) => *t = meta.clone(),
            None => return Ok(false),
        }
        let _ = core.data.save(&app.data_dir);
    }
    // 若该轨已加载,立即套用
    let idx = app
        .core
        .lock()
        .sessions
        .iter()
        .find(|s| s.lib_id == meta.id)
        .map(|s| s.engine_idx);
    if let Some(i) = idx {
        app.apply_meta_to_track(i, &meta);
    }
    register_hotkeys(&handle, &app);
    Ok(true)
}

#[tauri::command]
async fn remove_id(handle: AppHandle, app: State<'_, Arc<App>>, id: u64) -> Result<bool, String> {
    {
        let mut core = app.core.lock();
        core.data.library.retain(|t| t.id != id);
        core.data.rundown.retain(|i| *i != id);
        if core.data.active_script == Some(id) {
            core.data.active_script = None;
        }
        let _ = core.data.save(&app.data_dir);
    }
    // 引擎侧同步删除:已加载则删轨,其后会话索引前移,防止映射错位
    {
        let mut core = app.core.lock();
        if let Some(pos) = core.sessions.iter().position(|s| s.lib_id == id) {
            let idx = core.sessions[pos].engine_idx;
            owner::send(&app.engines, Command::Master(MasterCmd::RemoveTrack(idx)));
            core.sessions.remove(pos);
            for s in core.sessions.iter_mut() {
                if s.engine_idx > idx {
                    s.engine_idx -= 1;
                }
            }
        }
        core.pending.retain(|p| *p != id);
    }
    register_hotkeys(&handle, &app);
    Ok(true)
}

#[tauri::command]
async fn add_group(app: State<'_, Arc<App>>, name: String) -> Result<bool, String> {
    let mut core = app.core.lock();
    if core.data.groups.iter().any(|g| g == &name) {
        return Ok(false);
    }
    core.data.groups.push(name);
    let _ = core.data.save(&app.data_dir);
    Ok(true)
}

#[tauri::command]
async fn remove_group(app: State<'_, Arc<App>>, name: String) -> Result<(), String> {
    let mut core = app.core.lock();
    core.data.groups.retain(|g| g != &name);
    for t in core.data.library.iter_mut() {
        if t.group == name {
            t.group = "默认分组".into();
        }
    }
    let _ = core.data.save(&app.data_dir);
    Ok(())
}

#[tauri::command]
async fn play_id(app: State<'_, Arc<App>>, id: u64) -> Result<(), String> {
    play_with_auto_fade(&app, id);
    Ok(())
}

/// 播放素材;default_fade > 0 时从 0 音量淡入(切素材无爆音)。
fn play_with_auto_fade(app: &Arc<App>, id: u64) {
    let fade = app.core.lock().data.settings.default_fade;
    app.play_id(id);
    if fade > 0.0 {
        if let Some(idx) = app
            .core
            .lock()
            .sessions
            .iter()
            .find(|s| s.lib_id == id)
            .map(|s| s.engine_idx)
        {
            let gain = app
                .core
                .lock()
                .data
                .track(id)
                .map(|t| t.gain)
                .unwrap_or(1.0);
            send_app(app, Command::Track { index: idx, cmd: TrackCmd::SetGain(0.0) });
            send_app(
                app,
                Command::Track {
                    index: idx,
                    cmd: TrackCmd::FadeTo { target: gain, secs: fade },
                },
            );
        }
    }
}

/// 分组一键齐放:组内全部文件素材同时从入点播放。
#[tauri::command]
async fn play_group(app: State<'_, Arc<App>>, group: String) -> Result<usize, String> {
    let ids: Vec<u64> = {
        let core = app.core.lock();
        core.data
            .library
            .iter()
            .filter(|t| t.kind == TrackKind::File && t.group == group)
            .map(|t| t.id)
            .collect()
    };
    let n = ids.len();
    for id in ids {
        play_with_auto_fade(&app, id);
    }
    Ok(n)
}

/// 输入框聚焦期间挂起/恢复全局热键(避免打字触发播放)。
#[tauri::command]
async fn hotkeys_suspend(handle: AppHandle, app: State<'_, Arc<App>>, on: bool) -> Result<(), String> {
    if on {
        let _ = handle.global_shortcut().unregister_all();
    } else {
        register_hotkeys(&handle, &app);
    }
    Ok(())
}

#[tauri::command]
async fn playcue_id(app: State<'_, Arc<App>>, id: u64) -> Result<(), String> {
    let idx = app
        .core
        .lock()
        .sessions
        .iter()
        .find(|s| s.lib_id == id)
        .map(|s| s.engine_idx);
    if let Some(i) = idx {
        send_app(&app, Command::Track { index: i, cmd: TrackCmd::PlayFromCue });
    } else {
        app.play_id(id);
    }
    Ok(())
}

#[tauri::command]
async fn stop_id(app: State<'_, Arc<App>>, id: u64) -> Result<(), String> {
    let idx = app
        .core
        .lock()
        .sessions
        .iter()
        .find(|s| s.lib_id == id)
        .map(|s| s.engine_idx);
    if let Some(i) = idx {
        send_app(&app, Command::Track { index: i, cmd: TrackCmd::Pause });
    }
    Ok(())
}

/// 淡出全停(演出"黑场"等价物)。
#[tauri::command]
async fn stop_all(app: State<'_, Arc<App>>) -> Result<(), String> {
    send_app(&app, Command::All(TrackCmd::FadeTo { target: 0.0, secs: 0.5 }));
    let engines = app.engines.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        owner::send(&engines, Command::All(TrackCmd::Pause));
        owner::send(&engines, Command::All(TrackCmd::SetGain(1.0)));
    });
    Ok(())
}

#[tauri::command]
async fn all_cmd(app: State<'_, Arc<App>>, op: String, a: f64, b: f64) -> Result<(), String> {
    if let Some(cmd) = parse_op(&op, a, b) {
        send_app(&app, Command::All(cmd));
    }
    Ok(())
}

#[tauri::command]
async fn track_cmd(app: State<'_, Arc<App>>, index: usize, op: String, a: f64, b: f64) -> Result<(), String> {
    if let Some(cmd) = parse_op(&op, a, b) {
        send_app(&app, Command::Track { index, cmd });
    }
    Ok(())
}

#[tauri::command]
async fn master_cmd(app: State<'_, Arc<App>>, gain: f64) -> Result<(), String> {
    send_app(&app, Command::Master(MasterCmd::SetMasterGain(gain as f32)));
    Ok(())
}

#[tauri::command]
async fn metronome_cmd(app: State<'_, Arc<App>>, on: bool, bpm: f64, gain: f64) -> Result<(), String> {
    send_app(&app, Command::Master(MasterCmd::Metronome { on, bpm, gain: gain as f32 }));
    Ok(())
}

/// 波形峰值:命中缓存立即返回;未命中后台计算并 emit peaks_ready。
#[tauri::command]
async fn get_peaks(handle: AppHandle, app: State<'_, Arc<App>>, id: u64) -> Result<Option<Vec<playback::PeakPair>>, String> {
    {
        let mut core = app.core.lock();
        if let Some(p) = core.peaks.get(&id) {
            // 空占位 = 正在计算中,避免重复起解码线程
            return Ok(if p.is_empty() { None } else { Some(p.as_ref().clone()) });
        }
        core.peaks.insert(id, Arc::new(Vec::new())); // 占位
    }
    let app2: Arc<App> = Arc::clone(&app);
    std::thread::spawn(move || {
        let meta = app2.core.lock().data.track(id).cloned();
        let Some(meta) = meta.filter(|m| m.kind == TrackKind::File) else { return };
        if let Some(audio) = state::load_cached(&app2, &meta.path) {
            let peaks = Arc::new(playback::compute_peaks(&audio, 1200));
            app2.core.lock().peaks.insert(id, peaks);
            let _ = handle.emit("peaks_ready", id);
        }
    });
    Ok(None)
}

/// 导出(含入出点裁剪)为 16-bit WAV。
#[tauri::command]
async fn export_wav(app: State<'_, Arc<App>>, id: u64, path: String) -> Result<f64, String> {
    let meta = app.core.lock().data.track(id).cloned();
    let Some(meta) = meta.filter(|m| m.kind == TrackKind::File) else {
        return Err("素材不存在".into());
    };
    let audio = decode::load(std::path::Path::new(&meta.path)).map_err(|e| format!("{e:#}"))?;
    let sr = audio.sample_rate as f64;
    let total = audio.total_frames();
    let lo = ((meta.cue_in.unwrap_or(0.0) * sr) as usize).min(total);
    let hi = (((meta.cue_out.unwrap_or(audio.duration_secs())) * sr) as usize)
        .min(total)
        .max(lo);
    let ch = audio.channels;
    let trimmed: Vec<f32> = audio.samples[lo * ch..hi * ch].to_vec();
    let data = AudioData::new(trimmed, audio.sample_rate, ch);
    decode::write_wav(std::path::Path::new(&path), &data)
        .map(|_| data.duration_secs())
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
async fn record_start(app: State<'_, Arc<App>>) -> Result<(), String> {
    let mut rec = app.rec.lock();
    if rec.is_some() {
        return Err("已有录音进行中".into());
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = app.data_dir.join("uploads").join(format!("rec_{ts}.wav"));
    match recorder::start(path) {
        Ok(r) => {
            *rec = Some(r);
            Ok(())
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

#[tauri::command]
async fn record_stop(handle: AppHandle, app: State<'_, Arc<App>>) -> Result<serde_json::Value, String> {
    let rec = app.rec.lock().take();
    let Some(rec) = rec else {
        return Err("没有进行中的录音".into());
    };
    let path = rec.stop().map_err(|e| format!("{e:#}"))?;
    let p = path.to_string_lossy().into_owned();
    // 解码入库放后台:record_stop 立刻返回,不占用调用线程
    let app2: Arc<App> = Arc::clone(&app);
    let p_bg = p.clone();
    std::thread::spawn(move || {
        if let Some(m) = app2.import_one(&p_bg, "录音") {
            app2.core.lock().data.library.push(m);
            app2.save();
        }
        let _ = handle.emit("library_changed", ());
    });
    Ok(serde_json::json!({ "path": p }))
}

#[tauri::command]
async fn rundown_set(app: State<'_, Arc<App>>, ids: Vec<u64>, enabled: bool) -> Result<(), String> {
    let mut core = app.core.lock();
    core.data.rundown = ids;
    core.data.rundown_enabled = enabled;
    let _ = core.data.save(&app.data_dir);
    Ok(())
}

#[tauri::command]
async fn rundown_play(app: State<'_, Arc<App>>) -> Result<(), String> {
    let first = app.core.lock().data.rundown.first().copied();
    if let Some(id) = first {
        send_app(&app, Command::All(TrackCmd::Pause));
        app.play_id(id);
    }
    Ok(())
}

#[tauri::command]
async fn tasks_save(app: State<'_, Arc<App>>, tasks: Vec<ScheduleTask>) -> Result<(), String> {
    let mut core = app.core.lock();
    core.data.tasks = tasks;
    let _ = core.data.save(&app.data_dir);
    Ok(())
}

#[tauri::command]
async fn settings_save(handle: AppHandle, app: State<'_, Arc<App>>, settings: Settings) -> Result<(), String> {
    let mtc = settings.mtc_enabled;
    let metro = (settings.metro_on, settings.bpm, settings.metro_gain);
    let master = settings.master_gain;
    {
        let mut core = app.core.lock();
        core.data.settings = settings;
        let _ = core.data.save(&app.data_dir);
    }
    app.mtc_enabled.store(mtc, Ordering::Relaxed);
    send_app(&app, Command::Master(MasterCmd::SetMasterGain(master)));
    send_app(
        &app,
        Command::Master(MasterCmd::Metronome { on: metro.0, bpm: metro.1, gain: metro.2 }),
    );
    register_hotkeys(&handle, &app);
    Ok(())
}

#[tauri::command]
fn devices_list() -> Vec<String> {
    playback::engine_list_output_devices()
}

#[tauri::command]
async fn switch_device(app: State<'_, Arc<App>>, name: Option<String>) -> Result<(), String> {
    {
        let mut core = app.core.lock();
        core.data.settings.device = name.clone();
        let _ = core.data.save(&app.data_dir);
    }
    // 属主线程重启引擎(当前播放清空;素材库保留)
    let _ = app.owner_tx.send(owner::OwnerCmd::SwitchDevice(name));
    Ok(())
}

#[tauri::command]
fn remote_info(app: State<Arc<App>>) -> serde_json::Value {
    let ip = remote::lan_ip().unwrap_or_else(|| "127.0.0.1".into());
    let url = format!("http://{ip}:{}", app.remote_port);
    serde_json::json!({ "port": app.remote_port, "url": url, "svg": remote::qr_svg(&url) })
}

#[tauri::command]
fn window_mini(handle: AppHandle, mini: bool) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.set_size(tauri::PhysicalSize::new(
            if mini { 460 } else { 1040 },
            if mini { 200 } else { 720 },
        ));
    }
}

#[tauri::command]
fn window_top(handle: AppHandle, on: bool) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.set_always_on_top(on);
    }
}

#[tauri::command]
async fn script_set(app: State<'_, Arc<App>>, id: Option<u64>) -> Result<(), String> {
    let mut core = app.core.lock();
    core.data.active_script = id;
    let _ = core.data.save(&app.data_dir);
    Ok(())
}
