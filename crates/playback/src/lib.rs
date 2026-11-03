//! playback —— 音频播放引擎。
//!
//! 架构:cpal 回调持有 `Engine`,命令经 crossbeam 通道非阻塞注入,
//! 事件有界回传。P1 覆盖:多轨混音、淡入淡出、变速、倒放、入出点裁剪、
//! 循环、seek、主音量、节拍器、峰值(波形)、设备切换(可停机重启)。

mod engine;
mod metronome;
mod peaks;
mod track;
pub mod types;

pub use engine::{list_output_devices as engine_list_output_devices, Engine, EngineChannels, EngineHandle};
pub use peaks::{compute_peaks, decode_peaks, encode_peaks, PeakPair};
pub use track::{MAX_GAIN, MAX_SPEED, MIN_SPEED, Track};
pub use types::{AudioData, Command, EngineStatus, Event, MasterCmd, TrackCmd, TrackStatus};
