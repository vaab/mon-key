use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver};
use eframe::{egui, egui::{Align2, Color32, FontId, Pos2, Rect, Rounding, Stroke}};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};

// ===== Events & parsing =====
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Down,
    Up,
}

#[derive(Debug, Clone)]
struct Event {
    delta_ms: u64,
    key: String,
    kind: Kind,
}

fn parse_line(line: &str) -> Option<Event> {
    // Format: "+321 ms <device>    <id>    <key> <down|up>"
    let mut cols = line.split('\t');
    let delta_col = cols.next()?; // "+321 ms"
    let device = cols.next()?; // device string
    let _id = cols.next()?; // id (unused)
    let last = cols.next()?; // e.g., "leftcontrol down"

    // Parse delta
    let delta_ms: u64 = delta_col
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;

    // Split into key + state
    let mut parts = last.rsplitn(2, ' ');
    let state = parts.next()?.trim();
    let key = parts.next()?.trim().to_string();

    // Filter out mouse/touchpad/pointer noise
    let dev_l = device.to_ascii_lowercase();
    let key_l = key.to_ascii_lowercase();
    if dev_l.contains("mouse")
        || dev_l.contains("touchpad")
        || dev_l.contains("pointer")
        || key_l.contains("mouse")
        || key_l.starts_with("btn")
    {
        return None;
    }

    let kind = match state {
        "down" => Kind::Down,
        "up" => Kind::Up,
        _ => return None,
    };
    Some(Event { delta_ms, key, kind })
}

fn spawn_stdin_reader() -> Receiver<Event> {
    let (tx, rx) = unbounded();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin).lines() {
            match line {
                Ok(l) => {
                    if let Some(ev) = parse_line(&l) {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

// ===== Sequence model =====
#[derive(Default, Clone)]
struct Segment {
    key: String,
    start: u64,
    end: u64,
}

#[derive(Default, Clone)]
struct Tap {
    key: String,
    at: u64,
}

#[derive(Clone)]
struct IdleGap {
    start: u64,
    end: u64,
    from_key: String,
    to_key: String,
}

#[derive(Default, Clone)]
struct Sequence {
    uid: u64,
    row_index: HashMap<String, usize>,
    row_order: Vec<String>,
    holds: HashMap<String, u64>, // key -> start time
    segments: Vec<Segment>,
    taps: Vec<Tap>,
    idles: Vec<IdleGap>,
    idle_start: Option<(u64, String)>,
    now_ms: u64,
}

impl Sequence {
    fn new() -> Self {
        Self::default()
    }
    fn ensure_row(&mut self, key: &str) -> usize {
        if let Some(&i) = self.row_index.get(key) {
            return i;
        }
        let i = self.row_order.len();
        self.row_order.push(key.to_string());
        self.row_index.insert(key.to_string(), i);
        i
    }
    fn on_down(&mut self, key: &str) {
        // If we were idle, close the idle gap
        if let Some((start, from_key)) = self.idle_start.take() {
            self.idles.push(IdleGap {
                start,
                end: self.now_ms,
                from_key,
                to_key: key.to_string(),
            });
        }
        self.ensure_row(key);
        self.holds.insert(key.to_string(), self.now_ms);
    }
    fn on_up(&mut self, key: &str) {
        self.ensure_row(key);
        if let Some(start) = self.holds.remove(key) {
            if self.now_ms == start {
                self.taps.push(Tap { key: key.to_string(), at: self.now_ms });
            } else {
                self.segments.push(Segment { key: key.to_string(), start, end: self.now_ms });
            }
        } else {
            // Up without a known down -> treat as tap
            self.taps.push(Tap { key: key.to_string(), at: self.now_ms });
        }
        if self.holds.is_empty() {
            self.idle_start = Some((self.now_ms, key.to_string()));
        }
    }
}

// ===== App state =====
#[derive(Clone)]
struct ScaleAnim {
    start: Instant,
    from: f32,
    to: f32,
    dur_ms: u64,
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
fn nice_step(raw: f32) -> f32 {
    let raw = raw.max(1.0);
    let exp = raw.log10().floor();
    let base = 10f32.powf(exp);
    let m = raw / base;
    let nice = if m <= 1.0 { 1.0 } else if m <= 2.0 { 2.0 } else if m <= 5.0 { 5.0 } else { 10.0 };
    nice * base
}

struct AppState {
    rx: Receiver<Event>,
    gap_ms: u64,
    sequences: Vec<Sequence>, // history, most-recent first
    cur: Option<Sequence>,    // recording
    last_event: Option<Instant>,
    anim_scale: Option<ScaleAnim>,
    next_uid: u64,
    anim_target_uid: Option<u64>,
    selected_uid: Option<u64>,
}

impl AppState {
    fn new(rx: Receiver<Event>, gap_ms: u64) -> Self {
        Self {
            rx,
            gap_ms,
            sequences: Vec::new(),
            cur: None,
            last_event: None,
            anim_scale: None,
            next_uid: 1,
            anim_target_uid: None,
            selected_uid: None,
        }
    }

    fn ingest(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match &mut self.cur {
                None => {
                    let mut s = Sequence::new();
                    s.uid = self.next_uid;
                    self.next_uid += 1;
                    s.now_ms = 0;
                    match ev.kind {
                        Kind::Down => s.on_down(&ev.key),
                        Kind::Up => s.on_up(&ev.key),
                    }
                    // When a new recording starts, focus/select it
                    self.selected_uid = Some(s.uid);
                    self.cur = Some(s);
                }
                Some(s) => {
                    s.now_ms = s.now_ms.saturating_add(ev.delta_ms);
                    match ev.kind {
                        Kind::Down => s.on_down(&ev.key),
                        Kind::Up => s.on_up(&ev.key),
                    }
                }
            }
            self.last_event = Some(Instant::now());
        }
    }

    fn finalize_if_idle_elapsed(&mut self) {
        let elapsed_ok = self
            .last_event
            .map(|t| t.elapsed() >= Duration::from_millis(self.gap_ms))
            .unwrap_or(false);
        let no_holds = self.cur.as_ref().map(|s| s.holds.is_empty()).unwrap_or(false);
        if self.cur.is_some() && elapsed_ok && no_holds {
            if let Some(sref) = self.cur.as_ref() {
                let vis_elapsed = self
                    .last_event
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(0);
                let from = (sref.now_ms.saturating_add(vis_elapsed)).max(1) as f32;
                let to = (sref.now_ms.max(1)) as f32;
                self.anim_scale = Some(ScaleAnim { start: Instant::now(), from, to, dur_ms: 220 });
                // Remember which sequence should animate
                self.anim_target_uid = Some(sref.uid);
            }
            if let Some(done) = self.cur.take() {
                let uid = done.uid;
                self.sequences.insert(0, done);
                if self.selected_uid.is_none() {
                    self.selected_uid = Some(uid);
                }
            }
        }
    }
}
// ===== Drawing helpers =====
fn measure_max_label_px(ctx: &egui::Context, font: &FontId, labels: &[String]) -> f32 {
    ctx.fonts(|f| {
        labels
            .iter()
            .map(|k| f.layout_no_wrap(k.clone(), font.clone(), Color32::WHITE).size().x)
            .fold(0.0, f32::max)
    })
}

fn draw_grid_and_ticks(
    ctx: &egui::Context,
    painter: &egui::Painter,
    tmax: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    rows: f32,
    row_h: f32,
) {
    let label_font = FontId::proportional(12.0);
    let max_label_text = format!("{:.0} ms", tmax.round());
    let est_label_px: f32 = ctx.fonts(|f| {
        f.layout_no_wrap(max_label_text, label_font.clone(), Color32::WHITE)
            .size()
            .x
    });
    let min_px = est_label_px + 16.0;
    let plot_w = (x1 - x0).max(1.0);
    let mut step_ms = nice_step(tmax / 8.0);
    loop {
        let dx = (step_ms / tmax) * plot_w;
        if dx >= min_px {
            break;
        }
        let next = nice_step(step_ms * 1.5);
        if (next - step_ms).abs() < f32::EPSILON {
            break;
        }
        step_ms = next;
    }
    let mut ms = 0.0_f32;
    while ms <= tmax + 0.0001 {
        let x = x0 + (ms / tmax) * (x1 - x0);
        painter.line_segment(
            [Pos2::new(x, y0), Pos2::new(x, y0 + rows * row_h)],
            Stroke::new(1.0, Color32::from_gray(60)),
        );
        painter.text(
            Pos2::new(x, y0 - 6.0),
            Align2::CENTER_BOTTOM,
            format!("{:.0} ms", ms.round()),
            label_font.clone(),
            Color32::GRAY,
        );
        ms += step_ms;
    }
}

fn draw_labels_and_rows(
    painter: &egui::Painter,
    row_order: &[String],
    x0: f32,
    x1: f32,
    y0: f32,
    row_h: f32,
    label_pad: f32,
) {
    let key_font = FontId::proportional(14.0);
    for (i, key) in row_order.iter().enumerate() {
        let y = y0 + i as f32 * row_h + row_h * 0.5;
        let label_x = x0 - label_pad;
        painter.text(
            Pos2::new(label_x, y),
            Align2::RIGHT_CENTER,
            key,
            key_font.clone(),
            Color32::LIGHT_GRAY,
        );
        painter.line_segment(
            [Pos2::new(x0, y), Pos2::new(x1, y)],
            Stroke::new(1.0, Color32::from_gray(40)),
        );
    }
}

fn to_x(t_ms: u64, tmax: f32, x0: f32, x1: f32) -> f32 {
    x0 + (t_ms as f32 / tmax) * (x1 - x0)
}

fn draw_finished_segments(
    painter: &egui::Painter,
    seq: &Sequence,
    tmax: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    row_h: f32,
    line_thick: f32,
) {
    for seg in &seq.segments {
        let row = seq.row_index[&seg.key] as f32;
        let y = y0 + row * row_h + row_h * 0.5;
        let x_start = to_x(seg.start, tmax, x0, x1);
        let x_end = to_x(seg.end, tmax, x0, x1).max(x_start + 1.0);
        let y_top = y - (line_thick * 0.5);
        let y_bot = y + (line_thick * 0.5);
        let rect_seg = Rect::from_min_max(Pos2::new(x_start, y_top), Pos2::new(x_end, y_bot));
        painter.rect_filled(rect_seg, Rounding::same(2.0), Color32::from_rgb(90, 170, 255));

        // Duration label
        let mid_x = (x_start + x_end) * 0.5;
        let dur = seg.end.saturating_sub(seg.start);
        painter.text(
            Pos2::new(mid_x, y_top - 1.0),
            Align2::CENTER_BOTTOM,
            format!("{} ms", dur),
            FontId::proportional(10.0),
            Color32::LIGHT_GRAY,
        );
    }
}

fn draw_active_holds(
    painter: &egui::Painter,
    seq: &Sequence,
    now_vis_ms: u64,
    tmax: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    row_h: f32,
    line_thick: f32,
) {
    for (key, &start) in &seq.holds {
        let row = seq.row_index[key] as f32;
        let y = y0 + row * row_h + row_h * 0.5;
        let x_start = to_x(start, tmax, x0, x1);
        let x_end = to_x(now_vis_ms, tmax, x0, x1).max(x_start + 1.0);
        let y_top = y - (line_thick * 0.5);
        let y_bot = y + (line_thick * 0.5);
        let rect_live = Rect::from_min_max(Pos2::new(x_start, y_top), Pos2::new(x_end, y_bot));
        painter.rect_filled(rect_live, Rounding::same(4.0), Color32::WHITE);
    }
}

fn draw_taps(
    painter: &egui::Painter,
    seq: &Sequence,
    tmax: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    row_h: f32,
) {
    let dot_r = 6.0;
    for tap in &seq.taps {
        let row = seq.row_index[&tap.key] as f32;
        let y = y0 + row * row_h + row_h * 0.5;
        painter.circle_filled(Pos2::new(to_x(tap.at, tmax, x0, x1), y), dot_r, Color32::from_rgb(255, 210, 100));
    }
}

fn draw_idle_gaps(
    painter: &egui::Painter,
    seq: &Sequence,
    tmax: f32,
    x0: f32,
    x1: f32,
    y0: f32,
    row_h: f32,
) {
    for idle in &seq.idles {
        if idle.end <= idle.start {
            continue;
        }
        let xs = to_x(idle.start, tmax, x0, x1);
        let xe = to_x(idle.end, tmax, x0, x1);
        let row_s = seq.row_index[&idle.from_key];
        let row_e = seq.row_index[&idle.to_key];
        let ys = y0 + row_s as f32 * row_h + row_h * 0.5;
        let ye = y0 + row_e as f32 * row_h + row_h * 0.5;
        let thin = Stroke::new(1.0, Color32::from_rgba_unmultiplied(180, 180, 180, 128));

        let arrow_row: usize = if row_s == row_e {
            row_s
        } else if row_e > row_s {
            (row_s + 1).min(seq.row_order.len() - 1)
        } else {
            row_s.saturating_sub(1)
        };
        let y_arrow = y0 + arrow_row as f32 * row_h + row_h * 0.5;

        // verticals to arrow row
        painter.line_segment([Pos2::new(xs, ys), Pos2::new(xs, y_arrow)], thin);
        painter.line_segment([Pos2::new(xe, ye), Pos2::new(xe, y_arrow)], thin);

        // dotted horizontal arrow
        let dotted_col = Color32::from_rgba_unmultiplied(200, 200, 200, 150);
        let step = 4.0;
        let dot_r2 = 0.8;
        let mut x = xs;
        while x <= xe {
            painter.circle_filled(Pos2::new(x, y_arrow), dot_r2, dotted_col);
            x += step;
        }
        let ah = 5.0;
        painter.line_segment([Pos2::new(xs + ah, y_arrow - ah * 0.6), Pos2::new(xs, y_arrow)], thin);
        painter.line_segment([Pos2::new(xs + ah, y_arrow + ah * 0.6), Pos2::new(xs, y_arrow)], thin);
        painter.line_segment([Pos2::new(xe - ah, y_arrow - ah * 0.6), Pos2::new(xe, y_arrow)], thin);
        painter.line_segment([Pos2::new(xe - ah, y_arrow + ah * 0.6), Pos2::new(xe, y_arrow)], thin);

        // label
        let mid_x = (xs + xe) * 0.5;
        let dur = (idle.end - idle.start) as u64;
        painter.text(
            Pos2::new(mid_x, y_arrow - 2.0),
            Align2::CENTER_BOTTOM,
            format!("{} ms", dur),
            FontId::proportional(10.0),
            Color32::from_rgba_unmultiplied(200, 200, 200, 160),
        );
    }
}

fn draw_sequence_block(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    seq: &Sequence,
    is_live: bool,
    anim_scale: Option<&ScaleAnim>,
    last_event: Option<Instant>,
    selected: bool,
) -> egui::Response {
    // ---- Metrics ----
    let key_font = FontId::proportional(14.0);
    let label_pad = 20.0;
    let sel_w = 5.0;      // selection bar width (reserved in gutter)
    let sel_gap = 5.0;    // gap between selection bar and labels
    let left_gutter = (measure_max_label_px(ctx, &key_font, &seq.row_order) + label_pad + sel_w + sel_gap).max(30.0);
    let right_pad = 10.0;
    let top_pad = 22.0;
    let row_h = 28.0;
    let line_thick = 10.0;
    let rows = seq.row_order.len().max(1) as f32;
    let block_h = top_pad + rows * row_h + 18.0;

    let (resp, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width(), block_h),
        egui::Sense::click(),
    );
    let rect = resp.rect;

    // ---- Time axis ----
    let vis_elapsed_ms: u64 = if is_live {
        last_event.map(|t| t.elapsed().as_millis() as u64).unwrap_or(0)
    } else {
        0
    };
    let now_vis_ms = seq.now_ms.saturating_add(vis_elapsed_ms);
    let mut tmax = (now_vis_ms.max(1)) as f32;
    if !is_live {
        if let Some(anim) = anim_scale {
            let a = (anim.start.elapsed().as_millis() as f32 / anim.dur_ms as f32).clamp(0.0, 1.0);
            tmax = lerp(anim.from, anim.to, smoothstep(a));
        } else {
            tmax = (seq.now_ms.max(1)) as f32;
        }
    }

    let x0 = rect.left() + left_gutter;
    let x1 = rect.right() - right_pad;
    let y0 = rect.top() + top_pad;

    // ---- Grid, labels, content ----
    draw_grid_and_ticks(ctx, &painter, tmax, x0, x1, y0, rows, row_h);
    draw_labels_and_rows(&painter, &seq.row_order, x0, x1, y0, row_h, label_pad);
    draw_finished_segments(&painter, seq, tmax, x0, x1, y0, row_h, line_thick);
    if is_live {
        draw_active_holds(&painter, seq, now_vis_ms, tmax, x0, x1, y0, row_h, line_thick);
    }
    draw_taps(&painter, seq, tmax, x0, x1, y0, row_h);
    draw_idle_gaps(&painter, seq, tmax, x0, x1, y0, row_h);

    // Selection visuals (no margins)
    // Dim non-selected blocks slightly (full-rect overlay, no rounding)
    if !selected {
        painter.rect_filled(
            rect,
            Rounding::same(0.0),
            Color32::from_rgba_unmultiplied(0, 0, 0, 45),
        );
    } else {
        // Thick blue bar on the left edge only, with reserved gutter space for labels
        let sel_w = 5.0;
        let left_bar = Rect::from_min_max(
            Pos2::new(rect.left(), rect.top()),
            Pos2::new((rect.left() + sel_w).min(rect.right()), rect.bottom()),
        );
        painter.rect_filled(left_bar, Rounding::same(0.0), Color32::from_rgb(90, 170, 255));
    }

    resp
}

// ===== eframe::App =====
impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ingest();
        self.finalize_if_idle_elapsed();

        // If an animation finished, clear it
        if let Some(anim) = &self.anim_scale {
            if anim.start.elapsed() >= Duration::from_millis(anim.dur_ms) {
                self.anim_scale = None;
                self.anim_target_uid = None;
            }
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // Header: title left, red dot at far right when recording
            let recording = self
                .cur
                .as_ref()
                .map(|s| !s.holds.is_empty() || self.last_event.map(|t| t.elapsed() < Duration::from_millis(self.gap_ms)).unwrap_or(false))
                .unwrap_or(false);

            ui.horizontal(|ui| {
                ui.heading("keyd live graph");
                let dot_w = 14.0_f32;
                let space = (ui.available_width() - dot_w).max(0.0);
                if space > 0.0 {
                    ui.add_space(space);
                }
                if recording {
                    let (resp, rp) = ui.allocate_painter(egui::vec2(dot_w, dot_w), egui::Sense::hover());
                    rp.circle_filled(resp.rect.center(), 6.0, Color32::from_rgb(220, 50, 50));
                }
            });
            ui.label(format!(
                "Sequences split on ≥ {} ms gaps. Thick line = hold, dot = tap.",
                self.gap_ms
            ));
            ui.separator();

            // Ensure there is always a default selection
            if self.selected_uid.is_none() {
                if let Some(s) = self.cur.as_ref() {
                    self.selected_uid = Some(s.uid);
                } else if let Some(first) = self.sequences.first() {
                    self.selected_uid = Some(first.uid);
                }
            }

            // Blocks: live first, then history (top-most is latest)
            let mut blocks: Vec<(&Sequence, bool)> = Vec::new();
            if let Some(s) = self.cur.as_ref() {
                blocks.push((s, true));
            }
            for s in &self.sequences {
                blocks.push((s, false));
            }

            if blocks.is_empty() {
                let (resp, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::hover());
                painter.text(
                    resp.rect.center(),
                    Align2::CENTER_CENTER,
                    "Waiting for input on stdin…",
                    FontId::proportional(16.0),
                    Color32::GRAY,
                );
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for (idx, (s, is_live)) in blocks.into_iter().enumerate() {
                            let is_target = !is_live && self.anim_target_uid.map_or(false, |id| id == s.uid);
                            let anim = if is_target { self.anim_scale.as_ref() } else { None };
                            let is_selected = self.selected_uid.map_or(false, |id| id == s.uid);
                            let resp = draw_sequence_block(ctx, ui, s, is_live, anim, self.last_event, is_selected);
                            if resp.clicked() {
                                self.selected_uid = Some(s.uid);
                            }

                            if idx + 1 < self.sequences.len() + if self.cur.is_some() { 1 } else { 0 } {
                                let sep_h = 6.0;
                                let (r2, p2) = ui.allocate_painter(egui::vec2(ui.available_width(), sep_h), egui::Sense::hover());
                                p2.rect_filled(r2.rect, Rounding::same(3.0), Color32::from_gray(60));
                                // no extra spacing: next block sticks to the separator
                            }
                        }
                    });
            }
        });

        // ~60 FPS
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

// ===== main =====
fn main() -> Result<()> {
    // Parse --gap-ms <u64>
    let mut gap_ms: u64 = 1000;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--gap-ms" | "-g" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse() {
                        gap_ms = n;
                    }
                }
            }
            _ => {}
        }
    }

    let rx = spawn_stdin_reader();
    let native_options = eframe::NativeOptions::default();
    let app = AppState::new(rx, gap_ms);
    eframe::run_native("keyd live graph", native_options, Box::new(|_| Box::new(app)))
        .map_err(|e| anyhow!("eframe error: {e}"))
}
