use anyhow::{anyhow, Result};
use crossbeam_channel::{unbounded, Receiver};
use eframe::{egui, egui::{Align2, Color32, FontId, Pos2, Rect, Rounding, Stroke, Key, CursorIcon}};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::{Duration, Instant};
use std::f32::consts::TAU;

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

    // Filter out mouse/touchpad/pointer noise, and ignore Insert key from stdin (UI owns Insert)
    let dev_l = device.to_ascii_lowercase();
    let key_l = key.to_ascii_lowercase();
    if dev_l.contains("mouse")
        || dev_l.contains("touchpad")
        || dev_l.contains("pointer")
        || key_l.contains("mouse")
        || key_l.starts_with("btn")
        || key_l == "insert" // ignore Insert so UI handler owns it
        || key_l == "ins"
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
            // Ignore stray up without prior down
            return;
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

#[derive(Clone)]
struct IndicatorFade {
    start: Instant,
    dur_ms: u64,
    to_recording: bool,
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
    listening: bool,
    // indicator cross-fade state
    prev_recording: bool,
    ind_fade: Option<IndicatorFade>,
    scroll_selected_into_view: bool,
    ui_edit_active: bool,
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
            listening: true,
            prev_recording: false,
            ind_fade: None,
            scroll_selected_into_view: false,
            ui_edit_active: false,
        }
    }

    fn ingest(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            if !self.listening || self.ui_edit_active {
                // Exhaust stdin but ignore events while paused or when editing UI
                continue;
            }
            // Don't start a recording on stray 'Up' events (e.g., Enter release)
            if self.cur.is_none() && matches!(ev.kind, Kind::Up) {
                continue;
            }
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
                    // When a new recording starts, focus/select it & ensure it scrolls into view
                    self.selected_uid = Some(s.uid);
                    self.scroll_selected_into_view = true;
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


    fn force_finalize_now(&mut self) {
        if let Some(mut s) = self.cur.take() {
            // Compute visual elapsed since last event, but keep recorded times
            // anchored at the last event time for consistency with idle finalization.
            let vis_elapsed = self
                .last_event
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0);
            let now = s.now_ms; // do NOT add vis_elapsed to the stored data

            // Close any active holds at 'now'
            if !s.holds.is_empty() {
                let holds = std::mem::take(&mut s.holds);
                for (key, start) in holds {
                    if now == start {
                        s.taps.push(Tap { key, at: now });
                    } else {
                        s.segments.push(Segment { key, start, end: now });
                    }
                }
            }
            s.idle_start = None;

            // Only start a shrink animation if there isn't one already running.
            // Animate from visual-now -> logical-now (same as idle finalization).
            if self.anim_scale.is_none() {
                let from = (now.saturating_add(vis_elapsed)).max(1) as f32;
                let to = (now.max(1)) as f32;
                self.anim_scale = Some(ScaleAnim { start: Instant::now(), from, to, dur_ms: 220 });
                self.anim_target_uid = Some(s.uid);
            }

            let uid = s.uid;
            self.sequences.insert(0, s);
            // Keep selection on the same recording, now in history
            self.selected_uid = Some(uid);
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

#[allow(dead_code)]
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    // h in [0,1), s,v in [0,1]
    let h6 = (h * 6.0).fract();
    let i = (h * 6.0).floor() as i32;
    let f = h6;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    let (r, g, b) = match i.rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    (((r * 255.0).round() as u8), ((g * 255.0).round() as u8), ((b * 255.0).round() as u8))
}

fn draw_listening_ring(p: &egui::Painter, rect: Rect, listening: bool, time_secs: f32, alpha_mul: f32) {
    let size_min = rect.width().min(rect.height());
    let thickness = 3.0;
    let pix_pad = 0.5;
    let center = rect.center();
    let radius = size_min * 0.5 - pix_pad - thickness * 0.5;

    // Base ring (very dim when paused / subtle when listening)
    let base_alpha = if listening { 18 } else { 12 };
    p.circle_stroke(
        center,
        radius,
        Stroke::new(
            thickness,
            Color32::from_rgba_unmultiplied(255, 255, 255, ((base_alpha as f32) * alpha_mul).clamp(0.0, 255.0) as u8),
        ),
    );

    if !listening { return; }

    // One-sided comet arc: fully white at the head, smoothly fading to transparent at the tail.
    let rev_per_sec = 0.15;                  // rotation speed
    let head = (time_secs * rev_per_sec).fract() * TAU; // radians
    let arc_frac = 0.30;                     // fraction of full circle covered by the highlight
    let arc_ang = arc_frac * TAU;            // radians
    let tail = head - arc_ang;               // radians
    let steps: usize = 120;                  // smoothness along the arc

    // Mesh radii matching stroke thickness
    let r_inner = radius - thickness * 0.5;
    let r_outer = radius + thickness * 0.5;

    let mut mesh = egui::epaint::Mesh::default();

    for i in 0..=steps {
        let t = i as f32 / steps as f32;      // 0..1 from tail → head
        let a = tail + t * arc_ang;           // angle in radians
        let ca = a.cos();
        let sa = a.sin();

        let p_outer = Pos2::new(center.x + r_outer * ca, center.y + r_outer * sa);
        let p_inner = Pos2::new(center.x + r_inner * ca, center.y + r_inner * sa);

        // One-sided fade: 0 at tail → 1 at head, with gentle ease & perceptual gamma
        let s = t * t * (3.0 - 2.0 * t);      // smoothstep
        let g = s.powf(1.6);                  // gamma curve to avoid "flat" look
        let alpha = ((g * 235.0) * alpha_mul).clamp(0.0, 255.0) as u8;        // peak alpha near head
        let col = Color32::from_rgba_unmultiplied(255, 255, 255, alpha);

        mesh.vertices.push(egui::epaint::Vertex { pos: p_outer, uv: egui::pos2(0.0, 0.0), color: col });
        mesh.vertices.push(egui::epaint::Vertex { pos: p_inner, uv: egui::pos2(0.0, 0.0), color: col });
    }

    // Connect as a strip
    for i in 0..steps {
        let i0 = (2 * i) as u32;
        let i1 = i0 + 1;
        let i2 = i0 + 2;
        let i3 = i0 + 3;
        mesh.indices.extend_from_slice(&[i0, i2, i1, i1, i2, i3]);
    }

    p.add(egui::Shape::mesh(mesh));
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
) -> (egui::Response, bool) {
    // ---- Metrics ----
    let key_font = FontId::proportional(14.0);
    let label_pad = 20.0;
    let sel_w = 5.0;      // selection bar width (reserved in gutter)
    let sel_gap = 10.0;    // gap between selection bar and labels
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

    // ----- Delete cross (very top-left of the full record) -----
    let cross_sz = 14.0;
    let pad_left = 8.0; // horizontal padding from record border
    let pad_top  = 0.0; // tighter top padding to sit higher
    let cross_rect = Rect::from_min_size(Pos2::new(rect.left() + pad_left, rect.top() + pad_top), egui::vec2(cross_sz, cross_sz));
    let del_id = ui.id().with("del_x").with(seq.uid);
    let resp_cross = ui.interact(cross_rect, del_id, egui::Sense::click());
    resp_cross.clone().on_hover_cursor(CursorIcon::PointingHand);
    let pad = 3.0;
    let a = cross_rect.min + egui::vec2(pad, pad);
    let b = cross_rect.max - egui::vec2(pad, pad);
    let col = if ! resp_cross.hovered() { Color32::from_rgb(170, 170, 170) } else { Color32::from_rgb(220, 120, 120) };
    let sw = if resp_cross.hovered() { 2.0 } else { 1.5 };
    painter.line_segment([Pos2::new(a.x, a.y), Pos2::new(b.x, b.y)], Stroke::new(sw, col));
    painter.line_segment([Pos2::new(a.x, b.y), Pos2::new(b.x, a.y)], Stroke::new(sw, col));

    (resp, resp_cross.clicked())
}

// ===== eframe::App =====
impl eframe::App for AppState {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ingest();
        if !self.ui_edit_active { self.finalize_if_idle_elapsed(); }

        // Toggle listening with Insert key
        if !self.ui_edit_active && ctx.input(|i| i.key_pressed(Key::Insert)) {
            self.listening = !self.listening;
            if !self.listening {
                // When pausing, close the current recording immediately
                self.force_finalize_now();
            }
        }

        // Delete currently selected recording with Delete key (UI owns Delete only when paused)
        if !self.listening && ctx.input(|i| i.key_pressed(Key::Delete)) {
            if let Some(sel) = self.selected_uid {
                // If deleting the live one, finalize it first so it exists in history
                if self.cur.as_ref().map_or(false, |s| s.uid == sel) {
                    self.force_finalize_now();
                }
                if let Some(pos) = self.sequences.iter().position(|s| s.uid == sel) {
                    // Clear any running animation targeting this item
                    if self.anim_target_uid == Some(sel) {
                        self.anim_target_uid = None;
                        self.anim_scale = None;
                    }
                    // Remove it
                    self.sequences.remove(pos);
                    // Select neighbor: prefer the next item at same index; else previous
                    if !self.sequences.is_empty() {
                        let idx = if pos < self.sequences.len() { pos } else { self.sequences.len() - 1 };
                        self.selected_uid = Some(self.sequences[idx].uid);
                        self.scroll_selected_into_view = true;
                    } else {
                        self.selected_uid = None;
                    }
                }
            }
        }

        // If an animation finished, clear it
        if let Some(anim) = &self.anim_scale {
            if anim.start.elapsed() >= Duration::from_millis(anim.dur_ms) {
                self.anim_scale = None;
                self.anim_target_uid = None;
            }
        }

        // --- Indicator cross-fade (ring ↔ dot) ---
        let recording_now = self
            .cur
            .as_ref()
            .map(|s| !s.holds.is_empty() || self
                 .last_event
                 .map(|t| t.elapsed() < Duration::from_millis(self.gap_ms))
                 .unwrap_or(false))
            .unwrap_or(false);
        if self.prev_recording != recording_now {
            self.ind_fade = Some(IndicatorFade { start: Instant::now(), dur_ms: 60, to_recording: recording_now });
            self.prev_recording = recording_now;
        }
        let (dot_alpha, ring_alpha) = match self.ind_fade.take() {
            Some(f) => {
                let a = (f.start.elapsed().as_millis() as f32 / f.dur_ms as f32).clamp(0.0, 1.0);
                let a = smoothstep(a);
                if a >= 1.0 {
                    // fade finished; keep None
                    if f.to_recording { (1.0, 0.0) } else { (0.0, 1.0) }
                } else {
                    // not finished; put it back and interpolate
                    let to_rec = f.to_recording;
                    self.ind_fade = Some(f);
                    if to_rec { (a, 1.0 - a) } else { (1.0 - a, a) }
                }
            }
            None => {
                if recording_now { (1.0, 0.0) } else { (0.0, 1.0) }
            }
        };

        // Arrow-key navigation when paused
        if !self.listening {
            if self.selected_uid.is_none() {
                if let Some(first) = self.sequences.first() {
                    self.selected_uid = Some(first.uid);
                }
            }
            if let Some(sel) = self.selected_uid {
                if ctx.input(|i| i.key_pressed(Key::ArrowDown)) {
                    if let Some(idx) = self.sequences.iter().position(|s| s.uid == sel) {
                        if idx + 1 < self.sequences.len() {
                            self.selected_uid = Some(self.sequences[idx + 1].uid);
                            self.scroll_selected_into_view = true;
                        }
                    }
                }
                if ctx.input(|i| i.key_pressed(Key::ArrowUp)) {
                    if let Some(idx) = self.sequences.iter().position(|s| s.uid == sel) {
                        if idx > 0 {
                            self.selected_uid = Some(self.sequences[idx - 1].uid);
                            self.scroll_selected_into_view = true;
                        }
                    }
                }
            }
        }

        let mut pending_delete: Option<u64> = None;
        egui::CentralPanel::default().show(ctx, |ui| {

            ui.horizontal(|ui| {
                ui.heading("keyd live graph");

                // Indicator at far right: cross-faded red dot ↔ listening ring
                let icon_sz = 18.0_f32;
                let gap_icon = 8.0_f32;       // ring ↔ right edge
                let gap_text_icon = 0.0_f32;  // text ↔ ring
                let hint_text = if self.listening {
                    "press <Ins> or click to disable listening"
                } else {
                    "press <Ins> or click to enable listening"
                };
                let body_font = ui.style().text_styles.get(&egui::TextStyle::Body).cloned().unwrap_or(FontId::proportional(14.0));
                let hint_w = ctx.fonts(|f| {
                    f.layout_no_wrap(hint_text.to_string(), body_font.clone(), Color32::WHITE)
                        .size()
                        .x
                });
                let right_pad_ui = gap_icon;
                let space = (ui.available_width() - (hint_w + gap_text_icon + icon_sz + right_pad_ui)).max(0.0);
                ui.add_space(space);
                let hint_col = if self.listening { Color32::from_gray(170) } else { Color32::from_gray(160) };
                ui.label(egui::RichText::new(hint_text).color(hint_col));
                ui.add_space(gap_text_icon);

                let (r_icon, p_icon) = ui.allocate_painter(egui::vec2(icon_sz, icon_sz), egui::Sense::click());
                let t = ctx.input(|i| i.time) as f32;
                if ring_alpha > 0.001 {
                    draw_listening_ring(&p_icon, r_icon.rect, self.listening, t, ring_alpha);
                }
                if dot_alpha > 0.001 {
                    let size_min = r_icon.rect.width().min(r_icon.rect.height());
                    let pix_pad = 0.5;
                    let r = size_min * 0.5 - pix_pad; // match ring outer radius
                    let a = (dot_alpha * 255.0).clamp(0.0, 255.0) as u8;
                    p_icon.circle_filled(r_icon.rect.center(), r, Color32::from_rgba_unmultiplied(220, 50, 50, a));
                }
                // Toggle listening when clicking the ring/dot
                if r_icon.clicked() && !self.ui_edit_active {
                    self.listening = !self.listening;
                    if !self.listening {
                        self.force_finalize_now();
                    }
                }
                // Pointer hint
                r_icon.on_hover_cursor(CursorIcon::PointingHand);
                ui.add_space(right_pad_ui);
            });
            ui.horizontal(|ui| {
                ui.label("Records split on ≥");
                let mut gap_val = self.gap_ms as i64;
                let resp = ui.add(
                    egui::DragValue::new(&mut gap_val)
                        .speed(25.0)
                        .clamp_range(200..=2000)
                        .suffix(" ms")
                        .fixed_decimals(0)
                );

                // While this widget is focused, temporarily pause ingest/finalize without changing UI indicators
                self.ui_edit_active = resp.has_focus();

                // Mouse wheel over the control adjusts by ±50 ms per notch
                let wheel = ui.input(|i| i.raw_scroll_delta.y);
                let mut changed = resp.changed();
                if resp.hovered() && wheel.abs() > 0.0 {
                    let step: i64 = 50;
                    let dir: i64 = if wheel > 0.0 { -1 } else if wheel < 0.0 { 1 } else { 0 };
                    if dir != 0 {
                        gap_val = (gap_val + dir * step).clamp(50, 2000);
                        changed = true;
                    }
                }

                if changed {
                    self.gap_ms = gap_val as u64;
                }

                ui.label("gaps. Thick line = hold, dot = tap.");
            });
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
                            let (resp, del_clicked) = draw_sequence_block(ctx, ui, s, is_live, anim, self.last_event, is_selected);
                            if is_selected && self.scroll_selected_into_view {
                                resp.scroll_to_me(None);
                                self.scroll_selected_into_view = false;
                            }
                            if del_clicked {
                                pending_delete = Some(s.uid);
                            } else if resp.clicked() {
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

        // Apply pending deletion from UI cross
        if let Some(uid) = pending_delete {
            if self.cur.as_ref().map_or(false, |s| s.uid == uid) {
                self.force_finalize_now();
            }
            if let Some(pos) = self.sequences.iter().position(|s| s.uid == uid) {
                if self.anim_target_uid == Some(uid) {
                    self.anim_target_uid = None;
                    self.anim_scale = None;
                }
                self.sequences.remove(pos);
                if !self.sequences.is_empty() {
                    let idx = if pos < self.sequences.len() { pos } else { self.sequences.len() - 1 };
                    self.selected_uid = Some(self.sequences[idx].uid);
                    self.scroll_selected_into_view = true;
                } else {
                    self.selected_uid = None;
                }
            }
        }

        // ~60 FPS
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

// ===== main =====
fn main() -> Result<()> {
    // Parse --gap-ms <u64>
    let mut gap_ms: u64 = 500;
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
    eframe::run_native("keyd live graph", native_options, Box::new(move |_| Box::new(app)))
        .map_err(|e| anyhow!("eframe error: {e}"))
}
