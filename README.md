# Hei Player

舞台音频播控软件(Rust + Tauri 2)。面向现场演出、活动主持场景:
即点即播、多轨混音、控台联动、手机遥控。

**当前进度:v0.1.0** —— 播放引擎 + 全功能桌面客户端(macOS / Windows)。

## 快速开始

```bash
cargo build --release

# 桌面客户端
cargo run -p hei-app

# 命令行试听器(引擎直连)
cargo run -p hei-cli -- play 任意音频.mp3
cargo run -p hei-cli -- tone --hz 440 --secs 5

# 打包 macOS .app + .dmg
scripts/package-mac.sh
```

## 功能总览

| 模块 | 说明 |
|---|---|
| 播放引擎 | 多轨混音、淡入淡出、变速、倒放、循环、seek(实时回调零分配零锁) |
| 裁剪 | 波形上手柄拖拽 / 数值精输入,入出点循环,裁剪段导出 WAV |
| 波形 | 并行峰值计算 + canvas 渲染 + 播放头 |
| 素材库 | 分组、拼音搜索、持久化、解码磁盘缓存(二次播放秒加载) |
| 节目单 | 排序、播完自动接下一条 |
| 定时播放 | 时刻 + 星期,到点自动触发 |
| 文字卡 | 主持稿管理,手机端全屏提词 |
| 录音 | 麦克风录制,自动转 WAV 入库 |
| MTC | MIDI 时间码输出(虚拟端口,灯光控台同步) |
| 节拍器 | 可调 BPM |
| 全局快捷键 | 默认组 + 素材级 / 分组级热键,输入框聚焦自动挂起 |
| 手机端 | 扫码遥控、上传素材、提词(局域网 HTTP,无需服务器) |
| 其他 | 输出设备切换、主音量、迷你窗口、置顶、深浅主题 |

## 架构

```
crates/
├── playback/   音频引擎:cpal 输出 + 多轨混音器(增益斜坡/变速/倒放/入出点/
│               循环/主音量/节拍器/峰值),命令支持按轨道寻址
├── decode/     symphonia 整段解码 → 交错 f32;WAV 写出(导出/录音)
├── app/        桌面客户端(包名 hei-app)(Tauri 2 壳;ui/ 为原生 HTML/JS 无构建链)
└── player-cli/ 命令行试听器(包名 hei-cli)
```

设计要点:
- 音频回调内严格零分配、零锁;命令经 crossbeam 通道非阻塞注入
- 引擎属主线程独占音频流(设备热切换),事件多订阅者广播
- 全部互斥锁使用 parking_lot(用户态 futex)
- 所有 Tauri 命令 async 化,主线程不执行业务逻辑

## CI

每次 push 自动构建([Actions](.github/workflows/build.yml)):
macOS arm64(.app/.dmg)+ Windows x64(NSIS 安装包)。

## Roadmap

- [x] v0.1.0 播放引擎 + 全功能客户端
- [ ] 高质量重采样(rubato)、MTC quarter-frame 流、大文件分页
- [ ] 系统音频录制、特效链、自动更新
