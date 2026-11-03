//! 单轨状态与逐帧混合数学。回调路径上不允许分配/加锁,全部字段原地更新。

use std::sync::Arc;

use crate::types::AudioData;

/// 音量上限(允许轻微过 1.0 做提升)。
pub const MAX_GAIN: f32 = 2.0;
pub const MIN_SPEED: f32 = 0.25;
pub const MAX_SPEED: f32 = 4.0;

/// 一条参与混音的轨道。
///
/// 播放头用 f64 帧索引:小数部分供线性插值重采样,
/// 一步长同时吸收"源/输出采样率换算"和"变速"两个因素。
/// 入出点(cue)定义有效播放区间:playhead 永远落在 [in, out) 内。
pub struct Track {
    pub name: String,
    pub data: Arc<AudioData>,
    /// 播放头(源文件帧,含小数;始终在有效区间内)
    playhead: f64,
    playing: bool,
    ended: bool,
    gain: f32,
    target_gain: f32,
    /// 每输出帧的增益步进(正或负,0 = 已到位)
    gain_step: f32,
    speed: f32,
    reverse: bool,
    loop_all: bool,
    /// 入点(帧);默认 0
    cue_in: f64,
    /// 出点(帧);默认 = total
    cue_out: f64,
}

impl Track {
    pub fn new(name: impl Into<String>, data: Arc<AudioData>) -> Self {
        let total = data.total_frames() as f64;
        Self {
            name: name.into(),
            data,
            playhead: 0.0,
            playing: true,
            ended: false,
            gain: 1.0,
            target_gain: 1.0,
            gain_step: 0.0,
            speed: 1.0,
            reverse: false,
            loop_all: false,
            cue_in: 0.0,
            cue_out: total,
        }
    }

    pub fn is_active(&self) -> bool {
        self.playing && !self.ended
    }

    pub fn position_secs(&self) -> f64 {
        self.playhead / self.data.sample_rate as f64
    }

    pub fn duration_secs(&self) -> f64 {
        self.data.duration_secs()
    }

    /// 有效(裁剪后)时长,秒。
    pub fn trimmed_secs(&self) -> f64 {
        (self.cue_out - self.cue_in).max(0.0) / self.data.sample_rate as f64
    }

    pub fn gain(&self) -> f32 {
        self.gain
    }

    pub fn speed(&self) -> f32 {
        self.speed
    }

    pub fn reverse(&self) -> bool {
        self.reverse
    }

    pub fn looping(&self) -> bool {
        self.loop_all
    }

    pub fn ended(&self) -> bool {
        self.ended
    }

    pub fn cue_in_secs(&self) -> Option<f64> {
        (self.cue_in > 0.0).then(|| self.cue_in / self.data.sample_rate as f64)
    }

    pub fn cue_out_secs(&self) -> Option<f64> {
        let total = self.data.total_frames() as f64;
        (self.cue_out < total - 0.5)
            .then(|| self.cue_out / self.data.sample_rate as f64)
    }

    // ---- 命令实现(在回调开头、处理音频块之前调用) ----

    pub fn play(&mut self) {
        if self.ended {
            self.restart_from_cue();
        }
        self.playing = true;
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.gain_step = 0.0;
    }

    pub fn set_gain(&mut self, g: f32) {
        self.gain = g.clamp(0.0, MAX_GAIN);
        self.target_gain = self.gain;
        self.gain_step = 0.0;
    }

    pub fn fade_to(&mut self, target: f32, secs: f64, out_rate: f64) {
        let target = target.clamp(0.0, MAX_GAIN);
        self.target_gain = target;
        let frames = (secs.max(0.0) * out_rate).max(1.0);
        self.gain_step = (target - self.gain) / frames as f32;
    }

    pub fn set_speed(&mut self, s: f32) {
        self.speed = s.clamp(MIN_SPEED, MAX_SPEED);
    }

    pub fn set_reverse(&mut self, rev: bool) {
        if self.reverse != rev {
            self.reverse = rev;
            // 倒放从当前位置往回走;若已在入点则先跳到出点,避免立即结束
            if rev && self.playhead <= self.cue_in {
                self.playhead = self.cue_out;
            }
        }
    }

    pub fn set_loop_all(&mut self, on: bool) {
        self.loop_all = on;
    }

    /// 设置入出点(秒,相对原始时间轴);None 表示恢复默认(0 / 结尾)。
    /// 出点自动夹到 (入点, total]。
    pub fn set_cue(&mut self, cue_in: Option<f64>, cue_out: Option<f64>) {
        let sr = self.data.sample_rate as f64;
        let total = self.data.total_frames() as f64;
        if let Some(i) = cue_in {
            self.cue_in = (i * sr).clamp(0.0, total - 1.0);
        } else {
            self.cue_in = 0.0;
        }
        if let Some(o) = cue_out {
            self.cue_out = (o * sr).clamp(self.cue_in + 1.0, total);
        } else {
            self.cue_out = total;
        }
        // 播放头落回有效区间
        if self.playhead < self.cue_in || self.playhead > self.cue_out {
            self.playhead = if self.reverse { self.cue_out } else { self.cue_in };
        }
    }

    pub fn seek_secs(&mut self, secs: f64) {
        let sr = self.data.sample_rate as f64;
        self.playhead = (secs * sr).clamp(self.cue_in, self.cue_out - 1.0).max(0.0);
        self.ended = false;
    }

    /// 从入点重新播放(裁剪语义:"播放这一段")。
    pub fn restart_from_cue(&mut self) {
        self.playhead = if self.reverse { self.cue_out } else { self.cue_in };
        self.ended = false;
        self.playing = true;
    }

    // ---- 逐帧混合 ----

    /// 每输出帧的源帧步进(正 = 正向,负 = 倒放)。
    pub fn step_per_output_frame(&self, out_rate: f64) -> f64 {
        self.data.sample_rate as f64 / out_rate * self.speed as f64
            * if self.reverse { -1.0 } else { 1.0 }
    }

    /// 读第 `frame` 帧、映射到输出声道 `out_ch` 的采样(含声道上下变换)。
    #[inline]
    fn sample_at(&self, frame: usize, out_ch: usize) -> f32 {
        let ch = self.data.channels;
        let src_ch = out_ch % ch.max(1); // 单声道复制到所有输出声道;多声道取模轮询
        self.data.samples[frame * ch + src_ch]
    }

    /// 混合一个输出帧:线性插值 + 增益。返回 false 表示本轨已播完。
    #[inline]
    pub fn mix_frame(&mut self, out: &mut [f32]) -> bool {
        // 增益斜坡(到达即停)
        if self.gain_step != 0.0 {
            self.gain += self.gain_step;
            let reached = if self.gain_step > 0.0 {
                self.gain >= self.target_gain
            } else {
                self.gain <= self.target_gain
            };
            if reached {
                self.gain = self.target_gain;
                self.gain_step = 0.0;
            }
        }

        let total = self.data.total_frames();
        if total == 0 {
            self.ended = true;
            return false;
        }

        let pos = self.playhead;
        let i0 = pos.floor();
        let frac = (pos - i0) as f32;
        let i0 = i0 as usize;
        // 倒放时插值邻居取前一帧,保持连续性
        let i1 = if self.reverse { i0.saturating_sub(1) } else { (i0 + 1).min(total - 1) };
        let i0 = i0.min(total - 1);

        let g = self.gain;
        for (c, o) in out.iter_mut().enumerate() {
            let a = self.sample_at(i0, c);
            let b = self.sample_at(i1, c);
            *o += (a + (b - a) * frac) * g;
        }
        true
    }

    /// 推进播放头,处理循环与自然结束。返回 false 表示本轨已播完。
    #[inline]
    pub fn advance(&mut self, step: f64) -> bool {
        self.playhead += step;

        let (lo, hi) = (self.cue_in, self.cue_out);
        if self.loop_all {
            if step >= 0.0 && self.playhead >= hi {
                self.playhead = lo + (self.playhead - hi);
            } else if step < 0.0 && self.playhead <= lo {
                self.playhead = hi - (lo - self.playhead);
            }
            return true;
        }

        if self.playhead >= hi || self.playhead <= lo {
            self.ended = true;
            self.playing = false;
            // 停在边界,Status 显示才不越界
            self.playhead = if self.reverse { lo } else { hi }.clamp(0.0, self.data.total_frames() as f64);
            return false;
        }
        true
    }
}
