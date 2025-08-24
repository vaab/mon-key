use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};

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
        if self.holds.is_empty() {
            // Start idle from the key that just ended
            self.idle_start = Some((self.now_ms, key.to_string()));
        }
    }
}

fn stdin_reader(_rx_gap_ms: u64) -> Receiver<Event> {
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
    last_event: Option<Instant>,
    anim_scale: Option<ScaleAnim>,
}

impl AppState {
    fn new(rx: Receiver<Event>, gap_ms: u64) -> Self {
        Self { rx, gap_ms, prev: None, cur: None, last_event: None, anim_scale: None }
    }

    fn ingest(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match &mut self.cur {
                None => {
                    let mut s = Sequence::new();
                    s.now_ms = 0; // first event in a fresh sequence starts at 0
                    match ev.kind { Kind::Down => s.on_down(&ev.key), Kind::Up => s.on_up(&ev.key) }
                    self.cur = Some(s);
                }
                Some(s) => {
                    s.now_ms = s.now_ms.saturating_add(ev.delta_ms);
                    match ev.kind { Kind::Down => s.on_down(&ev.key), Kind::Up => s.on_up(&ev.key) }
                }
            }
            // independent timer anchor for recording / auto-finalize
            self.last_event = Some(Instant::now());
        }
    }
}

// ---------------- UI -----------------
use eframe::{egui, egui::{Color32, Pos2, Rect, Stroke, Rounding}};

// Choose a “nice” tick step (1/2/5×10^n) for the time axis
fn nice_step(raw: f32) -> f32 {
    let raw = raw.max(1.0);
    let exp = raw.log10().floor();
    let base = 10f32.powf(exp);
    let m = raw / base;
    let nice = if m <= 1.0 { 1.0 } else if m <= 2.0 { 2.0 } else if m <= 5.0 { 5.0 } else { 10.0 };
    nice * base
}

#[derive(Clone)]
struct ScaleAnim { start: Instant, from: f32, to: f32, dur_ms: u64 }
fn lerp(a: f32, b: f32, t: f32) -> f32 { a + (b - a) * t }
fn smoothstep(t: f32) -> f32 { let t = t.clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) }

impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Pull pending stdin events without blocking
        self.ingest();
        egui::CentralPanel::default().show(ctx, |ui| {
            // finalize (timer) before drawing header
            let elapsed_ok = self
                .last_event
                .map(|t| t.elapsed() >= Duration::from_millis(self.gap_ms))
                .unwrap_or(false);
            let has_holds = self.cur.as_ref().map(|s| !s.holds.is_empty()).unwrap_or(false);
            if self.cur.is_some() && elapsed_ok && !has_holds {
                // Start scale-shrink animation from current visual time to final time
                if let Some(sref) = self.cur.as_ref() {
                    let vis_elapsed = self.last_event.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0);
                    let from = (sref.now_ms.saturating_add(vis_elapsed)).max(1) as f32;
                    let to = (sref.now_ms.max(1)) as f32;
                    self.anim_scale = Some(ScaleAnim { start: Instant::now(), from, to, dur_ms: 220 });
                }
                self.prev = self.cur.take();
            }

            // Header row: left-aligned title + red dot at far right
            ui.horizontal(|ui| {
                ui.heading("keyd live graph");
                let recording = self.cur.is_some() && (!elapsed_ok || has_holds);
                let dot_w = 14.0f32;
                let space = (ui.available_width() - dot_w).max(0.0);
                if space > 0.0 { ui.add_space(space); }
                if recording {
                    let (resp, rp) = ui.allocate_painter(egui::vec2(dot_w, dot_w), egui::Sense::hover());
                    rp.circle_filled(resp.rect.center(), 6.0, Color32::from_rgb(220, 50, 50));
                }
            });
            ui.label(format!("Sequences split on ≥ {} ms gaps. Thick line = hold, dot = tap.", self.gap_ms));
            ui.separator();

            let available = ui.available_size_before_wrap();
            let (resp, painter) = ui.allocate_painter(available, egui::Sense::hover());
            let rect = resp.rect;

            // (finalize moved earlier before header)

            // Choose which sequence to draw: live (cur) if present, else previous (prev)
            let seq = if self.cur.is_some() { self.cur.as_ref() } else { self.prev.as_ref() };
            if let Some(s) = seq {
                // Dynamic gutter based on max key label width
                let key_font = egui::FontId::proportional(14.0);
                let max_key_label_px: f32 = ctx.fonts(|f| {
                    s.row_order.iter().map(|k| {
                        f.layout_no_wrap(k.clone(), key_font.clone(), Color32::WHITE).size().x
                    }).fold(0.0, f32::max)
                });
                let label_pad = 20.0; // spacing between labels and plot
                let left_gutter = (max_key_label_px + label_pad).max(30.0); // clamp to a reasonable minimum
                let right_pad = 10.0;
                let top_pad = 22.0;
                let row_h = 28.0;
                let line_thick = 10.0;
                let dot_r = 6.0;

                let rows = s.row_order.len().max(1) as f32;

                // Axis mapping (visual time grows between events; smooth shrink after finalize)
                let is_live = self.cur.is_some();
                let vis_elapsed_ms: u64 = if is_live {
                    self.last_event.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0)
                } else { 0 };
                let now_vis_ms = s.now_ms.saturating_add(vis_elapsed_ms);
                let mut tmax = (now_vis_ms.max(1)) as f32;
                // If we are showing the previous (finalized) sequence, animate tmax down to final
                if self.cur.is_none() {
                    if let Some(anim) = &self.anim_scale {
                        let a = (anim.start.elapsed().as_millis() as f32 / anim.dur_ms as f32).clamp(0.0, 1.0);
                        tmax = lerp(anim.from, anim.to, smoothstep(a));
                        if a >= 1.0 { self.anim_scale = None; }
                    } else {
                        tmax = (s.now_ms.max(1)) as f32;
                    }
                }
                let x0 = rect.left() + left_gutter;
                let x1 = rect.right() - right_pad;
                let y0 = rect.top() + top_pad;

                // time grid + tick labels
                // Choose tick step based on available width and estimated label width
                let label_font = egui::FontId::proportional(12.0);
                let max_label_text = format!("{:.0} ms", tmax.round());
                let est_label_px: f32 = ctx.fonts(|f| {
                    let galley = f.layout_no_wrap(max_label_text, label_font.clone(), Color32::WHITE);
                    galley.size().x
                });
                let min_px = est_label_px + 16.0; // label width + spacing
                let plot_w = (x1 - x0).max(1.0);
                let mut step_ms = nice_step(tmax / 8.0);
                loop {
                    let dx = (step_ms / tmax) * plot_w;
                    if dx >= min_px { break; }
                    let next = nice_step(step_ms * 1.5);
                    if (next - step_ms).abs() < f32::EPSILON { break; }
                    step_ms = next;
                }
                let mut ms = 0.0f32;
                while ms <= tmax + 0.0001 {
                    let x = x0 + (ms / tmax) * (x1 - x0);
                    painter.line_segment(
                        [Pos2::new(x, y0), Pos2::new(x, y0 + rows * row_h)],
                        Stroke::new(1.0, Color32::from_gray(60)),
                    );
                    painter.text(
                        Pos2::new(x, y0 - 6.0),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{:.0} ms", ms.round()),
                        label_font.clone(),
                        Color32::GRAY,
                    );
                    ms += step_ms;
                }

                // labels + baselines (right‑aligned to x0)
                for (i, key) in s.row_order.iter().enumerate() {
                    let y = y0 + i as f32 * row_h + row_h * 0.5;
                    let label_x = x0 - label_pad; // use the same computed pad
                    painter.text(
                        Pos2::new(label_x, y),
                        egui::Align2::RIGHT_CENTER,
                        key,
                        key_font.clone(),
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
                    let x_start = to_x(seg.start);
                    let x_end = to_x(seg.end).max(x_start + 1.0); // ensure visible width
                    let y_top = y - (line_thick * 0.5);
                    let y_bot = y + (line_thick * 0.5);
                    let rect_seg = Rect::from_min_max(Pos2::new(x_start, y_top), Pos2::new(x_end, y_bot));
                    painter.rect_filled(rect_seg, Rounding::same(2.0), Color32::from_rgb(90, 170, 255));

                    // duration label centered above the segment (smaller and closer)
                    let mid_x = (x_start + x_end) * 0.5;
                    let dur = seg.end.saturating_sub(seg.start);
                    let label_y = y_top - 1.0; // tuck just above the bar
                    painter.text(
                        Pos2::new(mid_x, label_y),
                        egui::Align2::CENTER_BOTTOM,
                        format!("{} ms", dur),
                        egui::FontId::proportional(10.0),
                        Color32::LIGHT_GRAY,
                    );
                }
                // active holds (draw to visual now in white)
                for (key, &start) in &s.holds {
                    let row = s.row_index[key];
                    let y = y0 + row as f32 * row_h + row_h * 0.5;
                    let x_start = to_x(start);
                    let x_end = to_x(now_vis_ms).max(x_start + 1.0);
                    let y_top = y - (line_thick * 0.5);
                    let y_bot = y + (line_thick * 0.5);
                    let rect_live = Rect::from_min_max(Pos2::new(x_start, y_top), Pos2::new(x_end, y_bot));
                    painter.rect_filled(rect_live, Rounding::same(4.0), Color32::WHITE);
                }
                // taps
                for tap in &s.taps {
                    let row = s.row_index[&tap.key];
                    let y = y0 + row as f32 * row_h + row_h * 0.5;
                    painter.circle_filled(Pos2::new(to_x(tap.at), y), dot_r, Color32::from_rgb(255, 210, 100));
                }

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
    // Parse --gap-ms <u64> from CLI
    let mut gap_ms: u64 = 1000;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--gap-ms" | "-g" => {
                if let Some(v) = args.next() { if let Ok(n) = v.parse() { gap_ms = n; } }
            }
            _ => {}
        }
    }

    let rx = stdin_reader(gap_ms);
    let native_options = eframe::NativeOptions::default();
    let app = AppState::new(rx, gap_ms);
    eframe::run_native(
        "keyd live graph",
        native_options,
        Box::new(|_| Box::new(app)),
    ).map_err(|e| anyhow!("eframe error: {e}"))
}
