//! hei-cli —— P0 命令行试听器。
//!
//! 用法:
//!   hei-cli play <FILE>...        解码并混音播放,进入交互控制
//!   hei-cli tone [--hz 440] [--secs 3] [--vol 0.4]
//!
//! 交互命令(回车逐条输入):
//!   p 播放   空格=暂停/继续(t)   fadeout 淡出   fadein 淡入并继续
//!   vol 0.5 音量   speed 1.5 变速   reverse 倒放   loop 循环开关
//!   seek 30 跳转   s 状态   q 退出

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use playback::{AudioData, Command, EngineHandle, Event, TrackCmd, TrackStatus};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("play") if args.len() >= 2 => run(decode_files(&args[1..])?),
        Some("tone") => run(vec![tone(track_tone_args(&args[1..])?)]),
        _ => {
            eprintln!("用法:");
            eprintln!("  hei-cli play <音频文件>...          播放文件(多文件混音)");
            eprintln!("  hei-cli tone [--hz 440] [--secs 3] [--vol 0.4]");
            std::process::exit(2);
        }
    }
}

fn track_tone_args(args: &[String]) -> Result<(f64, f64, f32)> {
    let (mut hz, mut secs, mut vol) = (440.0, 3.0, 0.4);
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let mut val = || it.next().context("缺少参数值").map(|s| s.to_owned());
        match flag.as_str() {
            "--hz" => hz = val()?.parse()?,
            "--secs" => secs = val()?.parse()?,
            "--vol" => vol = val()?.parse()?,
            other => bail!("tone: 未知参数 {other}"),
        }
    }
    Ok((hz, secs, vol))
}

/// 合成带 5ms 起落包络的双声道正弦音(立体声左右差半音,便于听声道)。
fn tone((hz, secs, vol): (f64, f64, f32)) -> (String, Arc<AudioData>) {
    const RATE: u32 = 44_100;
    let frames = (secs * RATE as f64) as usize;
    let attack = (0.005 * RATE as f64) as usize;
    let mut samples = Vec::with_capacity(frames * 2);
    let hz_r = hz * 2f64.powf(1.0 / 12.0); // 右声道高半音
    for i in 0..frames {
        let env = (i.min(attack).min(frames - 1 - i.min(frames - 1)) as f64 / attack as f64).min(1.0);
        let t = i as f64 / RATE as f64;
        samples.push((vol * env as f32) * (2.0 * std::f32::consts::PI * (hz * t) as f32).sin());
        samples.push((vol * env as f32) * (2.0 * std::f32::consts::PI * (hz_r * t) as f32).sin());
    }
    ("tone".into(), Arc::new(AudioData::new(samples, RATE, 2)))
}

fn decode_files(paths: &[String]) -> Result<Vec<(String, Arc<AudioData>)>> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let path = std::path::Path::new(p);
        let audio = decode::load(path).with_context(|| format!("解码 {p}"))?;
        println!("已解码 {p}:{}ch {:.1}s {:.2}MB",
            audio.channels,
            audio.duration_secs(),
            audio.samples.len() as f32 * 4.0 / 1024.0 / 1024.0);
        let name = path.file_stem().and_then(|n| n.to_str()).unwrap_or("track").to_owned();
        out.push((name, audio));
    }
    Ok(out)
}

fn run(tracks: Vec<(String, Arc<AudioData>)>) -> Result<()> {
    let handle = EngineHandle::start_default().context("启动音频引擎失败")?;
    println!("输出设备就绪:{}, {}ch。输入 s 查看状态,q 退出。", handle.sample_rate, handle.channels);

    for (name, data) in tracks {
        handle.cmd_tx.send(Command::AddTrack { name, data })?;
    }

    let running = Arc::new(AtomicBool::new(true));

    // 事件打印线程
    {
        let evt_rx = handle.evt_rx.clone();
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            for evt in evt_rx.iter() {
                match evt {
                    Event::Status(st) => print_status(&st.tracks),
                    Event::TrackEnded { index, name } => {
                        println!("[{index}] {name} 播完");
                    }
                }
                if !running.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
    }

    // 每秒自动刷一次状态
    {
        let cmd_tx = handle.cmd_tx.clone();
        let running = Arc::clone(&running);
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let _ = cmd_tx.send(Command::Status);
            }
        });
    }

    // 主线程:交互命令
    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut reverse = false;
    let mut looping = false;
    loop {
        line.clear();
        print!("> ");
        std::io::stdout().flush().ok();
        if stdin.lock().read_line(&mut line)? == 0 {
            break; // EOF / Ctrl-D
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        let (cmd, rest) = input.split_once(' ').unwrap_or((input, ""));
        match cmd {
            "p" | "play" => handle.cmd_tx.send(Command::All(TrackCmd::Play))?,
            "t" | "toggle" | "pause" => handle.cmd_tx.send(Command::All(TrackCmd::Toggle))?,
            "fadeout" => handle.cmd_tx.send(Command::All(TrackCmd::FadeTo { target: 0.0, secs: 2.0 }))?,
            "fadein" => {
                handle.cmd_tx.send(Command::All(TrackCmd::Play))?;
                handle.cmd_tx.send(Command::All(TrackCmd::FadeTo { target: 1.0, secs: 2.0 }))?;
            }
            "vol" => match rest.parse::<f32>() {
                Ok(v) => handle.cmd_tx.send(Command::All(TrackCmd::SetGain(v)))?,
                Err(_) => eprintln!("用法: vol 0.5"),
            },
            "speed" => match rest.parse::<f32>() {
                Ok(v) => handle.cmd_tx.send(Command::All(TrackCmd::SetSpeed(v)))?,
                Err(_) => eprintln!("用法: speed 1.5"),
            },
            "reverse" => {
                reverse = !reverse;
                handle.cmd_tx.send(Command::All(TrackCmd::SetReverse(reverse)))?;
            }
            "loop" => {
                looping = !looping;
                handle.cmd_tx.send(Command::All(TrackCmd::LoopAll(looping)))?;
            }
            "seek" => match rest.parse::<f64>() {
                Ok(v) => handle.cmd_tx.send(Command::All(TrackCmd::SeekSecs(v)))?,
                Err(_) => eprintln!("用法: seek 30"),
            },
            "s" | "status" => handle.cmd_tx.send(Command::Status)?,
            "q" | "quit" | "exit" => break,
            other => eprintln!("未知命令 {other}。可用:p t fadeout fadein vol speed reverse loop seek s q"),
        }
    }

    running.store(false, Ordering::Relaxed);
    Ok(()) // handle drop → stream drop → 停声
}

fn print_status(list: &[TrackStatus]) {
    for t in list {
        println!(
            "[{}] {} {} {:02}:{:05.1}/{:.1}s vol{:.2} spd{:.2}{}{}",
            t.index,
            t.name,
            if t.playing { "▶" } else { "■" },
            t.position_secs as u64 / 60,
            t.position_secs % 60.0,
            t.duration_secs,
            t.gain,
            t.speed,
            if t.reverse { " rev" } else { "" },
            if t.looping { " loop" } else { "" },
        );
    }
}
