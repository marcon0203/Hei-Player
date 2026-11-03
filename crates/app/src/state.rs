//! 全局共享状态:持久化数据 + 会话映射 + 各类句柄。
//! Tauri 命令与手机端 HTTP 服务共用一个 `Arc<App>`。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use parking_lot::Mutex;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::owner::{EngineSlot, EventHub};
use crate::store::{EndAction, Persisted, SessionTrack, TrackMeta};

/// 解码缓存:首次解码后转 16-bit WAV 存 data_dir/cache,
/// 之后直接解码缓存文件(WAV 解码近乎 memcpy)。
/// key = 路径 + mtime(文件变了自动失效)。
pub fn load_cached(app: &App, path: &str) -> Option<std::sync::Arc<playback::AudioData>> {
    let p = std::path::Path::new(path);
    let key = {
        let mut h = DefaultHasher::new();
        path.hash(&mut h);
        if let Ok(md) = std::fs::metadata(p) {
            if let Ok(m) = md.modified() {
                m.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| (d.as_secs(), d.subsec_nanos()))
                    .unwrap_or((0, 0))
                    .hash(&mut h);
            }
        }
        format!("{:016x}", h.finish())
    };
    let cache = app.data_dir.join("cache");
    let _ = std::fs::create_dir_all(&cache);
    let wav = cache.join(format!("{key}.wav"));

    if let Ok(audio) = decode::load(&wav) {
        return Some(audio);
    }
    let audio = decode::load(p).ok()?;
    let _ = decode::write_wav(&wav, &audio);
    Some(audio)
}

/// 核心可变状态(锁保护)。
pub struct Core {
    pub data: Persisted,
    /// 引擎轨道索引 ↔ 素材库 id
    pub sessions: Vec<SessionTrack>,
    /// 等待关联的 AddTrack(lib id 队列;状态到达时按新索引顺序消费)
    pub pending: Vec<u64>,
    /// 峰值缓存:{lib_id: peaks}
    pub peaks: std::collections::HashMap<u64, Arc<Vec<playback::PeakPair>>>,
}

pub struct App {
    pub core: Mutex<Core>,
    pub hub: EventHub,
    pub engines: EngineSlot,
    pub data_dir: PathBuf,
    pub remote_port: u16,
    pub mtc_enabled: Arc<AtomicBool>,
    /// 进行中的麦克风录音
    pub rec: Mutex<Option<Arc<crate::recorder::Recording>>>,
    /// 引擎属主控制通道(设备切换)
    pub owner_tx: crossbeam_channel::Sender<crate::owner::OwnerCmd>,
    /// 串行化 pending 登记 + AddTrack 发送(多线程加载时保序)
    pub add_gate: Mutex<()>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data: Persisted,
        hub: EventHub,
        engines: EngineSlot,
        data_dir: PathBuf,
        remote_port: u16,
        mtc_enabled: Arc<AtomicBool>,
        owner_tx: crossbeam_channel::Sender<crate::owner::OwnerCmd>,
    ) -> Arc<Self> {
        Arc::new(Self {
            core: Mutex::new(Core {
                data,
                sessions: Vec::new(),
                pending: Vec::new(),
                peaks: Default::default(),
            }),
            hub,
            engines,
            data_dir,
            remote_port,
            mtc_enabled,
            rec: Mutex::new(None),
            owner_tx,
            add_gate: Mutex::new(()),
        })
    }

    pub fn save(&self) {
        let core = self.core.lock();
        if let Err(e) = core.data.save(&self.data_dir) {
            eprintln!("[store] 保存失败:{e:#}");
        }
    }

    /// 同步解码并入库(手机上传路径;桌面路径走后台线程版本)。
    pub fn add_files_blocking(self: &Arc<Self>, paths: Vec<String>, group: String) {
        for p in paths {
            if let Some(meta) = self.import_one(&p, &group) {
                self.core.lock().data.library.push(meta);
            }
        }
        self.save();
    }

    /// 解码探测 + 入库条目(不加载进引擎)。
    pub fn import_one(self: &Arc<Self>, path: &str, group: &str) -> Option<TrackMeta> {
        let p = std::path::Path::new(path);
        let audio = match decode::load(p) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[library] 解码失败 {path}:{e:#}");
                return None;
            }
        };
        let id = self.core.lock().data.next_id();
        Some(TrackMeta {
            id,
            kind: crate::store::TrackKind::File,
            name: p
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("未命名")
                .to_owned(),
            path: path.to_owned(),
            duration: audio.duration_secs(),
            group: group.to_owned(),
            ..Default::default()
        })
    }

    /// 确保素材已加载进引擎;返回已加载轨道的引擎索引。
    /// 未加载:后台线程解码 → (gate 内)登记 pending + 发送 AddTrack,
    /// 加载完成由事件泵自动 PlayFromCue。**调用线程绝不阻塞在解码上。**
    pub fn ensure_loaded(self: &Arc<Self>, lib_id: u64) -> Option<usize> {
        {
            let core = self.core.lock();
            if let Some(s) = core.sessions.iter().find(|s| s.lib_id == lib_id) {
                return Some(s.engine_idx);
            }
            // 已在加载队列,别重复起线程
            if core.pending.contains(&lib_id) {
                return None;
            }
            if !core.data.library.iter().any(|t| t.id == lib_id) {
                return None;
            }
        }
        let meta = self.core.lock().data.track(lib_id)?.clone();
        if matches!(meta.kind, crate::store::TrackKind::Text) {
            return None;
        }
        // 先登记 pending 再解码(防止解码完成后错过绑定窗口)
        self.core.lock().pending.push(lib_id);
        let app2: Arc<Self> = Arc::clone(self);
        std::thread::spawn(move || {
            let Some(audio) = load_cached(&app2, &meta.path) else {
                // 解码失败:回滚 pending,别让后续绑定错位
                app2.core.lock().pending.retain(|id| *id != lib_id);
                eprintln!("[library] 加载失败:{}", meta.path);
                return;
            };
            let _gate = app2.add_gate.lock();
            crate::owner::send(
                &app2.engines,
                playback::Command::AddTrack { name: meta.name.clone(), data: audio },
            );
        });
        None
    }

    /// 套用素材库设置(音量/速度/入出点/循环)到引擎轨道。
    pub fn apply_meta_to_track(self: &Arc<Self>, engine_idx: usize, meta: &TrackMeta) {
        use playback::{Command, TrackCmd};
        let send_track = |cmd: TrackCmd| {
            crate::owner::send(&self.engines, Command::Track { index: engine_idx, cmd })
        };
        send_track(TrackCmd::SetGain(meta.gain));
        send_track(TrackCmd::SetSpeed(meta.speed));
        send_track(TrackCmd::SetCue { cue_in: meta.cue_in, cue_out: meta.cue_out });
        send_track(TrackCmd::LoopAll(meta.loop_on || meta.end_action == EndAction::Loop));
    }

    /// 播放素材(未加载则加载;加载完成由事件线程接续)。
    pub fn play_id(self: &Arc<Self>, lib_id: u64) {
        if let Some(idx) = self.ensure_loaded(lib_id) {
            let meta = self.core.lock().data.track(lib_id).cloned();
            self.apply_meta_to_track(idx, meta.as_ref().unwrap());
            crate::owner::send(
                &self.engines,
                playback::Command::Track {
                    index: idx,
                    cmd: playback::TrackCmd::PlayFromCue,
                },
            );
        }
        // 刚加载(pending)的情况:事件线程在会话回填后自动 PlayFromCue
    }

    /// Status 到达时同步会话映射;返回本次新绑定的 (engine_idx, lib_id),
    /// 供事件线程做"加载即播放"接续。
    pub fn sync_sessions(self: &Arc<Self>, track_count: usize) -> Vec<(usize, u64)> {
        let mut core = self.core.lock();
        core.sessions.retain(|s| s.engine_idx < track_count);
        let known: std::collections::HashSet<usize> =
            core.sessions.iter().map(|s| s.engine_idx).collect();
        let mut bound = Vec::new();
        for idx in 0..track_count {
            if !known.contains(&idx) {
                if let Some(lib_id) = core.pending.first().copied() {
                    core.pending.remove(0);
                    core.sessions.push(SessionTrack { engine_idx: idx, lib_id });
                    bound.push((idx, lib_id));
                }
            }
        }
        bound
    }

    /// TrackEnded 处理:节目单连播推进。
    pub fn on_track_ended(self: &Arc<Self>, engine_idx: usize) {
        let next_id: Option<u64> = {
            let core = self.core.lock();
            let Some(sess) = core.sessions.iter().find(|s| s.engine_idx == engine_idx) else {
                return;
            };
            let lib_id = sess.lib_id;
            let Some(meta) = core.data.track(lib_id) else { return };
            match meta.end_action {
                EndAction::Next if core.data.rundown_enabled => {
                    let pos = core.data.rundown.iter().position(|id| *id == lib_id);
                    pos.and_then(|p| {
                        core.data.rundown.get(p + 1).copied().or_else(|| core.data.rundown.first().copied())
                    })
                }
                _ => None,
            }
        };
        if let Some(id) = next_id {
            self.play_id(id);
        }
    }
}
