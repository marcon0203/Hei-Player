//! 引擎对外类型:音频数据、控制命令、事件。

use std::sync::Arc;

/// 解码后的整段音频(交错存储,归一化 f32)。
///
/// 整段载入内存是 P0 的取舍:舞台播控场景单曲目几十 MB、
/// 且"预热/即点即播"本来就要求素材常驻。后续大文件再改为分页。
#[derive(Debug)]
pub struct AudioData {
    pub samples: Arc<Vec<f32>>,
    pub sample_rate: u32,
    pub channels: usize,
}

impl AudioData {
    pub fn new(samples: Vec<f32>, sample_rate: u32, channels: usize) -> Self {
        debug_assert!(channels > 0);
        Self { samples: Arc::new(samples), sample_rate, channels }
    }

    /// 总帧数(每帧 = 每声道各一个采样)。
    pub fn total_frames(&self) -> usize {
        match self.channels {
            0 => 0,
            ch => self.samples.len() / ch,
        }
    }

    pub fn duration_secs(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.total_frames() as f64 / self.sample_rate as f64
        }
    }
}

/// 作用于单条轨道的操作。
#[derive(Debug, Clone)]
pub enum TrackCmd {
    Play,
    Pause,
    /// Play + Pause 翻转
    Toggle,
    /// 立即设置音量(0.0 ~ MAX_GAIN,不做斜坡)
    SetGain(f32),
    /// 在 `secs` 秒内把音量斜坡到 target(0 = 淡出,配 Pause 即"淡出后停")
    FadeTo { target: f32, secs: f64 },
    /// 变速(0.25 ~ 4.0,1.0 = 原速)
    SetSpeed(f32),
    /// 倒放开关
    SetReverse(bool),
    /// 循环开关(在入出点之间循环;未设入出点则整曲)
    LoopAll(bool),
    /// 跳转到秒(相对素材原始时间轴)
    SeekSecs(f64),
    /// 设置入点(None = 0)/ 出点(None = 结尾),秒,相对原始时间轴
    SetCue { cue_in: Option<f64>, cue_out: Option<f64> },
    /// 从入点开始播放(裁剪语义下最常用的"播放这段")
    PlayFromCue,
}

/// 引擎级命令(不针对单轨)。
#[derive(Debug, Clone)]
pub enum MasterCmd {
    /// 主音量(作用于最终混音输出)
    SetMasterGain(f32),
    /// 节拍器:on/off、BPM、每拍响度
    Metronome { on: bool, bpm: f64, gain: f32 },
    /// 移除轨道
    RemoveTrack(usize),
    /// 清空全部轨道
    ClearTracks,
}

/// UI/CLI → 引擎的控制命令。全部非阻塞投递,在音频回调开头统一消费。
#[derive(Debug, Clone)]
pub enum Command {
    /// 作用于全部轨道
    All(TrackCmd),
    /// 只作用于第 index 条轨道(越界忽略)
    Track { index: usize, cmd: TrackCmd },
    /// 加一个轨道参与混音
    AddTrack { name: String, data: Arc<AudioData> },
    /// 引擎级命令
    Master(MasterCmd),
    /// 请求一次全量状态快照(回应 Event::Status)
    Status,
}

/// 轨道状态快照。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TrackStatus {
    pub index: usize,
    pub name: String,
    pub playing: bool,
    /// 相对素材原始时间轴
    pub position_secs: f64,
    pub duration_secs: f64,
    pub gain: f32,
    pub speed: f32,
    pub reverse: bool,
    pub looping: bool,
    pub ended: bool,
    pub cue_in_secs: Option<f64>,
    pub cue_out_secs: Option<f64>,
    /// 有效播放时长(出点-入点)
    pub trimmed_secs: f64,
}

/// 引擎整体状态。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EngineStatus {
    pub tracks: Vec<TrackStatus>,
    pub master_gain: f32,
    pub metronome_on: bool,
    pub bpm: f64,
    /// 自引擎启动以来累计渲染的输出帧数(时钟源,驱动 MTC)
    pub frames_rendered: u64,
    pub sample_rate: u32,
    pub channels: usize,
}

/// 引擎 → UI/CLI 的事件。有界通道,丢弃策略 = 丢旧保新不阻塞回调。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Status(Box<EngineStatus>),
    /// 某轨自然播完(非暂停、非手动停;裁剪范围内播完也算)
    TrackEnded { index: usize, name: String },
}
