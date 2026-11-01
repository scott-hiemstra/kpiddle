//! Pods as bubbles: what each bubble is, how pods group in each layout,
//! search, and rendering. The layout simulation is `piddle_scene`,
//! shared with PIDdle.
//!
//! Size is the memory working set and color is CPU, where 1000m (one full
//! core) is red. Usage that wasn't measured is never drawn as an idle
//! bubble: a pod without a sample is a hollow gray ring at a fixed minimal
//! size, and a stale sample is drawn dimmed.

use std::collections::HashMap;
use std::sync::Arc;

use gtk::prelude::*;
use gtk::{gsk, pango};

use kpiddle_core::model::{Measurement, Phase, PodView, Snapshot, Total};
use piddle_encode::{MIN_RADIUS, Rgb, cpu_color, mem_radius, mix};
use piddle_gtk::Paint;
use piddle_gtk::draw::{card, font, glib_escape, layout, point, rect, rgb, rgba, ring, text};
pub use piddle_scene::Motion;
use piddle_scene::{Arrangement, GroupSpec, Item, Scene};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Workload,
    Namespace,
    Node,
    Free,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Workload, Layout::Namespace, Layout::Node, Layout::Free];

    pub fn name(self) -> &'static str {
        match self {
            Layout::Workload => "By workload",
            Layout::Namespace => "By namespace",
            Layout::Node => "By node",
            Layout::Free => "Free",
        }
    }

    pub fn parse(s: &str) -> Option<Layout> {
        match s {
            "workload" => Some(Layout::Workload),
            "namespace" => Some(Layout::Namespace),
            "node" => Some(Layout::Node),
            "free" => Some(Layout::Free),
            _ => None,
        }
    }

    fn arrangement(self) -> Arrangement {
        if self == Layout::Free { Arrangement::Free } else { Arrangement::Grouped }
    }
}

/// A pod as a bubble.
pub struct PodItem {
    pub pod: PodView,
    /// "namespace/workload": pods of one workload start next to each other.
    affinity: String,
}

impl PodItem {
    fn usage_cpu(&self) -> Option<f64> {
        self.pod.usage.usage().map(|u| u.cpu_millis)
    }
}

#[derive(Clone)]
pub struct GroupInfo {
    title: String,
    /// "deployment", "namespace", "node".
    kind: String,
    total: Total,
}

pub struct LabelCache {
    key: (i32, i32),
    name: pango::Layout,
    value: pango::Layout,
}

impl Item for PodItem {
    type Key = Arc<str>;
    type Group = GroupInfo;
    type Cache = Option<LabelCache>;

    fn key(&self) -> Arc<str> {
        self.pod.pod.uid.clone()
    }
    /// UIDs are never reused.
    fn same_instance(&self, _: &Self) -> bool {
        true
    }
    fn parent(&self) -> Option<Arc<str>> {
        None
    }
    /// Unmeasured pods get a fixed minimal size rather than borrowing
    /// requests (that is a separate, labeled mode).
    fn radius(&self) -> f64 {
        self.pod.usage.usage().map_or(MIN_RADIUS, |u| mem_radius(u.mem_bytes))
    }
    /// PIDdle's scale: 100 is one core.
    fn cpu(&self) -> f64 {
        self.usage_cpu().map_or(0.0, |m| m / 10.0)
    }
    fn mem(&self) -> u64 {
        self.pod.usage.usage().map_or(0, |u| u.mem_bytes)
    }
    fn affinity(&self) -> &str {
        &self.affinity
    }
}

pub type Bubble = piddle_scene::Bubble<PodItem>;

pub struct View {
    pub scene: Scene<PodItem>,
    pub layout: Layout,
    query: String,
    pub snap: Option<Snapshot>,
    /// Totals per group key in the current layout, from the model (so
    /// partial and unmeasured groups read as such).
    totals: HashMap<Arc<str>, Total>,
}

fn group_key(layout: Layout, p: &PodView) -> (Arc<str>, String, String) {
    match layout {
        Layout::Workload => {
            let w = &p.workload;
            let key = format!("{}/{}/{}", p.pod.namespace, w.kind.label(), w.name);
            (key.into(), format!("{}/{}", p.pod.namespace, w.name), w.kind.label().to_string())
        }
        Layout::Namespace => (p.pod.namespace.clone(), p.pod.namespace.to_string(), "namespace".into()),
        Layout::Node => {
            let node = p.pod.node.clone().unwrap_or_else(|| Arc::from("(not scheduled)"));
            (node.clone(), node.to_string(), "node".into())
        }
        Layout::Free => (Arc::from(""), String::new(), String::new()),
    }
}

impl View {
    pub fn new(layout: Layout) -> Self {
        let mut scene = Scene::new();
        scene.set_arrangement(layout.arrangement(), |_: &PodItem| GroupSpec {
            key: Arc::from(""),
            info: GroupInfo { title: String::new(), kind: String::new(), total: Total::default() },
        });
        Self { scene, layout, query: String::new(), snap: None, totals: HashMap::new() }
    }

    pub fn bubble(&self, uid: &Arc<str>) -> Option<&Bubble> {
        self.scene.bubble(uid)
    }

    pub fn apply(&mut self, snap: Snapshot) {
        let items = snap
            .pods
            .iter()
            .map(|p| PodItem {
                affinity: format!("{}/{}/{}", p.pod.namespace, p.workload.kind.label(), p.workload.name),
                pod: p.clone(),
            })
            .collect();
        self.snap = Some(snap);
        self.recompute_totals();
        let (layout, totals) = (self.layout, &self.totals);
        self.scene.sync(items, |it| spec(layout, totals, it));
        self.refresh_matches();
    }

    fn recompute_totals(&mut self) {
        self.totals.clear();
        let Some(snap) = &self.snap else { return };
        for p in &snap.pods {
            let (key, ..) = group_key(self.layout, p);
            self.totals.entry(key).or_default().add(&p.usage);
        }
    }

    pub fn set_layout(&mut self, l: Layout) {
        self.layout = l;
        self.recompute_totals();
        let totals = &self.totals;
        self.scene.set_arrangement(l.arrangement(), |it| spec(l, totals, it));
    }

    pub fn set_query(&mut self, q: &str) {
        self.query = q.trim().to_lowercase();
        self.refresh_matches();
    }

    pub fn has_query(&self) -> bool {
        !self.query.is_empty()
    }

    fn refresh_matches(&mut self) {
        let q = self.query.as_str();
        self.scene.set_matches(!q.is_empty(), |it| {
            let p = &it.pod;
            [&p.pod.name, &p.pod.namespace, &p.workload.name].iter().any(|s| s.to_lowercase().contains(q))
                || p.pod.node.as_ref().is_some_and(|n| n.to_lowercase().contains(q))
        });
    }

    pub fn step(&mut self, dt: f64) -> Motion {
        self.scene.step(dt)
    }

    pub fn pick(&self, sx: f64, sy: f64) -> Option<Arc<str>> {
        self.scene.pick(sx, sy)
    }

    // ------------------------------------------------------------------ drawing

    pub fn render(&mut self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let (w, h) = self.scene.size;
        let full = rect(0.0, 0.0, w, h);
        snap.append_radial_gradient(
            &full,
            &point(w / 2.0, h / 2.0),
            (w * 0.7) as f32,
            (h * 0.9) as f32,
            0.0,
            1.0,
            &[gsk::ColorStop::new(0.0, rgb(0.085, 0.10, 0.135)), gsk::ColorStop::new(1.0, rgb(0.045, 0.05, 0.068))],
        );

        if self.scene.bubbles.is_empty() {
            let msg = match &self.snap {
                None => "Connecting…",
                Some(s) if s.pods_state == kpiddle_core::model::PodsState::Listing => "Listing pods…",
                Some(_) => "No pods",
            };
            let l = layout(pctx, msg, 15.0, false);
            let (lw, lh) = l.pixel_size();
            text(snap, &l, (w - lw as f64) / 2.0, (h - lh as f64) / 2.0, rgba(1.0, 1.0, 1.0, 0.5));
            return;
        }

        let s = self.scene.scale();
        let dimming = self.scene.has_query();

        // Group hulls.
        if self.layout != Layout::Free {
            let pb = gsk::PathBuilder::new();
            for g in self.scene.groups.iter().filter(|g| g.members >= 2) {
                let (gx, gy) = self.scene.to_screen(g.cx, g.cy);
                pb.add_circle(&point(gx, gy), (g.hull * s) as f32);
            }
            let path = pb.to_path();
            snap.append_fill(&path, gsk::FillRule::Winding, &rgba(0.6, 0.7, 0.9, 0.03));
            let stroke = gsk::Stroke::new(1.0);
            stroke.set_dash(&[4.0, 4.0]);
            snap.append_stroke(&path, &stroke, &rgba(0.6, 0.7, 0.9, 0.22));
        }

        let hover = self.scene.hover.clone();
        for i in 0..self.scene.bubbles.len() {
            let (sx, sy) = {
                let b = &self.scene.bubbles[i];
                self.scene.to_screen(b.x, b.y)
            };
            let b = &mut self.scene.bubbles[i];
            let sr = b.r * s;
            if sr < 0.5 || sx + sr < 0.0 || sy + sr < 0.0 || sx - sr > w || sy - sr > h {
                continue;
            }
            let dim = if dimming && !b.matched { 0.16 } else { 1.0 };
            let a = b.alpha * dim;
            let usage = b.item.pod.usage;

            match usage {
                Measurement::Missing | Measurement::NotRunning => {
                    // Hollow: no sample, so no fill that could pass for idle.
                    let (c, dash) = if usage == Measurement::Missing { (0.62, 3.0) } else { (0.45, 1.5) };
                    let path = {
                        let pb = gsk::PathBuilder::new();
                        pb.add_circle(&point(sx, sy), (sr - 0.75).max(0.5) as f32);
                        pb.to_path()
                    };
                    let stroke = gsk::Stroke::new(1.5);
                    stroke.set_dash(&[dash as f32, 2.5]);
                    snap.append_stroke(&path, &stroke, &rgba(c, c + 0.03, c + 0.08, 0.85 * a));
                }
                Measurement::Fresh(_) | Measurement::Stale { .. } => {
                    let stale = matches!(usage, Measurement::Stale { .. });
                    let mut c = cpu_color(b.cpu);
                    if stale {
                        c = mix(c, Rgb(0.45, 0.47, 0.52), 0.65);
                    }
                    let a = if stale { a * 0.6 } else { a };
                    if !stale && b.cpu > 35.0 && sr > 3.0 {
                        let t = ((b.cpu - 35.0) / 65.0).clamp(0.0, 1.0);
                        let gr = sr * 1.45;
                        snap.append_radial_gradient(
                            &rect(sx - gr, sy - gr, gr * 2.0, gr * 2.0),
                            &point(sx, sy),
                            gr as f32,
                            gr as f32,
                            0.0,
                            1.0,
                            &[
                                gsk::ColorStop::new(0.0, rgba(c.0, c.1, c.2, 0.0)),
                                gsk::ColorStop::new(0.55, rgba(c.0, c.1, c.2, 0.45 * t * a)),
                                gsk::ColorStop::new(1.0, rgba(c.0, c.1, c.2, 0.0)),
                            ],
                        );
                    }
                    let hi = mix(c, Rgb(1.0, 1.0, 1.0), 0.35);
                    let lo = mix(c, Rgb(0.0, 0.0, 0.0), 0.25);
                    let bounds = rect(sx - sr, sy - sr, sr * 2.0, sr * 2.0);
                    let round = gsk::RoundedRect::from_rect(bounds, sr as f32);
                    snap.push_rounded_clip(&round);
                    if sr < 4.0 {
                        snap.append_color(&rgba(c.0, c.1, c.2, 0.95 * a), &bounds);
                    } else {
                        snap.append_radial_gradient(
                            &bounds,
                            &point(sx - sr * 0.35, sy - sr * 0.4),
                            (sr * 1.35) as f32,
                            (sr * 1.35) as f32,
                            0.0,
                            1.0,
                            &[
                                gsk::ColorStop::new(0.0, rgba(hi.0, hi.1, hi.2, 0.96 * a)),
                                gsk::ColorStop::new(1.0, rgba(lo.0, lo.1, lo.2, 0.92 * a)),
                            ],
                        );
                    }
                    snap.pop();
                    if sr >= 4.0 {
                        ring(snap, sx, sy, sr, 1.0, rgba(hi.0, hi.1, hi.2, 0.5 * a));
                    }
                }
            }

            // A pod in trouble (CrashLoopBackOff, OOMKilled, ...) gets a red outline.
            if b.item.pod.pod.reason.is_some() || b.item.pod.pod.phase == Phase::Failed {
                ring(snap, sx, sy, sr + 2.0, 2.0, rgba(0.94, 0.3, 0.3, 0.9 * b.alpha));
            }
            if dimming && b.matched {
                ring(snap, sx, sy, sr + 4.5, 2.0, rgba(1.0, 1.0, 1.0, 0.9 * b.alpha));
            }
            if hover.as_ref() == Some(&b.item.pod.pod.uid) && !b.dying {
                ring(snap, sx, sy, sr + 2.5, 2.5, rgba(1.0, 1.0, 1.0, 0.95));
            }
            if sr >= 15.0 {
                draw_label(snap, pctx, b, sx, sy, sr, a);
            }
        }

        if self.layout != Layout::Free {
            self.draw_group_labels(snap, pctx);
        }
        self.draw_hover_card(snap, pctx);
    }

    fn draw_group_labels(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let s = self.scene.scale();
        for g in self.scene.groups.iter().filter(|g| g.members >= 2) {
            let (gx, gy) = self.scene.to_screen(g.cx, g.cy);
            let gr = g.hull * s;
            if gr < 30.0 {
                continue;
            }
            let t = self.totals.get(&g.key).copied().unwrap_or(g.info.total);
            let mut detail = format!("{} · ×{} · {} · {}", g.info.kind, g.members, t.cpu_text(), t.mem_text());
            // Short form, so it survives on small hulls: it explains the ≥.
            if t.measured < t.expected {
                detail.push_str(&format!(" · {}/{} measured", t.measured, t.expected));
            }
            let markup =
                format!("<b>{}</b>  <span alpha='65%'>{}</span>", glib_escape(&g.info.title), glib_escape(&detail));
            let l = pango::Layout::new(pctx);
            l.set_font_description(Some(&font(11.0, false)));
            l.set_markup(&markup);
            l.set_width((gr * 2.0).max(340.0) as i32 * pango::SCALE);
            l.set_ellipsize(pango::EllipsizeMode::End);
            let (lw, lh) = l.pixel_size();
            let (lw, lh) = (lw as f64, lh as f64);
            let x = gx - lw / 2.0;
            let y = (gy - gr - lh / 2.0).max(4.0);
            let border = if t.is_partial() || t.is_unmeasured() || t.is_stale() {
                rgba(0.94, 0.63, 0.24, 0.55)
            } else {
                rgba(0.6, 0.7, 0.9, 0.3)
            };
            card(snap, x - 8.0, y - 3.0, lw + 16.0, lh + 6.0, (lh + 6.0) / 2.0, rgba(0.09, 0.105, 0.14, 0.92), border);
            text(snap, &l, x, y, rgba(0.86, 0.89, 0.95, 0.95));
        }
    }

    fn draw_hover_card(&self, snap: &gtk::Snapshot, pctx: &pango::Context) {
        let Some(b) = self.scene.hover.as_ref().and_then(|k| self.bubble(k)) else { return };
        let p = &b.item.pod;
        let usage = match &p.usage {
            Measurement::Fresh(_) => String::new(),
            Measurement::Stale { .. } => {
                format!("\n<span foreground='#f0a03c'>stale: sample {}</span>", p.usage.age_text().unwrap_or_default())
            }
            Measurement::Missing => "\n<span foreground='#f0a03c'>no usage sample from metrics.k8s.io</span>".into(),
            Measurement::NotRunning => "\n<span alpha='60%'>not running, so no usage</span>".into(),
        };
        let status = match &p.pod.reason {
            Some(r) => format!("<span foreground='#f05a5a'>{}</span>", glib_escape(r)),
            None => p.pod.phase.label().to_string(),
        };
        let restarts = p.pod.restarts();
        let markup = format!(
            "<span size='large' weight='bold'>{}</span>\n\
             <span alpha='65%'>{} · {}\nnode {}</span>\n\n\
             Status\t\t{}{}\n\
             CPU\t\t<b>{}</b>\n\
             Memory\t<b>{}</b>{}",
            glib_escape(&p.pod.name),
            glib_escape(&p.pod.namespace),
            glib_escape(&p.workload.describe()),
            glib_escape(p.pod.node.as_deref().unwrap_or("—")),
            status,
            if restarts > 0 { format!(" · {restarts} restarts") } else { String::new() },
            glib_escape(&p.usage.cpu_text()),
            glib_escape(&p.usage.mem_text()),
            usage,
        );
        let l = pango::Layout::new(pctx);
        l.set_font_description(Some(&font(12.5, false)));
        l.set_markup(&markup);
        let (lw, lh) = l.pixel_size();
        let pad = 12.0;
        let (cw, ch) = (lw as f64 + pad * 2.0 + 6.0, lh as f64 + pad * 2.0);
        let (w, h) = self.scene.size;
        let (mx, my) = self.scene.mouse;
        let mut x = mx + 18.0;
        let mut y = my + 18.0;
        if x + cw > w - 6.0 {
            x = mx - cw - 14.0;
        }
        if y + ch > h - 6.0 {
            y = (h - ch - 6.0).max(6.0);
        }
        x = x.max(6.0);

        card(snap, x, y, cw, ch, 10.0, rgba(0.075, 0.085, 0.11, 0.96), rgba(1.0, 1.0, 1.0, 0.12));
        let c = match p.usage {
            Measurement::Fresh(_) => cpu_color(b.cpu),
            _ => Rgb(0.5, 0.52, 0.58),
        };
        card(snap, x + 6.0, y + pad, 3.0, ch - pad * 2.0, 1.5, rgb(c.0, c.1, c.2), rgb(c.0, c.1, c.2));
        text(snap, &l, x + pad + 6.0, y + pad, rgba(0.93, 0.95, 0.98, 1.0));
    }
}

fn spec(layout: Layout, totals: &HashMap<Arc<str>, Total>, it: &PodItem) -> GroupSpec<GroupInfo> {
    let (key, title, kind) = group_key(layout, &it.pod);
    let total = totals.get(&key).copied().unwrap_or_default();
    GroupSpec { key, info: GroupInfo { title, kind, total } }
}

/// Pod name and its CPU (or `—`) inside the bubble.
fn draw_label(snap: &gtk::Snapshot, pctx: &pango::Context, b: &mut Bubble, sx: f64, sy: f64, sr: f64, a: f64) {
    let px = (sr * 0.3).clamp(9.0, 14.0).round() as i32;
    let width = (sr * 1.65 / 4.0).round() as i32 * 4;
    let key = (px, width);
    let value_text = b.item.pod.usage.cpu_text();
    let stale_label = b.cache.as_ref().is_some_and(|l| l.value.text() != value_text);
    if stale_label || b.cache.as_ref().map(|l| l.key) != Some(key) {
        let name = layout(pctx, &b.item.pod.pod.name, px as f64, true);
        name.set_width(width * pango::SCALE);
        name.set_ellipsize(pango::EllipsizeMode::Middle);
        name.set_alignment(pango::Alignment::Center);
        let value = layout(pctx, &value_text, (px as f64 * 0.85).max(8.0), false);
        value.set_width(width * pango::SCALE);
        value.set_alignment(pango::Alignment::Center);
        b.cache = Some(LabelCache { key, name, value });
    }
    let lc = b.cache.as_ref().unwrap();
    let (_, nh) = lc.name.pixel_size();
    let (_, vh) = lc.value.pixel_size();
    let show_value = sr >= 22.0 && !value_text.is_empty();
    let total = nh as f64 + if show_value { vh as f64 } else { 0.0 };
    let x = sx - width as f64 / 2.0;
    let y = sy - total / 2.0;
    let filled = matches!(b.item.pod.usage, Measurement::Fresh(_));
    let c = cpu_color(b.cpu);
    // Dark text on bright (yellow) bubbles, light text with a soft shadow elsewhere.
    let dark = filled && 0.299 * c.0 + 0.587 * c.1 + 0.114 * c.2 > 0.66;
    let lines: &[(&pango::Layout, f64, f64)] =
        if show_value { &[(&lc.name, y, 1.0), (&lc.value, y + nh as f64, 0.75)] } else { &[(&lc.name, y, 1.0)] };
    for &(l, yy, alpha) in lines {
        if dark {
            text(snap, l, x, yy, rgba(0.07, 0.07, 0.09, alpha * a));
        } else {
            text(snap, l, x, yy + 1.0, rgba(0.0, 0.0, 0.0, 0.45 * a));
            text(snap, l, x, yy, rgba(1.0, 1.0, 1.0, alpha * a * if filled { 1.0 } else { 0.7 }));
        }
    }
}

// Drawing helpers, as in piddle-gui's view.

impl Paint for View {
    fn resize(&mut self, width: f64, height: f64) {
        self.scene.size = (width, height);
    }

    fn paint(&mut self, snapshot: &gtk::Snapshot, pctx: &pango::Context) {
        self.render(snapshot, pctx);
    }
}
