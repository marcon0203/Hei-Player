//! MTC(MIDI Time Code)输出:midir 虚拟端口,灯光控台可直接连接。
//!
//! P1 策略:播放时以 100ms 间隔发送 MTC "Full Frame" 广播消息
//! (0xF0 7F 7F 01 01 hh mm ss ff 0xF7)。多数设备接受周期性全帧更新;
//! 后续可升级为 quarter-frame 连续流(96 msg/s,由音频帧时钟驱动)。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Receiver;
use midir::{MidiOutput, MidiOutputConnection};

#[cfg(unix)]
use midir::os::unix::VirtualOutput;
use playback::EngineStatus;
use playback::Event;

/// 启动 MTC 发送线程。返回退出开关(置 true 即停)。
pub fn spawn_mtc(
    enabled: Arc<AtomicBool>,
    fps: u32,
    evt_rx: Receiver<Event>,
) -> anyhow::Result<Arc<AtomicBool>> {
    let out = MidiOutput::new("HeiPlayer")?;
    let conn: MidiOutputConnection = {
        #[cfg(unix)]
        {
            // macOS/Linux:虚拟端口,控台直接连接
            out.create_virtual("HeiPlayer MTC")?
        }
        #[cfg(windows)]
        {
            // Windows 无虚拟端口机制:连第一个物理 MIDI 输出
            let port = out
                .ports()
                .into_iter()
                .next()
                .ok_or_else(|| anyhow::anyhow!("Windows 上未找到 MIDI 输出端口"))?;
            out.connect(&port, "HeiPlayer MTC")?
        }
    };
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);

    std::thread::spawn(move || {
        let mut conn = conn;
        let mut last_status: Option<EngineStatus> = None;
        let mut mtc_secs: f64 = 0.0;

        while !stop_thread.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(50));

            // 消费事件,保留最新状态(丢旧保新)
            while let Ok(evt) = evt_rx.try_recv() {
                if let Event::Status(s) = evt {
                    last_status = Some(*s);
                }
            }

            if !enabled.load(Ordering::Relaxed) {
                continue;
            }

            let Some(st) = last_status.as_ref() else { continue };

            // 时间码驱动:第一条正在播放的轨;无播放则冻结在最后值
            if let Some(t) = st.tracks.iter().find(|t| t.playing) {
                mtc_secs = t.position_secs;
            }
            send_full_frame(&mut conn, mtc_secs, fps);
        }
    });

    Ok(stop)
}

/// MTC Full Frame:0xF0 7F 7F 01 01 HH MM SS FF 0xF7(hh/mm/ss/ff 为 BCD)。
fn send_full_frame(conn: &mut MidiOutputConnection, secs: f64, fps: u32) {
    let total = secs.max(0.0);
    let hours = (total / 3600.0) as u8 % 24;
    let minutes = ((total / 60.0) as u32 % 60) as u8;
    let seconds = ((total) as u32 % 60) as u8;
    let frames = ((total.fract()) * fps as f64) as u8 % fps as u8;

    let bcd = |v: u8| (v / 10) << 4 | (v % 10);
    let fps_type: u8 = match fps {
        24 => 0,
        25 => 1,
        _ => 2, // 30 drop=3,30 non-drop=2
    };
    let hh = bcd(hours) | (fps_type << 5);
    let msg = [
        0xF0, 0x7F, 0x7F, 0x01, 0x01, hh, bcd(minutes), bcd(seconds), bcd(frames), 0xF7,
    ];
    let _ = conn.send(&msg);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_encoding_sanity() {
        let bcd = |v: u8| (v / 10) << 4 | (v % 10);
        assert_eq!(bcd(0), 0x00);
        assert_eq!(bcd(7), 0x07);
        assert_eq!(bcd(59), 0x59);
        assert_eq!(bcd(23), 0x23);
    }
}
