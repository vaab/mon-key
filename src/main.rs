use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{unbounded, Receiver};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::thread;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind { Down, Up }

#[derive(Debug, Clone)]
struct Event { delta_ms: u64, key: String, kind: Kind }

fn parse_line(line: &str) -> Option<Event> {
    // Expected: "+321 ms\t<device>\t<id>\t<key> <down|up>"
    let mut cols = line.split('\t');
    let delta_col = cols.next()?; // "+321 ms"
    let _device = cols.next()?;   // device (unused)
    let _id = cols.next()?;       // id (unused)
    let last = cols.next()?;      // "s down" or "leftmouse up"

    // parse delta
    let mut num = String::new();
    for ch in delta_col.chars() { if ch.is_ascii_digit() { num.push(ch); } }
    let delta_ms: u64 = num.parse().ok()?;

    let mut parts = last.rsplitn(2, ' ');
    let state = parts.next()?.trim();
    let key = parts.next()?.trim().to_string();
    let kind = match state { "down" => Kind::Down, "up" => Kind::Up, _ => return None };
    Some(Event { delta_ms, key, kind })
}

#[derive(Default, Clone)]
struct Segment { key: String, start: u64, end: u64 }
#[derive(Default, Clone)]
struct Tap { key: String, at: u64 }

#[derive(Default, Clone)]
struct Sequence {
    // Row order by first appearance
    row_index: HashMap<String, usize>,
    row_order: Vec<String>,
    // Active holds (key -> start_time)
    holds: HashMap<String, u64>,
    // Finished segments and taps
    segments: Vec<Segment>,
    taps: Vec<Tap>,
    // Current time within sequence
    now_ms: u64,
}

impl Sequence {
    fn new() -> Self { Self::default() }
    fn ensure_row(&mut self, key: &str) -> usize {
        if let Some(&i) = self.row_index.get(key) { return i; }
        let i = self.row_order.len();
        self.row_order.push(key.to_string());
        self.row_index.insert(key.to_string(), i);
        i
    }
    fn on_down(&mut self, key: &str) { self.ensure_row(key); self.holds.insert(key.to_string(), self.now_ms); }
    fn on_up(&mut self, key: &str) {
        self.ensure_row(key);
        if let Some(start) = self.holds.remove(key) {
            if self.now_ms == start { // instant tap
                self.taps.push(Tap { key: key.to_string(), at: self.now_ms });
            } else {
                self.segments.push(Segment { key: key.to_string(), start, end: self.now_ms });
            }
        } else {
            // Up without known down: treat as tap at now
            self.taps.push(Tap { key: key.to_string(), at: self.now_ms });
        }
    }
    fn t_max(&self) -> u64 {
        let mut t = self.now_ms;
        for (_, &s) in &self.holds { if s > t { t = s; } }
        t
    }
}

fn stdin_reader(rx_gap_ms: u64) -> Receiver<Event> {
    let (tx, rx) = unbounded();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin).lines() {
            match line { Ok(l) => {
                if let Some(mut ev) = parse_line(&l) {
                    // Accumulate deltas across lines until we emit the event to the UI; the UI
                    // will decide when to start a new sequence based on >= gap_ms.
                    // We pass the delta as-is; the UI state machine will interpret it.
                    if tx.send(ev).is_err() { break; }
                }
            }, Err(_) => break }
        }
    });
    rx
}

struct AppState {
    rx: Receiver<Event>,
    gap_ms: u64,
    prev: Option<Sequence>,
    cur: Option<Sequence>,
}

impl AppState {
    fn new(rx: Receiver<Event>, gap_ms: u64) -> Self { Self { rx, gap_ms, prev: None, cur: None } }
    fn ingest(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match &mut self.cur {
                None => {
                    let mut s = Sequence::new();
                    s.now_ms = 0;
                    // first event starts at t=0
                    match ev.kind { Kind::Down => s.on_down(&ev.key), Kind::Up => s.on_up(&ev.key) }
                    self.cur = Some(s);
                }
                Some(s) => {
                    if ev.delta_ms >= self.gap_ms {
                        // finalize current -> prev, start new sequence
                        self.prev = self.cur.take();
                        let mut s2 = Sequence::new();
                        s2.now_ms = 0; // reset time for new burst
                        match ev.kind { Kind::Down => s2.on_down(&ev.key), Kind::Up => s2.on_up(&ev.key) }
                        self.cur = Some(s2);
                    } else {
                        s.now_ms = s.now_ms.saturating_add(ev.delta_ms);
                        match ev.kind { Kind::Down => s.on_down(&ev.key), Kind::Up => s.on_up(&ev.key) }
                    }
                }
            }
        }
    }
}

// ---------------- UI -----------------
use eframe::{egui, egui::{Color32, Pos2, Rect, Stroke}};

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull pending stdin events without blocking
        self.ingest();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("keyd live graph");
            ui.label("Sequences split on ≥ 1000 ms gaps. Thick line = hold, dot = tap.");
            ui.separator();

            let available = ui.available_size_before_wrap();
            let (resp, painter) = ui.allocate_painter(available, egui::Sense::hover());
            let rect = resp.rect;

            // Choose which sequence to draw: live (cur) if present, else previous (prev)
            let seq = if self.cur.is_some() { self.cur.as_ref() } else { self.prev.as_ref() };
            if let Some(s) = seq {
                let left_gutter = 110.0; // space for labels
                let right_pad = 10.0;
                let top_pad = 10.0;
                let row_h = 28.0;
                let line_thick = 10.0;
                let dot_r = 6.0;

                let rows = s.row_order.len().max(1) as f32;

                // Axis mapping
                let tmax = s.t_max().max(1) as f32;
                let x0 = rect.left() + left_gutter;
                let x1 = rect.right() - right_pad;
                let y0 = rect.top() + top_pad;

                // grid line at each 100 ms
                let step_ms = 100.0_f32;
                let mut ms = 0.0_f32;
                while ms <= tmax {
                    let x = x0 + (ms / tmax) * (x1 - x0);
                    painter.line_segment(
                        [Pos2::new(x, y0), Pos2::new(x, y0 + rows * row_h)],
                        Stroke::new(1.0, Color32::from_gray(60)),
                    );
                    ms += step_ms;
                }

                // labels + baselines
                for (i, key) in s.row_order.iter().enumerate() {
                    let y = y0 + i as f32 * row_h + row_h * 0.5;
                    painter.text(
                        Pos2::new(rect.left() + 8.0, y - 8.0),
                        egui::Align2::LEFT_TOP,
                        key,
                        egui::FontId::proportional(14.0),
                        Color32::LIGHT_GRAY,
                    );
                    painter.line_segment(
                        [Pos2::new(x0, y), Pos2::new(x1, y)],
                        Stroke::new(1.0, Color32::from_gray(40)),
                    );
                }

                // helper for x mapping
                let to_x = |t_ms: u64| -> f32 { x0 + (t_ms as f32 / tmax) * (x1 - x0) };

                // finished segments
                for seg in &s.segments {
                    let row = s.row_index[&seg.key];
                    let y = y0 + row as f32 * row_h + row_h * 0.5;
                    painter.line_segment(
                        [Pos2::new(to_x(seg.start), y), Pos2::new(to_x(seg.end), y)],
                        Stroke::new(line_thick, Color32::from_rgb(90, 170, 255)),
                    );
                }
                // active holds (draw to now)
                for (key, &start) in &s.holds {
                    let row = s.row_index[key];
                    let y = y0 + row as f32 * row_h + row_h * 0.5;
                    painter.line_segment(
                        [Pos2::new(to_x(start), y), Pos2::new(to_x(s.now_ms), y)],
                        Stroke::new(line_thick, Color32::from_rgb(90, 170, 255)),
                    );
                }
                // taps
                for tap in &s.taps {
                    let row = s.row_index[&tap.key];
                    let y = y0 + row as f32 * row_h + row_h * 0.5;
                    painter.circle_filled(Pos2::new(to_x(tap.at), y), dot_r, Color32::from_rgb(255, 210, 100));
                }

                // time legend
                painter.text(
                    Pos2::new(x0, rect.bottom() - 18.0),
                    egui::Align2::LEFT_CENTER,
                    format!("0 ms … {} ms", s.now_ms),
                    egui::FontId::proportional(14.0),
                    Color32::GRAY,
                );
            } else {
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "Waiting for input on stdin…",
                    egui::FontId::proportional(16.0),
                    Color32::GRAY,
                );
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_millis(16)); // ~60 FPS
    }
}

fn main() -> Result<()> {
    let rx = stdin_reader(1000);
    let native_options = eframe::NativeOptions::default();
    let app = AppState::new(rx, 1000);
    eframe::run_native(
        "keyd live graph",
        native_options,
        Box::new(|_| Box::new(app)),
    ).map_err(|e| anyhow!("eframe error: {e}"))
}


