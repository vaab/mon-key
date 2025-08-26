use evdev::{self, Device, EventType, KeyCode, RelativeAxisCode, AttributeSetRef};
use std::collections::HashMap;

use crate::evdev_reader::DeviceInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceType { Keyboard, Mouse, KeyboardMouse, Other }

#[inline]
fn has_keyboard_keys(keys: &AttributeSetRef<KeyCode>) -> bool {
    // Same scancode window Kanata uses: 1..=115 (before power button range)
    const LOWER: u16 = 1;
    const UPPER: u16 = 115;
    (LOWER..=UPPER).any(|sc| keys.contains(KeyCode::new(sc)))
}

#[inline]
fn has_alpha_letters(keys: &AttributeSetRef<KeyCode>) -> bool {
    const LETTERS: [KeyCode; 26] = [
        KeyCode::KEY_A, KeyCode::KEY_B, KeyCode::KEY_C, KeyCode::KEY_D, KeyCode::KEY_E,
        KeyCode::KEY_F, KeyCode::KEY_G, KeyCode::KEY_H, KeyCode::KEY_I, KeyCode::KEY_J,
        KeyCode::KEY_K, KeyCode::KEY_L, KeyCode::KEY_M, KeyCode::KEY_N, KeyCode::KEY_O,
        KeyCode::KEY_P, KeyCode::KEY_Q, KeyCode::KEY_R, KeyCode::KEY_S, KeyCode::KEY_T,
        KeyCode::KEY_U, KeyCode::KEY_V, KeyCode::KEY_W, KeyCode::KEY_X, KeyCode::KEY_Y,
        KeyCode::KEY_Z,
    ];
    LETTERS.iter().any(|k| keys.contains(*k))
}

fn detect_type(dev: &Device) -> DeviceType {
    let (kbd_any, kbd_alpha) = dev
        .supported_keys()
        .map(|k| (has_keyboard_keys(k), has_alpha_letters(k)))
        .unwrap_or((false, false));

    // Treat as “keyboard” only if it has sensible keys AND at least one A..Z
    let is_keyboard = kbd_any && kbd_alpha;

    let is_mouse = dev
        .supported_relative_axes()
        .is_some_and(|axes| axes.contains(RelativeAxisCode::REL_X));

    match (is_keyboard, is_mouse) {
        (true,  true ) => DeviceType::KeyboardMouse,
        (true,  false) => DeviceType::Keyboard,
        (false, true ) => DeviceType::Mouse,
        (false, false) => DeviceType::Other,
    }
}
#[inline]
fn keyboard_score(dev: &Device, dt: DeviceType) -> i32 {
    // Prefer pure keyboards over keyboard+mouse combos, then count scancodes in [1..=115]
    let base = match dt {
        DeviceType::Keyboard => 1000,
        DeviceType::KeyboardMouse => 900,
        _ => 0,
    };
    let count_in_range = dev.supported_keys().map(|keys| {
        const LOWER: u16 = 1;
        const UPPER: u16 = 115;
        (LOWER..=UPPER).filter(|sc| keys.contains(KeyCode::new(*sc))).count() as i32
    }).unwrap_or(0);
    base + count_in_range
}

/// Enumerate, classify, and **deduplicate** keyboards with heuristics mirroring Kanata's
/// `is_input_device`, but we **do not** exclude virtual devices like "kanata".
/// We emulate `DeviceDetectMode::KeyboardMice` (accept Keyboard and KeyboardMouse).
/// Runs without reopening devices (uses `evdev::enumerate()`), which keeps it snappy.
pub fn collect_devices() -> Vec<DeviceInfo> {
    // Keyed by (name, vendor, product) to collapse multi-interface keyboards
    let mut best: HashMap<(String, Option<u16>, Option<u16>), (i32, DeviceInfo)> = HashMap::new();

    for (path, dev) in evdev::enumerate() {
        // Must have KEY capability at all
        if !dev.supported_events().contains(EventType::KEY) {
            continue;
        }
        let dt = detect_type(&dev);
        // Accept only Keyboard and KeyboardMouse (like DeviceDetectMode::KeyboardMice)
        match dt { DeviceType::Keyboard | DeviceType::KeyboardMouse => {}, _ => continue }

        let id = dev.input_id();
        let name = dev.name().unwrap_or("Unknown").to_string();
        let info = DeviceInfo {
            path: path.clone(),
            name: name.clone(),
            vendor: Some(id.vendor()),
            product: Some(id.product()),
        };
        let score = keyboard_score(&dev, dt);
        let key = (name, Some(id.vendor()), Some(id.product()));

        match best.get(&key) {
            Some((old, _)) if *old >= score => {}
            _ => { best.insert(key, (score, info)); }
        }
    }

    best.into_values().map(|(_, info)| info).collect()
}



use crossbeam_channel::{unbounded, Receiver};
use inotify::{EventMask, Inotify, WatchMask};
use std::thread;


/// Spawn an inotify watcher on /dev/input to auto-refresh device list on add/remove.
/// Linux-only. Non-intrusive: users don't need read perms on the event nodes for this.
pub fn spawn_input_dir_watcher() -> Receiver<()> {
    let (tx, rx) = unbounded();


    thread::spawn(move || {
        let mut ino = match Inotify::init() {
            Ok(i) => i,
            Err(_) => return,
        };
        // Watch create/delete/move events; devices often appear as MOVED_TO
        let mask = WatchMask::CREATE | WatchMask::DELETE | WatchMask::MOVED_TO | WatchMask::MOVED_FROM;
        if ino.watches().add("/dev/input", mask).is_err() {
            return;
        }


        let mut buf = [0u8; 4096];
        loop {
            let Ok(events) = ino.read_events_blocking(&mut buf) else { break };
            let mut changed = false;
            for ev in events {
                // Only react to event* nodes
                if let Some(name) = ev.name {
                    if name.to_string_lossy().starts_with("event")
                        && ev.mask.intersects(EventMask::CREATE | EventMask::DELETE | EventMask::MOVED_TO | EventMask::MOVED_FROM)
                    {
                        changed = true;
                    }
                }
            }
            if changed {
                let _ = tx.send(());
            }
        }
    });


    rx
}
