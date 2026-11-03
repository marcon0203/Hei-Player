//! 混音引擎与 cpal 输出集成。
//!
//! 实时安全约定(回调路径):
//! - 只做 try_recv / try_send,绝不阻塞、绝不分配(命令/事件通道预建)
//! - Track 数据全部 Arc 预载,`mix_frame`/`advance` 原地更新
//! - 所有布局/设备协商在启动线程完成
//!
//! Stream 非 Send:由 `StreamGuard` 线程持有,shutdown 信号释放(支持设备切换)。

use anyhow::{anyhow, bail, Context};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

use crate::metronome::Metronome;
use crate::track::Track;
use crate::types::{Command, EngineStatus, Event, MasterCmd, TrackCmd, TrackStatus};

#[cfg(test)]
use crate::types::AudioData;

/// 对外句柄:命令通道 + 事件通道 + 设备信息。
/// cpal 的 Stream 既非 Send 也非 Sync:本结构体只允许在创建它的线程上
/// 存活(由上层"引擎属主线程"持有),跨线程只传 `EngineChannels`。
pub struct EngineHandle {
    pub cmd_tx: Sender<Command>,
    pub evt_rx: Receiver<Event>,
    pub sample_rate: u32,
    pub channels: usize,
    _stream: Stream,
}

/// 可跨线程传递的引擎通道。
#[derive(Clone)]
pub struct EngineChannels {
    pub cmd_tx: Sender<Command>,
    pub sample_rate: u32,
    pub channels: usize,
}

/// 列出输出设备名字。
pub fn list_output_devices() -> Vec<String> {
    let host = cpal::default_host();
    let mut out = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                out.push(name);
            }
        }
    }
    out
}

impl EngineHandle {
    /// 打开默认输出设备;`device_name` 为 Some 时按名字匹配(找不到回退默认)。
    pub fn start(device_name: Option<&str>) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(want) => host
                .output_devices()?
                .find(|d| d.name().map(|n| n == want).unwrap_or(false))
                .or_else(|| host.default_output_device())
                .ok_or_else(|| anyhow!("找不到输出设备:{want}"))?,
            None => host
                .default_output_device()
                .ok_or_else(|| anyhow!("找不到默认输出设备"))?,
        };

        let supported = device
            .default_output_config()
            .context("读取默认输出配置失败")?;
        if supported.sample_format() != SampleFormat::F32 {
            bail!(
                "设备采样格式 {:?} 暂不支持(仅支持 f32)",
                supported.sample_format()
            );
        }
        let cfg: StreamConfig = supported.into();
        let sample_rate = cfg.sample_rate.0;
        let channels = cfg.channels as usize;

        let (cmd_tx, cmd_rx) = unbounded();
        let (evt_tx, evt_rx) = bounded::<Event>(1024);

        let mut engine =
            Engine::new(cmd_rx, evt_tx, sample_rate as f64, channels);

        let err_cb = |err| eprintln!("[audio] 流错误: {err}");
        let data_cb = move |data: &mut [f32], _: &_| engine.process(data);
        let stream = device
            .build_output_stream(&cfg, data_cb, err_cb, None)
            .context("构建输出流失败")?;
        stream.play().context("启动输出流失败")?;

        Ok(Self { cmd_tx, evt_rx, sample_rate, channels, _stream: stream })
    }

    pub fn start_default() -> anyhow::Result<Self> {
        Self::start(None)
    }

    /// 跨线程可用的通道快照(命令发送端克隆 + 设备信息)。
    pub fn channels(&self) -> EngineChannels {
        EngineChannels {
            cmd_tx: self.cmd_tx.clone(),
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
    }
}

/// 引擎本体,整体移入 cpal 回调闭包。
pub struct Engine {
    tracks: Vec<Track>,
    cmd_rx: Receiver<Command>,
    evt_tx: Sender<Event>,
    out_rate: f64,
    out_channels: usize,
    master_gain: f32,
    metronome: Metronome,
    frames_rendered: u64,
}

impl Engine {
    pub fn new(
        cmd_rx: Receiver<Command>,
        evt_tx: Sender<Event>,
        out_rate: f64,
        out_channels: usize,
    ) -> Self {
        Self {
            tracks: Vec::new(),
            cmd_rx,
            evt_tx,
            out_rate,
            out_channels,
            master_gain: 1.0,
            metronome: Metronome::new(out_rate),
            frames_rendered: 0,
        }
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    /// 音频回调入口:消费命令 → 清零 → 逐轨逐帧混合 → 节拍器 → 主音量。
    pub fn process(&mut self, out: &mut [f32]) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            self.apply(cmd);
        }

        let ch = self.out_channels;
        if ch == 0 {
            return;
        }
        let frames = out.len() / ch;
        out.fill(0.0);

        if !self.tracks.is_empty() {
            let mut ended = Vec::new();
            for (idx, track) in self.tracks.iter_mut().enumerate() {
                if !track.is_active() {
                    continue;
                }
                let step = track.step_per_output_frame(self.out_rate);
                for f in 0..frames {
                    let frame_out = &mut out[f * ch..(f + 1) * ch];
                    if !track.mix_frame(frame_out) || !track.advance(step) {
                        ended.push((idx, track.name.clone()));
                        break;
                    }
                }
            }
            for (index, name) in ended {
                let _ = self.evt_tx.try_send(Event::TrackEnded { index, name });
            }
        }

        self.metronome.mix(out, frames, ch);
        self.frames_rendered += frames as u64;

        if self.master_gain != 1.0 {
            for v in out.iter_mut() {
                *v *= self.master_gain;
            }
        }
    }

    fn apply(&mut self, cmd: Command) {
        match cmd {
            Command::AddTrack { name, data } => {
                self.tracks.push(Track::new(name, data));
            }
            Command::All(op) => self.apply_op(None, op),
            Command::Track { index, cmd } => self.apply_op(Some(index), cmd),
            Command::Master(m) => self.apply_master(m),
            Command::Status => {
                let snapshot = EngineStatus {
                    tracks: self
                        .tracks
                        .iter()
                        .enumerate()
                        .map(|(index, t)| TrackStatus {
                            index,
                            name: t.name.clone(),
                            playing: t.is_active(),
                            position_secs: t.position_secs(),
                            duration_secs: t.duration_secs(),
                            gain: t.gain(),
                            speed: t.speed(),
                            reverse: t.reverse(),
                            looping: t.looping(),
                            ended: t.ended(),
                            cue_in_secs: t.cue_in_secs(),
                            cue_out_secs: t.cue_out_secs(),
                            trimmed_secs: t.trimmed_secs(),
                        })
                        .collect(),
                    master_gain: self.master_gain,
                    metronome_on: self.metronome.is_on(),
                    bpm: self.metronome.bpm(),
                    frames_rendered: self.frames_rendered,
                    sample_rate: self.out_rate as u32,
                    channels: self.out_channels,
                };
                let _ = self.evt_tx.try_send(Event::Status(Box::new(snapshot)));
            }
        }
    }

    fn apply_master(&mut self, m: MasterCmd) {
        match m {
            MasterCmd::SetMasterGain(g) => self.master_gain = g.clamp(0.0, crate::track::MAX_GAIN),
            MasterCmd::Metronome { on, bpm, gain } => self.metronome.configure(on, bpm, gain),
            MasterCmd::RemoveTrack(index) => {
                if index < self.tracks.len() {
                    self.tracks.remove(index);
                }
            }
            MasterCmd::ClearTracks => self.tracks.clear(),
        }
    }

    /// `target` = None 作用于全部轨道;Some(i) 只作用于第 i 条。
    fn apply_op(&mut self, target: Option<usize>, op: TrackCmd) {
        let rate = self.out_rate;
        let pick = |idx: usize| target.is_none_or(|t| t == idx);
        for (idx, t) in self.tracks.iter_mut().enumerate() {
            if !pick(idx) {
                continue;
            }
            match op {
                TrackCmd::Play => t.play(),
                TrackCmd::Pause => t.pause(),
                TrackCmd::Toggle => {
                    if t.is_active() {
                        t.pause()
                    } else {
                        t.play()
                    }
                }
                TrackCmd::SetGain(g) => t.set_gain(g),
                TrackCmd::FadeTo { target: tg, secs } => t.fade_to(tg, secs, rate),
                TrackCmd::SetSpeed(s) => t.set_speed(s),
                TrackCmd::SetReverse(r) => t.set_reverse(r),
                TrackCmd::LoopAll(on) => t.set_loop_all(on),
                TrackCmd::SeekSecs(s) => t.seek_secs(s),
                TrackCmd::SetCue { cue_in, cue_out } => t.set_cue(cue_in, cue_out),
                TrackCmd::PlayFromCue => t.restart_from_cue(),
            }
        }
    }
}

/// 测试辅助:内存数据构造 AudioData。
#[cfg(test)]
pub(crate) fn make_audio(samples: Vec<f32>, sample_rate: u32, channels: usize) -> std::sync::Arc<AudioData> {
    std::sync::Arc::new(AudioData::new(samples, sample_rate, channels))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TrackCmd;

    fn test_engine(rate: f64, ch: usize) -> (Engine, Sender<Command>, Receiver<Event>) {
        let (cmd_tx, cmd_rx) = unbounded();
        let (evt_tx, evt_rx) = bounded(64);
        (Engine::new(cmd_rx, evt_tx, rate, ch), cmd_tx, evt_rx)
    }

    fn tone(frames: usize) -> std::sync::Arc<AudioData> {
        make_audio(vec![0.5; frames], 1000, 1) // 1000 Hz 引擎,1 声道
    }

    /// 消费挂起命令但不混合任何帧:零长度缓冲。
    fn drain(engine: &mut Engine) {
        engine.process(&mut []);
    }

    fn status_of(
        engine: &mut Engine,
        cmd_tx: &Sender<Command>,
        evt_rx: &Receiver<Event>,
    ) -> Box<EngineStatus> {
        cmd_tx.send(Command::Status).unwrap();
        drain(engine);
        match evt_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap() {
            Event::Status(s) => s,
            other => panic!("期待 Status 事件,得到 {other:?}"),
        }
    }

    #[test]
    fn fade_out_reaches_zero() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(10_000) }).unwrap();
        drain(&mut engine);

        cmd_tx.send(Command::All(TrackCmd::FadeTo { target: 0.0, secs: 0.5 })).unwrap();
        engine.process(&mut vec![0.0; 500]);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(st.tracks[0].gain < 1e-4, "gain 应接近 0,实际 {}", st.tracks[0].gain);
    }

    #[test]
    fn speed_doubles_advance() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(10_000) }).unwrap();
        drain(&mut engine);

        cmd_tx.send(Command::All(TrackCmd::SetSpeed(2.0))).unwrap();
        drain(&mut engine);
        engine.process(&mut vec![0.0; 100]);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(
            (st.tracks[0].position_secs - 0.2).abs() < 1e-6,
            "2 倍速下 100 帧应前进 0.2s,实际 {}",
            st.tracks[0].position_secs
        );
    }

    #[test]
    fn loop_wraps_around() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(2_000) }).unwrap();
        cmd_tx.send(Command::All(TrackCmd::LoopAll(true))).unwrap();
        cmd_tx.send(Command::All(TrackCmd::SeekSecs(1.995))).unwrap();
        drain(&mut engine);

        engine.process(&mut vec![0.0; 20]);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(st.tracks[0].playing, "循环下不应结束");
        assert!(st.tracks[0].position_secs < 0.05, "应回绕到头部附近");
    }

    #[test]
    fn natural_end_fires_event() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(50) }).unwrap();
        drain(&mut engine);
        engine.process(&mut vec![0.0; 100]);

        match evt_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap() {
            Event::TrackEnded { index, name } => {
                assert_eq!(index, 0);
                assert_eq!(name, "t");
            }
            other => panic!("期待 TrackEnded,得到 {other:?}"),
        }
        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(!st.tracks[0].playing);
    }

    #[test]
    fn reverse_plays_backward() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(10_000) }).unwrap();
        cmd_tx.send(Command::All(TrackCmd::SeekSecs(5.0))).unwrap();
        cmd_tx.send(Command::All(TrackCmd::SetReverse(true))).unwrap();
        drain(&mut engine);

        engine.process(&mut vec![0.0; 300]);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(st.tracks[0].reverse);
        assert!((st.tracks[0].position_secs - 4.7).abs() < 1e-6);
    }

    #[test]
    fn multi_track_mixes_into_same_buffer() {
        let (mut engine, cmd_tx, _evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "a".into(), data: tone(1_000) }).unwrap();
        cmd_tx.send(Command::AddTrack { name: "b".into(), data: tone(1_000) }).unwrap();
        drain(&mut engine);

        let mut buf = vec![0.0; 10];
        engine.process(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-4, "混音值应为 1.0,实际 {}", buf[0]);
    }

    #[test]
    fn per_track_addressing_only_hits_target() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "a".into(), data: tone(1_000) }).unwrap();
        cmd_tx.send(Command::AddTrack { name: "b".into(), data: tone(1_000) }).unwrap();
        drain(&mut engine);

        cmd_tx.send(Command::Track { index: 1, cmd: TrackCmd::SetGain(0.2) }).unwrap();
        drain(&mut engine);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!((st.tracks[0].gain - 1.0).abs() < 1e-6);
        assert!((st.tracks[1].gain - 0.2).abs() < 1e-6);
    }

    #[test]
    fn cue_crop_limits_playback_range() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(3_000) }).unwrap();
        // 只播 0.5s ~ 1.0s
        cmd_tx
            .send(Command::All(TrackCmd::SetCue { cue_in: Some(0.5), cue_out: Some(1.0) }))
            .unwrap();
        drain(&mut engine);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!((st.tracks[0].trimmed_secs - 0.5).abs() < 1e-3);
        assert!((st.tracks[0].position_secs - 0.5).abs() < 1e-3, "应从入点开始");

        engine.process(&mut vec![0.0; 2_000]); // 远超 0.5s 有效长度
        match evt_rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap() {
            Event::TrackEnded { .. } => {}
            other => panic!("裁剪区间播完应发 TrackEnded,得到 {other:?}"),
        }
        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(!st.tracks[0].playing);
        assert!((st.tracks[0].position_secs - 1.0).abs() < 1e-3, "停在出点");
    }

    #[test]
    fn cue_loop_wraps_within_cues() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(3_000) }).unwrap();
        cmd_tx
            .send(Command::All(TrackCmd::SetCue { cue_in: Some(0.5), cue_out: Some(1.0) }))
            .unwrap();
        cmd_tx.send(Command::All(TrackCmd::LoopAll(true))).unwrap();
        drain(&mut engine);

        engine.process(&mut vec![0.0; 1_500]); // 1.5s > 0.5s 区间,循环 3 次

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(st.tracks[0].playing, "入出点循环不应结束");
        assert!(st.tracks[0].position_secs >= 0.5 && st.tracks[0].position_secs < 1.0);
    }

    #[test]
    fn play_from_cue_restarts_trimmed_region() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "t".into(), data: tone(3_000) }).unwrap();
        cmd_tx
            .send(Command::All(TrackCmd::SetCue { cue_in: Some(0.5), cue_out: Some(1.0) }))
            .unwrap();
        cmd_tx.send(Command::All(TrackCmd::SeekSecs(2.0))).unwrap();
        drain(&mut engine);
        cmd_tx.send(Command::All(TrackCmd::PlayFromCue)).unwrap();
        drain(&mut engine);

        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!((st.tracks[0].position_secs - 0.5).abs() < 1e-3);
    }

    #[test]
    fn master_gain_scales_output() {
        let (mut engine, cmd_tx, _evt_rx) = test_engine(1000.0, 1);
        cmd_tx.send(Command::AddTrack { name: "a".into(), data: tone(1_000) }).unwrap();
        cmd_tx.send(Command::Master(MasterCmd::SetMasterGain(0.5))).unwrap();
        drain(&mut engine);

        let mut buf = vec![0.0; 10];
        engine.process(&mut buf);
        assert!((buf[0] - 0.25).abs() < 1e-4, "0.5 轨 × 0.5 主 = 0.25,实际 {}", buf[0]);
    }

    #[test]
    fn remove_and_clear_tracks() {
        let (mut engine, cmd_tx, evt_rx) = test_engine(1000.0, 1);
        for n in ["a", "b"] {
            cmd_tx.send(Command::AddTrack { name: n.into(), data: tone(500) }).unwrap();
        }
        drain(&mut engine);
        cmd_tx.send(Command::Master(MasterCmd::RemoveTrack(0))).unwrap();
        drain(&mut engine);
        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert_eq!(st.tracks.len(), 1);
        assert_eq!(st.tracks[0].name, "b");

        cmd_tx.send(Command::Master(MasterCmd::ClearTracks)).unwrap();
        drain(&mut engine);
        let st = status_of(&mut engine, &cmd_tx, &evt_rx);
        assert!(st.tracks.is_empty());
    }

    #[test]
    fn metronome_fires_on_beats() {
        let (mut engine, cmd_tx, _evt_rx) = test_engine(1000.0, 1);
        cmd_tx
            .send(Command::Master(MasterCmd::Metronome { on: true, bpm: 300.0, gain: 1.0 }))
            .unwrap();
        drain(&mut engine);

        // 300 BPM = 每 200 帧一拍
        let mut buf = vec![0.0; 410];
        engine.process(&mut buf);
        assert!(buf[0].abs() > 1e-3, "第 0 帧应有 click");
        assert!(buf[200].abs() > 1e-3, "第 200 帧应有 click");
        assert!(buf[100].abs() < 1e-6, "拍间应静音");
        assert!(buf[400].abs() > 1e-3, "第 400 帧应有 click");
    }
}
