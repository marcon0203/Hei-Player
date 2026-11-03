//! 引擎属主线程 + 事件广播。
//!
//! cpal 的 Stream 非 Send/非 Sync:只能在创建线程存活。
//! 属主线程独占 EngineHandle,跨线程只暴露 `EngineChannels`(命令发送端)。
//! 设备切换 = 控制消息 → 属主线程 drop 旧句柄(停声)→ 按新设备重建。

use std::sync::Arc;
use parking_lot::Mutex;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use playback::{Command, EngineChannels, EngineHandle, Event};

/// 事件广播:多订阅者(桌面 UI / 手机端 / MTC),try_send 不阻塞发布者。
#[derive(Clone, Default)]
pub struct EventHub {
    subs: Arc<Mutex<Vec<Sender<Event>>>>,
}

impl EventHub {
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = bounded(256);
        self.subs.lock().push(tx);
        rx
    }

    pub fn publish(&self, evt: &Event) {
        let mut subs = self.subs.lock();
        // try_send:订阅者满了就丢事件(状态类事件丢旧保新无害),
        // 绝不允许在持有订阅表锁时阻塞等待——那会造成全局死锁。
        subs.retain(|tx| tx.try_send(evt.clone()).is_ok());
    }
}

/// 属主控制命令。
pub enum OwnerCmd {
    /// 切换输出设备(None = 系统默认)
    SwitchDevice(Option<String>),
}

/// 跨线程共享的引擎槽位(命令发送端)。
pub type EngineSlot = Arc<Mutex<Option<EngineChannels>>>;

/// 启动属主线程。返回(命令通道, 引擎槽位)。
pub fn spawn_owner(initial_device: Option<String>, hub: EventHub) -> (Sender<OwnerCmd>, EngineSlot) {
    let (ctrl_tx, ctrl_rx) = bounded::<OwnerCmd>(4);
    let slot: EngineSlot = Arc::new(Mutex::new(None));
    let thread_slot = Arc::clone(&slot);

    std::thread::spawn(move || {
        let mut device = initial_device;
        loop {
            let handle = match EngineHandle::start(device.as_deref()) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("[engine] 启动失败:{e:#},2 秒后重试");
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };
            eprintln!(
                "[engine] 输出就绪:{} Hz / {}ch{}",
                handle.sample_rate,
                handle.channels,
                device.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()
            );
            *thread_slot.lock() = Some(handle.channels());

            // 事件泵 + 控制等待;SwitchDevice 跳出内层循环,drop handle 停声后重建
            loop {
                crossbeam_channel::select! {
                    recv(handle.evt_rx) -> msg => match msg {
                        Ok(evt) => hub.publish(&evt),
                        Err(_) => break, // 引擎侧通道关闭,重建
                    },
                    recv(ctrl_rx) -> cmd => match cmd {
                        Ok(OwnerCmd::SwitchDevice(d)) => {
                            device = d;
                            break;
                        }
                        Err(_) => return, // 属主退出
                    },
                }
            }
        }
    });

    (ctrl_tx, slot)
}

/// 便捷:向当前引擎发命令(无引擎时丢弃)。
pub fn send(slot: &EngineSlot, cmd: Command) {
    if let Some(ch) = slot.lock().as_ref() {
        let _ = ch.cmd_tx.send(cmd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_fanout_and_cleanup() {
        let hub = EventHub::default();
        let rx1 = hub.subscribe();
        let rx2 = hub.subscribe();
        let evt = Event::TrackEnded { index: 0, name: "a".into() };
        hub.publish(&evt);
        assert_eq!(rx1.recv_timeout(Duration::from_secs(1)).unwrap(), evt);
        assert_eq!(rx2.recv_timeout(Duration::from_secs(1)).unwrap(), evt);
        drop(rx2);
        hub.publish(&evt); // 触发清理
        assert!(rx1.recv_timeout(Duration::from_secs(1)).is_ok());
    }
}
