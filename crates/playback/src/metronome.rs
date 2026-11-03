//! 节拍器:预合成 click 缓冲,按帧时钟在拍点混入输出。
//! 回调路径零分配:click 缓冲仅在配置变化时重建。

/// 节拍器状态。click 为单声道,复制到所有输出声道。
pub struct Metronome {
    on: bool,
    bpm: f64,
    gain: f32,
    out_rate: f64,
    /// click 波形(与输出采样率同域)
    click: Vec<f32>,
    /// click 播放位置(None = 空闲)
    click_pos: Option<usize>,
    /// 距下一拍的帧数
    frames_to_beat: u64,
}

impl Metronome {
    pub fn new(out_rate: f64) -> Self {
        let mut m = Self {
            on: false,
            bpm: 120.0,
            gain: 0.6,
            out_rate,
            click: Vec::new(),
            click_pos: None,
            frames_to_beat: 0,
        };
        m.synth_click();
        m
    }

    /// 1kHz 余弦 × 快速衰减,约 30ms(余弦保证任意采样率下 t=0 即峰值)。
    fn synth_click(&mut self) {
        let len = (self.out_rate * 0.03) as usize;
        self.click = (0..len)
            .map(|i| {
                let t = i as f64 / self.out_rate;
                (2.0 * std::f32::consts::PI * 1000.0 * t as f32).cos()
                    * (-30.0 * t as f32).exp()
                    * 0.8
            })
            .collect();
    }

    fn beat_len(&self) -> u64 {
        ((self.out_rate * 60.0 / self.bpm).round() as u64).max(1)
    }

    pub fn configure(&mut self, on: bool, bpm: f64, gain: f32) {
        self.bpm = bpm.clamp(20.0, 300.0);
        self.gain = gain.clamp(0.0, 1.0);
        if on && !self.on {
            self.frames_to_beat = 0; // 开启即对齐拍点
            self.click_pos = None;
        }
        self.on = on;
        if !on {
            self.click_pos = None;
        }
    }

    pub fn is_on(&self) -> bool {
        self.on
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    /// 在输出块上叠加 click。`frames`/`ch` 描述 out 布局。
    pub fn mix(&mut self, out: &mut [f32], frames: usize, ch: usize) {
        if !self.on {
            return;
        }
        let beat_len = self.beat_len();
        for f in 0..frames {
            if self.click_pos.is_none() && self.frames_to_beat == 0 {
                self.click_pos = Some(0);
                self.frames_to_beat = beat_len;
            }
            if let Some(idx) = self.click_pos {
                if idx < self.click.len() {
                    let v = self.click[idx] * self.gain;
                    for c in 0..ch {
                        out[f * ch + c] += v;
                    }
                    self.click_pos = Some(idx + 1);
                } else {
                    self.click_pos = None;
                }
            }
            if self.frames_to_beat > 0 {
                self.frames_to_beat -= 1;
            }
        }
    }
}
