//! 定时播放:每秒检查一次任务表,命中 "HH:MM" 且星期匹配即触发。
//! 无外部时间库:从 Unix 时间换算时分与星期(周一=1)。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::state::App;

/// (HH, MM, 周一=1..周日=7)
pub fn now_hm_weekday() -> (u8, u8, u8) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let hour = (tod / 3600) as u8;
    let minute = ((tod % 3600) / 60) as u8;
    // 1970-01-01 是周四(=4);周一=1
    let weekday = ((days + 3).rem_euclid(7) + 1) as u8;
    (hour, minute, weekday)
}

pub fn spawn(app: Arc<App>) {
    std::thread::spawn(move || {
        let mut last_fired = String::new();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let (h, m, wd) = now_hm_weekday();
            let stamp = format!("{h:02}:{m:02}");
            let actions: Vec<crate::store::ScheduleAction> = {
                let core = app.core.lock();
                core.data
                    .tasks
                    .iter()
                    .filter(|t| t.enabled && t.time == stamp)
                    .filter(|t| t.days.is_empty() || t.days.contains(&wd))
                    .map(|t| t.action.clone())
                    .collect()
            };
            if !actions.is_empty() && last_fired != stamp {
                last_fired = stamp;
                for a in actions {
                    match a {
                        crate::store::ScheduleAction::PlayRundown => {
                            let first = app.core.lock().data.rundown.first().copied();
                            if let Some(id) = first {
                                app.play_id(id);
                            }
                        }
                        crate::store::ScheduleAction::PlayTrack(id) => app.play_id(id),
                    }
                }
            } else if last_fired != stamp {
                last_fired.clear();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekday_math() {
        // 1970-01-01 00:00 UTC = 周四
        let secs = 0i64;
        let days = secs.div_euclid(86_400);
        let weekday = ((days + 3).rem_euclid(7) + 1) as u8;
        assert_eq!(weekday, 4);
        // 2026-11-02 00:00 UTC ≈ 1793577600,周一
        let days2 = 1_793_577_600i64.div_euclid(86_400);
        let wd2 = ((days2 + 3).rem_euclid(7) + 1) as u8;
        assert_eq!(wd2, 1);
    }
}
