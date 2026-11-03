//! 持久化状态:素材库、分组、节目单、定时任务、设置。
//! 存储为 app-data 目录下的一个 JSON。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    File,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EndAction {
    /// 播完停止
    Stop,
    /// 播完循环(引擎 loop)
    Loop,
    /// 播完接节目单下一条
    Next,
}

/// 素材库条目(文件素材 / 文字卡片)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackMeta {
    pub id: u64,
    pub kind: TrackKind,
    pub name: String,
    pub path: String,
    pub duration: f64,
    pub group: String,
    /// 默认音量/速度/入出点/循环(播放时套用)
    pub gain: f32,
    pub speed: f32,
    pub cue_in: Option<f64>,
    pub cue_out: Option<f64>,
    pub loop_on: bool,
    pub end_action: EndAction,
    pub hotkey: Option<String>,
    pub note: String,
    /// 文字卡片内容
    pub text: String,
    /// 颜色标签(css color)
    pub color: Option<String>,
}

impl Default for TrackMeta {
    fn default() -> Self {
        Self {
            id: 0,
            kind: TrackKind::File,
            name: String::new(),
            path: String::new(),
            duration: 0.0,
            group: "默认分组".into(),
            gain: 1.0,
            speed: 1.0,
            cue_in: None,
            cue_out: None,
            loop_on: false,
            end_action: EndAction::Stop,
            hotkey: None,
            note: String::new(),
            text: String::new(),
            color: None,
        }
    }
}

/// 定时任务:在指定时刻触发。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTask {
    pub id: u64,
    /// "HH:MM" 24 小时制
    pub time: String,
    /// 星期几(1=周一 … 7=周日),空 = 每天
    pub days: Vec<u8>,
    /// 播放什么:PlayRundown / 播放素材 id
    pub action: ScheduleAction,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    PlayRundown,
    PlayTrack(u64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: String,        // dark | light
    pub card_size: String,    // big | small | list
    pub default_fade: f64,    // 秒
    pub double_click_play: bool,
    pub master_gain: f32,
    pub mtc_enabled: bool,
    pub mtc_fps: u32,         // 24 | 25 | 30
    pub metro_on: bool,
    pub bpm: f64,
    pub metro_gain: f32,
    pub device: Option<String>,
    pub remote_port: u16,
    pub hotkeys: HashMap<String, String>, // 动作 → 加速键
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            card_size: "big".into(),
            default_fade: 2.0,
            double_click_play: true,
            master_gain: 1.0,
            mtc_enabled: false,
            mtc_fps: 30,
            metro_on: false,
            bpm: 120.0,
            metro_gain: 0.6,
            device: None,
            remote_port: 8765,
            hotkeys: HashMap::new(),
        }
    }
}

/// 播放中的会话轨道:引擎索引 ↔ 素材库 id。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionTrack {
    pub engine_idx: usize,
    pub lib_id: u64,
}

/// 全部可持久化数据。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Persisted {
    pub library: Vec<TrackMeta>,
    pub groups: Vec<String>,
    pub settings: Settings,
    /// 节目单(素材 id 顺序)
    pub rundown: Vec<u64>,
    /// 节目单连播开关
    pub rundown_enabled: bool,
    pub tasks: Vec<ScheduleTask>,
    /// 当前司仪宝使用的文字卡 id
    pub active_script: Option<u64>,
}

impl Persisted {
    pub fn load(dir: &Path) -> Self {
        let file = dir.join("store.json");
        match std::fs::read_to_string(&file) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("[store] 解析失败,使用默认:{}(备份为 store.json.bad)", e);
                let _ = std::fs::rename(&file, dir.join("store.json.bad"));
                Self::default()
            }),
            Err(_) => Self {
                groups: vec!["默认分组".into()],
                ..Self::default()
            },
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let file = dir.join("store.json");
        let tmp = dir.join("store.json.tmp");
        let text = serde_json::to_string_pretty(self).context("序列化失败")?;
        std::fs::write(&tmp, text).context("写临时文件失败")?;
        std::fs::rename(&tmp, &file).context("落盘失败(原子替换)")?;
        Ok(())
    }

    pub fn next_id(&self) -> u64 {
        let lib_max = self.library.iter().map(|t| t.id).max().unwrap_or(1000);
        let task_max = self.tasks.iter().map(|t| t.id).max().unwrap_or(0);
        lib_max.max(task_max) + 1
    }

    pub fn track(&self, id: u64) -> Option<&TrackMeta> {
        self.library.iter().find(|t| t.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_roundtrip_with_defaults() {
        let mut p = Persisted::default();
        p.library.push(TrackMeta { id: 1, name: "a".into(), ..Default::default() });
        p.settings.bpm = 96.0;
        // 旧版本缺字段的 JSON 也能解析(default)
        let json = r#"{"library":[{"id":2,"name":"b"}],"settings":{"bpm":60.0}}"#;
        let p2: Persisted = serde_json::from_str(json).unwrap();
        assert_eq!(p2.library[0].gain, 1.0);
        assert_eq!(p2.settings.bpm, 60.0);
        assert_eq!(p2.groups.len(), 0); // 显式空数组覆盖 default
        let round = serde_json::to_string(&p).unwrap();
        let p3: Persisted = serde_json::from_str(&round).unwrap();
        assert_eq!(p3.library[0].name, "a");
        assert_eq!(p3.settings.bpm, 96.0);
    }
}
