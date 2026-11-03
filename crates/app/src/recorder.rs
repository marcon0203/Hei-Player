//! 麦克风录音:cpal 输入流(与输出同理,Stream 非 Send——
//! 输入流在录音线程内创建、持有、销毁,start() 只回传通道)。

use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;

use crossbeam_channel::bounded;
use playback::AudioData;

/// 一次录音会话的停止手柄。
pub struct Recording {
    stop_tx: crossbeam_channel::Sender<()>,
    done: Mutex<Option<crossbeam_channel::Receiver<anyhow::Result<PathBuf>>>>,
}

impl Recording {
    /// 停止并等待写盘完成,返回 WAV 路径。
    pub fn stop(&self) -> anyhow::Result<PathBuf> {
        let _ = self.stop_tx.send(());
        let rx = self.done.lock().take().expect("stop() 只能调用一次");
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(path)) => Ok(path),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("录音写盘超时"),
        }
    }
}

/// 开始录音;输入流在录音线程内构建。start() 阻塞到流启动成功/失败。
pub fn start(out_path: PathBuf) -> anyhow::Result<Arc<Recording>> {
    let (stop_tx, stop_rx) = bounded(1);
    let (done_tx, done_rx) = bounded(1);
    let (ready_tx, ready_rx) = bounded::<anyhow::Result<()>>(1);

    let recording = Arc::new(Recording {
        stop_tx: stop_tx.clone(),
        done: Mutex::new(Some(done_rx)),
    });

    std::thread::spawn(move || {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
        type Started = (cpal::Stream, Arc<Mutex<Vec<f32>>>, u32, usize);
        let result = (|| -> anyhow::Result<Started> {
            let host = cpal::default_host();
            let device = host
                .default_input_device()
                .ok_or_else(|| anyhow::anyhow!("找不到默认输入设备(麦克风)"))?;
            let supported = device
                .default_input_config()
                .map_err(|e| anyhow::anyhow!("读取麦克风配置失败:{e}"))?;
            if supported.sample_format() != cpal::SampleFormat::F32 {
                anyhow::bail!("麦克风采样格式 {:?} 暂不支持", supported.sample_format());
            }
            let cfg: cpal::StreamConfig = supported.into();
            let sample_rate = cfg.sample_rate.0;
            let channels = (cfg.channels as usize).max(1);

            let buffer: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
            let cb = Arc::clone(&buffer);
            let err_cb = |err| eprintln!("[mic] 流错误: {err}");
            let stream = device.build_input_stream(
                &cfg,
                move |data: &[f32], _: &_| {
                    cb.lock().extend_from_slice(data);
                },
                err_cb,
                None,
            )?;
            stream.play()?;
            Ok((stream, buffer, sample_rate, channels))
        })();

        match result {
            Ok((stream, buffer, sample_rate, channels)) => {
                let _ = ready_tx.send(Ok(()));
                // 录音线程持有流;停止信号 → drop 流 → 收尾写盘
                let mut collected: Vec<f32> = Vec::new();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    collected.append(&mut buffer.lock());
                }
                drop(stream);
                collected.append(&mut buffer.lock());

                let frames = collected.len() / channels;
                let done = if frames == 0 {
                    Err(anyhow::anyhow!("没有录到任何数据"))
                } else {
                    let audio = AudioData::new(collected, sample_rate, channels);
                    decode::write_wav(&out_path, &audio)
                        .map(|_| out_path.clone())
                        .map_err(|e| anyhow::anyhow!("写 WAV 失败:{e:#}"))
                };
                let _ = done_tx.send(done);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });

    // 等录音流真正启动(或失败)
    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Ok(recording),
        Ok(Err(e)) => Err(e),
        Err(_) => anyhow::bail!("录音启动超时"),
    }
}
