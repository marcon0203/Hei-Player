//! 手机端 HTTP 服务:遥控、扫码传文件、司仪宝(主持稿)。
//! axum + 端口绑定 0.0.0.0;桌面端显示二维码(局域网 IP)。
//! 命令协议与桌面端一致(parse_op 字符串),master/metronome 单独。

use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Multipart, Query, State};
use axum::response::Html;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::owner::send;
use crate::state::App;
use playback::{Command, MasterCmd};

/// 局域网 IP(UDP connect 技巧,不真正发包)。
pub fn lan_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// 二维码 SVG 字符串。
pub fn qr_svg(text: &str) -> String {
    use qrcode::{Color, QrCode};
    let Ok(code) = QrCode::new(text.as_bytes()) else {
        return String::new();
    };
    let w = code.width();
    let n = w as f64;
    let colors = code.to_colors();
    let mut paths = String::new();
    for y in 0..w {
        for x in 0..w {
            if colors[y * w + x] == Color::Dark {
                paths.push_str(&format!("M{x},{y}h1v1h-1z"));
            }
        }
    }
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {n} {n}\" shape-rendering=\"crispEdges\"><rect width=\"100%\" height=\"100%\" fill=\"#fff\"/><path d=\"{paths}\" fill=\"#000\"/></svg>"
    )
}

pub fn spawn_server(app: std::sync::Arc<App>, port: u16) {
    let router = Router::new()
        .route("/", get(page_control))
        .route("/script", get(page_script))
        .route("/api/status", get(api_status))
        .route("/api/cmd", post(api_cmd))
        .route("/api/upload", post(api_upload))
        .route("/api/library", get(api_library))
        .with_state(app);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    std::thread::spawn(move || {
        rt.block_on(async move {
            let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("[remote] 端口 {port} 绑定失败:{e}(手机功能不可用)");
                    return;
                }
            };
            eprintln!("[remote] 手机端就绪: http://0.0.0.0:{port}");
            let _ = axum::serve(listener, router).await;
        });
    });
}

// ---- API ----

async fn api_status(State(app): State<std::sync::Arc<App>>) -> Json<serde_json::Value> {
    let rx = app.hub.subscribe();
    send(&app.engines, Command::Status);
    match rx.recv_timeout(Duration::from_millis(800)) {
        Ok(playback::Event::Status(s)) => {
            let core = app.core.lock();
            Json(json!({
                "engine": *s,
                "sessions": core.sessions,
                "rundown": core.data.rundown,
                "rundown_enabled": core.data.rundown_enabled,
                "active_script": core.data.active_script,
            }))
        }
        _ => Json(json!({ "error": "engine timeout" })),
    }
}

async fn api_library(State(app): State<std::sync::Arc<App>>) -> Json<serde_json::Value> {
    let core = app.core.lock();
    Json(json!({ "library": core.data.library, "groups": core.data.groups }))
}

#[derive(Deserialize)]
struct CmdBody {
    /// all | track | master | metronome
    kind: String,
    /// play/pause/toggle/gain/fade/speed/reverse/loop/seek/playcue
    #[serde(default)]
    op: String,
    #[serde(default)]
    index: usize,
    #[serde(default)]
    a: f64,
    #[serde(default)]
    b: f64,
}

async fn api_cmd(State(app): State<std::sync::Arc<App>>, Json(body): Json<CmdBody>) -> Json<serde_json::Value> {
    let cmd = match body.kind.as_str() {
        "all" => crate::parse_op(&body.op, body.a, body.b).map(Command::All),
        "track" => {
            crate::parse_op(&body.op, body.a, body.b)
                .map(|c| Command::Track { index: body.index, cmd: c })
        }
        "master" => Some(Command::Master(MasterCmd::SetMasterGain(body.a as f32))),
        "metronome" => Some(Command::Master(MasterCmd::Metronome {
            on: body.a > 0.5,
            bpm: body.b,
            gain: 0.6,
        })),
        _ => None,
    };
    if let Some(cmd) = cmd {
        send(&app.engines, cmd);
        Json(json!({ "ok": true }))
    } else {
        Json(json!({ "ok": false, "error": "bad kind/op" }))
    }
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    group: Option<String>,
}

async fn api_upload(
    State(app): State<std::sync::Arc<App>>,
    Query(q): Query<UploadQuery>,
    mut multipart: Multipart,
) -> Json<serde_json::Value> {
    let mut saved: Vec<String> = Vec::new();
    let uploads_dir = app.data_dir.join("uploads");
    let _ = std::fs::create_dir_all(&uploads_dir);

    while let Ok(Some(field)) = multipart.next_field().await {
        let Some(filename) = field.file_name().map(|s| s.to_owned()) else {
            continue;
        };
        let Ok(bytes) = field.bytes().await else { continue };
        if bytes.is_empty() || bytes.len() > 200 * 1024 * 1024 {
            continue;
        }
        let safe: String = filename
            .chars()
            .map(|c| if c.is_alphanumeric() || "._- ".contains(c) { c } else { '_' })
            .collect();
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path: PathBuf = uploads_dir.join(format!("{ts}_{safe}"));
        if std::fs::write(&path, &bytes).is_ok() {
            saved.push(path.to_string_lossy().into_owned());
        }
    }

    let count = saved.len();
    if count > 0 {
        app.add_files_blocking(saved, q.group.unwrap_or_else(|| "默认分组".into()));
    }
    Json(json!({ "ok": true, "count": count }))
}

// ---- 页面 ----

async fn page_control(State(_app): State<std::sync::Arc<App>>) -> Html<String> {
    Html(CONTROL_PAGE.into())
}

async fn page_script(State(app): State<std::sync::Arc<App>>) -> Html<String> {
    let (title, text) = {
        let core = app.core.lock();
        match core.data.active_script.and_then(|id| core.data.track(id)) {
            Some(t) => (t.name.clone(), t.text.clone()),
            None => ("未选择".into(), "请在桌面端选择要使用的主持稿(文字卡片)".into()),
        }
    };
    Html(script_page(&title, &text))
}

fn script_page(title: &str, text: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title><style>
body{{background:#fff;color:#111;font:17px/1.9 -apple-system,"PingFang SC",sans-serif;padding:24px;max-width:720px;margin:0 auto}}
h1{{font-size:20px;border-bottom:2px solid #22d3ee;padding-bottom:8px}}
.now{{color:#1fb6d4;font-size:14px;text-align:center;font-weight:600}}
.t{{color:#999;font-size:12px;text-align:center;margin-top:40px}}
</style></head><body><h1>{title}</h1><div style="white-space:pre-wrap">{text}</div>
<p class="now" id="now"></p>
<p class="t">Hei Player 司仪宝 · 桌面端切换文字卡后自动刷新</p>
</body></html>"#,
        title = esc(title),
        text = esc(text),
    ) + NOW_PLAYING_SCRIPT
}

/// 司仪宝页"正在播放"条(独立常量,避免 format! 转义地狱)。
const NOW_PLAYING_SCRIPT: &str = r#"
<script>
setInterval(()=>location.reload(),15000);
async function now(){
  try{
    const s = await (await fetch('/api/status')).json();
    const t = (s.engine && s.engine.tracks || []).find(x=>x.playing);
    document.getElementById('now').textContent = t
      ? '♪ 正在播放:' + t.name + ' ' + t.position_secs.toFixed(0) + 's' : '';
  }catch(e){}
}
now(); setInterval(now,2000);
</script>"#;

const CONTROL_PAGE: &str = r##"<!DOCTYPE html><html lang="zh-CN"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Hei Player 遥控</title><style>
body{background:#0e1116;color:#e8edf4;font:15px -apple-system,"PingFang SC",sans-serif;margin:0;padding:14px;padding-bottom:44px}
h1{font-size:17px;margin:8px 0}
.row{display:flex;gap:8px;margin:8px 0}
button{flex:1;background:#1c2330;color:#e8edf4;border:1px solid #2a3444;border-radius:10px;padding:13px 6px;font-size:15px}
button.play{background:linear-gradient(135deg,#22d3ee,#7c5cff);color:#0b0e13;font-weight:700;border:0}
.t{font:12px ui-monospace,monospace;color:#8a94a6}
.card{background:#161b23;border:1px solid #2a3444;border-radius:12px;padding:12px;margin:10px 0}
.bar{height:5px;background:#1c2330;border-radius:99px;overflow:hidden}
.fill{height:100%;background:linear-gradient(90deg,#22d3ee,#7c5cff);width:0}
a.up{display:block;text-align:center;padding:14px;border:1px dashed #2a3444;border-radius:12px;margin:12px 0;color:#22d3ee;text-decoration:none}
</style></head><body>
<h1>🎛 Hei Player 遥控</h1>
<div class="row"><button class="play" onclick="post('all','play')">▶ 全部播放</button><button onclick="post('all','pause')">⏸ 全部暂停</button></div>
<div class="row"><button onclick="post('all','fade',0,2)">全部淡出</button><button onclick="post('all','fadein')">全部淡入</button></div>
<div class="t">主音量 <input type="range" min="0" max="2" step="0.05" value="1" oninput="post('master',null,this.value)"></div>
<div id="tracks"></div>
<a class="up" href="#" onclick="event.preventDefault();document.getElementById('f').click()">📤 上传音频到电脑</a>
<input type="file" id="f" accept="audio/*" multiple hidden>
<p class="t">司仪宝:<a href="/script" style="color:#22d3ee">打开主持稿</a></p>
<script>
async function post(kind, op, a=0, b=0, index=0){
  const body = op==='fadein'
    ? {kind:'all',op:'fade',a:1,b:2}
    : {kind,op:op??'',a:+a||0,b:+b||0,index};
  await fetch('/api/cmd',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)}).catch(()=>{});
}
document.getElementById('f').onchange = async (e)=>{
  const fd=new FormData();
  for(const f of e.target.files) fd.append('files',f);
  await fetch('/api/upload',{method:'POST',body:fd});
  e.target.value='';
};
async function tick(){
  try{
    const s = await (await fetch('/api/status')).json();
    if(s.engine){
      document.getElementById('tracks').innerHTML = (s.engine.tracks||[]).map(t=>`
        <div class="card">
          <div style="display:flex;justify-content:space-between"><b>${t.name}</b><span class="t">${t.position_secs.toFixed(1)}/${t.duration_secs.toFixed(1)}s</span></div>
          <div class="bar"><div class="fill" style="width:${t.duration_secs>0?(t.position_secs/t.duration_secs*100):0}%"></div></div>
          <div class="row">
            <button class="play" onclick="post('track','toggle',0,0,${t.index})">▶/⏸</button>
            <button onclick="post('track','fade',0,2,${t.index})">淡出</button>
            <button onclick="post('track','playcue',0,0,${t.index})">重播</button>
          </div>
          <div class="t">音量</div>
          <input type="range" min="0" max="2" step="0.05" value="${t.gain}" oninput="post('track','gain',this.value,0,${t.index})">
        </div>`).join('') || '<p class="t">暂无播放轨:去电脑端或上传素材</p>';
    }
  }catch(e){}
  setTimeout(tick,900);
}
tick();
</script></body></html>"##;
