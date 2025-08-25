use crossbeam_channel::{unbounded, Receiver};
use evdev::{self, Device, EventSummary, KeyCode};
use std::{path::PathBuf, thread, time::SystemTime};

use crate::{Event, Kind};

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub vendor: Option<u16>,
    pub product: Option<u16>,
}


fn keycode_label(code: KeyCode) -> String {
    format!("{:?}", code).trim_start_matches("KEY_").to_ascii_lowercase()
}

pub fn spawn_evdev_reader(dev_path: PathBuf) -> Receiver<Event> {
    let (tx, rx) = unbounded::<Event>();
    thread::spawn(move || {
        let mut dev = match Device::open(&dev_path) {
            Ok(d) => d,
            Err(_) => return,
        };
        let mut last_ts: Option<SystemTime> = None;

        loop {
            let Ok(mut iter) = dev.fetch_events() else { break; };
            for ev in &mut iter {
                match ev.destructure() {
                    EventSummary::Key(_, code, val) => {
                        if val == 2 { continue; } // ignore repeats; we can revisit
                        if code == KeyCode::KEY_INSERT { continue; } // <-- ignore Insert entirely
                        let kind = if val != 0 { Kind::Down } else { Kind::Up };
                        let key = keycode_label(code);

                        let ts = ev.timestamp();
                        let delta_ms = match last_ts {
                            Some(prev) => ts.duration_since(prev).map(|d| d.as_millis() as u64).unwrap_or(0),
                            None => 0,
                        };
                        last_ts = Some(ts);

                        let _ = tx.send(Event { delta_ms, key, kind });
                    }
                    _ => {}
                }
            }
        }
    });
    rx
}
