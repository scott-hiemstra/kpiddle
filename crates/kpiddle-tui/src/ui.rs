//! Rendering. CPU color follows PIDdle: cool blue through yellow to hot red,
//! where 1000m (one full core) is red. Usage that wasn't measured is never
//! drawn as zero: it reads `—` in gray, and stale values are dimmed.

use std::time::{Duration, SystemTime};

use kpiddle_core::model::{Measurement, MetricsState, PodView, PodsState, Resources, Total, fmt_age, fmt_cpu};
use piddle_view::encode::{cpu_color, fmt_bytes};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Padding, Paragraph, Row as TRow, Table, Wrap};

use crate::app::{App, Mode, Row};
use crate::metrics_text;

const DIM: Color = Color::Rgb(130, 140, 160);
const ACCENT: Color = Color::Rgb(242, 201, 76);
const BORDER: Color = Color::Rgb(60, 68, 84);
const SELECT_BG: Color = Color::Rgb(38, 46, 64);
const GOOD: Color = Color::Rgb(120, 200, 160);
const WARN: Color = Color::Rgb(240, 160, 60);
const BAD: Color = Color::Rgb(240, 90, 90);
const MEM: Color = Color::Rgb(90, 140, 255);

/// Map an RGB color to what the terminal supports. (A copy of piddle-tui's;
/// worth sharing once a second Ratatui consumer needs more of it.)
pub struct Palette {
    truecolor: bool,
}

impl Palette {
    pub fn detect() -> Self {
        let ct = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
        Self { truecolor: ct.contains("truecolor") || ct.contains("24bit") }
    }

    fn rgb(&self, r: u8, g: u8, b: u8) -> Color {
        if self.truecolor {
            return Color::Rgb(r, g, b);
        }
        // Nearest entry in the xterm 6x6x6 color cube.
        let q = |v: u8| ((v as u16 * 5 + 127) / 255) as u8;
        Color::Indexed(16 + 36 * q(r) + 6 * q(g) + q(b))
    }

    fn fix(&self, c: Color) -> Color {
        match c {
            Color::Rgb(r, g, b) => self.rgb(r, g, b),
            other => other,
        }
    }

    /// Millicores to PIDdle's scale, where 100% is one core.
    fn cpu(&self, millis: f64) -> Color {
        let c = cpu_color(millis / 10.0);
        self.rgb((c.0 * 255.0) as u8, (c.1 * 255.0) as u8, (c.2 * 255.0) as u8)
    }
}

pub fn draw(f: &mut Frame, app: &mut App, pal: &Palette) {
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    draw_header(f, app, pal, header);
    draw_table(f, app, pal, body);
    draw_footer(f, app, pal, footer);
    match app.mode {
        Mode::Details => draw_details(f, app, pal),
        Mode::Help => draw_help(f, pal),
        _ => {}
    }
}

fn draw_header(f: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let [l1, l2, l3] = Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let dim = Style::new().fg(pal.fix(DIM));
    let scope = app.namespace.as_ref().map_or("all namespaces".to_string(), |n| format!("namespace {n}"));
    let mut title = vec![
        Span::styled(" k", Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
        Span::styled("PID", Style::new().add_modifier(Modifier::BOLD)),
        Span::styled("dle", Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
        Span::styled(format!("   context {} · {scope}", app.context), dim),
    ];
    if let Some(s) = &app.snap {
        title.push(Span::styled(
            format!(" · {} pods · view: {} · sort: {}", s.pods.len(), app.view.name(), app.sort.name()),
            dim,
        ));
    }
    f.render_widget(Paragraph::new(Line::from(title)), l1);

    let Some(s) = &app.snap else {
        f.render_widget(Paragraph::new(" Connecting…").style(dim), l2);
        return;
    };
    let color = match s.metrics {
        MetricsState::Waiting => DIM,
        MetricsState::Ok => GOOD,
        MetricsState::Stale { .. } => WARN,
        MetricsState::Unavailable { .. } => BAD,
    };
    let mut status = vec![
        Span::styled(" metrics ", dim),
        Span::styled(metrics_text(&s.metrics), Style::new().fg(pal.fix(color)).add_modifier(Modifier::BOLD)),
    ];
    match &s.pods_state {
        PodsState::Listing => status.push(Span::styled("   pods: listing…", dim)),
        PodsState::Failing { reason } => status.push(Span::styled(
            format!("   pods: watch failing, list may be out of date: {reason}"),
            Style::new().fg(pal.fix(BAD)),
        )),
        PodsState::Synced => {}
    }
    f.render_widget(Paragraph::new(Line::from(status)), l2);

    let c = &s.cluster;
    let mut totals = vec![
        Span::styled(" CPU ", dim),
        total_span(c, c.cpu_text(), pal.cpu(c.usage.cpu_millis), pal),
        Span::styled("   memory ", dim),
        total_span(c, c.mem_text(), pal.fix(MEM), pal),
    ];
    if let Some(cov) = c.coverage_text() {
        totals.push(Span::styled(format!("   {cov}"), Style::new().fg(pal.fix(WARN))));
    }
    if s.suggest_requests_mode() {
        totals.push(Span::styled(
            "   no running pod has usage, so none is shown (a labeled requests/limits mode is planned)",
            dim,
        ));
    }
    f.render_widget(Paragraph::new(Line::from(totals)), l3);
}

/// A total: bold in `color` when complete, warning-colored when partial or
/// stale, gray when nothing was measured.
fn total_span(t: &Total, text: String, color: Color, pal: &Palette) -> Span<'static> {
    let style = if t.is_unmeasured() || t.expected == 0 {
        Style::new().fg(pal.fix(DIM))
    } else if t.is_partial() || t.is_stale() {
        Style::new().fg(pal.fix(WARN)).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    };
    Span::styled(text, style)
}

/// A bar for CPU where one core fills it, with `▶` for more than a core.
/// Unmeasured usage gets a hollow bar, never an empty (idle-looking) one.
fn cpu_bar(millis: Option<f64>, width: usize) -> String {
    let Some(m) = millis else { return "╌".repeat(width) };
    let filled = ((m / 1000.0).clamp(0.0, 1.0) * width as f64).round() as usize;
    let mut s = "▮".repeat(filled) + &"·".repeat(width - filled);
    if m > 1000.0 {
        s.pop();
        s.push('▶');
    }
    s
}

fn draw_table(f: &mut Frame, app: &mut App, pal: &Palette, area: Rect) {
    let header = TRow::new(["", "NAME", "CPU", "", "MEMORY", "MEASURED", "STATUS"])
        .style(Style::new().fg(pal.fix(DIM)).add_modifier(Modifier::BOLD));
    let dim = Style::new().fg(pal.fix(DIM));
    let rows: Vec<TRow> = match &app.snap {
        None => Vec::new(),
        Some(snap) => app
            .rows
            .iter()
            .map(|r| match r {
                Row::Namespace { name, total, pods, collapsed } => group_row(
                    *collapsed,
                    vec![
                        Span::styled(name.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  ×{pods}"), dim),
                    ],
                    total,
                    pal,
                )
                .style(Style::new().bg(pal.fix(Color::Rgb(22, 26, 34)))),
                Row::Workload { workload, total, pods, collapsed, .. } => group_row(
                    *collapsed,
                    vec![
                        Span::raw("  "),
                        Span::styled(format!("{} ", workload.kind.label()), dim),
                        Span::styled(workload.name.to_string(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  ×{pods}"), dim),
                    ],
                    total,
                    pal,
                ),
                Row::Pod { idx, depth } => pod_row(&snap.pods[*idx], *depth, pal),
            })
            .collect(),
    };

    let title = if app.query.is_empty() {
        Line::from(format!(" {} rows ", app.rows.len()))
    } else {
        Line::from(vec![
            Span::raw(" search: "),
            Span::styled(app.query.clone(), Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
            Span::raw(format!(" · {} rows ", app.rows.len())),
        ])
    };
    let widths = [
        Constraint::Length(1),
        Constraint::Fill(2),
        Constraint::Length(12),
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Length(9),
        Constraint::Fill(1),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(pal.fix(BORDER)))
                .title(title),
        )
        .row_highlight_style(Style::new().bg(pal.fix(SELECT_BG)).add_modifier(Modifier::BOLD))
        .column_spacing(1);
    f.render_stateful_widget(table, area, &mut app.table);
}

fn group_row<'a>(collapsed: bool, name: Vec<Span<'a>>, t: &Total, pal: &Palette) -> TRow<'a> {
    let cpu_color = pal.cpu(t.usage.cpu_millis);
    let measured = (t.measured > 0).then_some(t.usage.cpu_millis);
    // Coverage only where it qualifies the total.
    let coverage = if t.measured == t.expected {
        Span::raw("")
    } else {
        let color = if t.is_unmeasured() { BAD } else { WARN };
        Span::styled(format!("{}/{}", t.measured, t.expected), Style::new().fg(pal.fix(color)))
    };
    let status = if t.is_stale() {
        Span::styled(format!("{} stale", t.stale), Style::new().fg(pal.fix(WARN)))
    } else {
        Span::raw("")
    };
    let bar_style = if measured.is_some() { Style::new().fg(cpu_color) } else { Style::new().fg(pal.fix(BORDER)) };
    TRow::new(vec![
        Cell::from(if collapsed { "▸" } else { "▾" }),
        Cell::from(Line::from(name)),
        Cell::from(total_span(t, t.cpu_text(), cpu_color, pal)),
        Cell::from(Span::styled(cpu_bar(measured, 10), bar_style)),
        Cell::from(total_span(t, t.mem_text(), pal.fix(MEM), pal)),
        Cell::from(coverage),
        Cell::from(status),
    ])
}

fn pod_row<'a>(p: &PodView, depth: u8, pal: &Palette) -> TRow<'a> {
    let dim = Style::new().fg(pal.fix(DIM));
    let (cpu_style, mem_style, bar) = match p.usage {
        Measurement::Fresh(u) => (Style::new().fg(pal.cpu(u.cpu_millis)), Style::new(), Some(u.cpu_millis)),
        Measurement::Stale { usage, .. } => (dim.add_modifier(Modifier::ITALIC), dim, Some(usage.cpu_millis)),
        Measurement::Missing | Measurement::NotRunning => (dim, dim, None),
    };
    let bar_style = match p.usage {
        Measurement::Fresh(u) => Style::new().fg(pal.cpu(u.cpu_millis)),
        Measurement::Stale { .. } => dim,
        _ => Style::new().fg(pal.fix(BORDER)),
    };
    let bar_text = if p.usage == Measurement::NotRunning { String::new() } else { cpu_bar(bar, 10) };

    let mut status = vec![phase_span(p, pal)];
    let restarts = p.pod.restarts();
    if restarts > 0 {
        status.push(Span::styled(format!("  ↻{restarts}"), Style::new().fg(pal.fix(WARN))));
    }
    match &p.usage {
        Measurement::Stale { .. } => {
            status.push(Span::styled(format!("  usage {}", p.usage.age_text().unwrap_or_default()), dim))
        }
        Measurement::Missing => status.push(Span::styled("  no usage sample", dim)),
        _ => {}
    }
    TRow::new(vec![
        Cell::from(""),
        Cell::from(Line::from(vec![Span::raw("  ".repeat(depth as usize)), Span::raw(p.pod.name.to_string())])),
        Cell::from(Span::styled(p.usage.cpu_text(), cpu_style)),
        Cell::from(Span::styled(bar_text, bar_style)),
        Cell::from(Span::styled(p.usage.mem_text(), mem_style)),
        Cell::from(""),
        Cell::from(Line::from(status)),
    ])
}

/// Phase, or the reason that matters more ("CrashLoopBackOff").
fn phase_span(p: &PodView, pal: &Palette) -> Span<'static> {
    match &p.pod.reason {
        Some(r) => Span::styled(r.to_string(), Style::new().fg(pal.fix(BAD))),
        None => {
            let color = match p.pod.phase {
                kpiddle_core::model::Phase::Running => DIM,
                kpiddle_core::model::Phase::Succeeded => BORDER,
                kpiddle_core::model::Phase::Pending => WARN,
                _ => BAD,
            };
            Span::styled(p.pod.phase.label(), Style::new().fg(pal.fix(color)))
        }
    }
}

fn draw_footer(f: &mut Frame, app: &App, pal: &Palette, area: Rect) {
    let key = |k: &'static str| Span::styled(k, Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD));
    let txt = |t: &'static str| Span::styled(t, Style::new().fg(pal.fix(DIM)));
    let line = if let Some((msg, at)) = &app.status
        && at.elapsed() < Duration::from_secs(4)
    {
        Line::from(Span::styled(format!(" {msg}"), Style::new().fg(pal.fix(ACCENT))))
    } else if matches!(app.mode, Mode::Search) {
        Line::from(vec![txt(" type to filter · "), key("Enter"), txt(" keep · "), key("Esc"), txt(" clear")])
    } else {
        Line::from(vec![
            key(" ↑↓"),
            txt(" move  "),
            key("Enter"),
            txt(" details/collapse  "),
            key("/"),
            txt(" search  "),
            key("v"),
            txt(" view  "),
            key("s"),
            txt(" sort  "),
            key("c"),
            txt(" collapse all  "),
            key("?"),
            txt(" help  "),
            key("q"),
            txt(" quit"),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Centered popup area of at most `w` x `h` cells.
fn popup(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn popup_block<'a>(title: &'a str, color: Color, pal: &Palette) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Line::from(Span::styled(format!(" {title} "), Style::new().add_modifier(Modifier::BOLD))))
        .style(Style::new().bg(pal.fix(Color::Rgb(16, 19, 26))))
        .padding(Padding::horizontal(1))
}

fn draw_details(f: &mut Frame, app: &App, pal: &Palette) {
    let area = popup(f.area(), 96, 30);
    f.render_widget(Clear, area);
    let Some(p) = app.detail_pod() else {
        let block = popup_block("Pod details", pal.fix(BORDER), pal);
        f.render_widget(Paragraph::new("\nThis pod no longer exists.  (Esc to close)").block(block), area);
        return;
    };
    let now = app.snap.as_ref().map_or_else(SystemTime::now, |s| s.taken);
    let dim = Style::new().fg(pal.fix(DIM));
    let color = match p.usage {
        Measurement::Fresh(u) => pal.cpu(u.cpu_millis),
        _ => pal.fix(BORDER),
    };
    let label = |k: &str| Span::styled(format!("{k:<12}"), dim);
    let opt = |s: &Option<std::sync::Arc<str>>| s.as_deref().unwrap_or("—").to_string();

    let usage = match &p.usage {
        Measurement::Fresh(_) => Span::raw("fresh"),
        Measurement::Stale { .. } => Span::styled(
            format!("stale: the newest sample is {}", p.usage.age_text().unwrap_or_default()),
            Style::new().fg(pal.fix(WARN)),
        ),
        Measurement::Missing => Span::styled(
            "no sample: metrics.k8s.io hasn't reported this pod (new pod, or metrics unavailable)",
            Style::new().fg(pal.fix(WARN)),
        ),
        Measurement::NotRunning => Span::styled("not running, so no usage", dim),
    };
    let created = p
        .pod
        .created
        .map_or("—".to_string(), |c| format!("{} ago", fmt_age(now.duration_since(c).unwrap_or_default())));
    let owner = match &p.pod.owner {
        Some(direct) if *direct != p.workload => format!("{} (via {})", p.workload.describe(), direct.describe()),
        Some(_) => p.workload.describe(),
        None => "none (a bare pod: deleting it is permanent)".into(),
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(p.pod.name.to_string(), Style::new().add_modifier(Modifier::BOLD).fg(color)),
            Span::styled(format!("   namespace {}", p.pod.namespace), dim),
        ]),
        Line::raw(""),
        Line::from(vec![
            label("Status"),
            phase_span(p, pal),
            Span::styled(format!("   restarts {}", p.pod.restarts()), dim),
        ]),
        Line::from(vec![
            label("CPU"),
            Span::styled(p.usage.cpu_text(), Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::styled("   memory ", dim),
            Span::styled(p.usage.mem_text(), Style::new().add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![label("Usage"), usage]),
        Line::from(vec![label("Workload"), Span::raw(owner)]),
        Line::from(vec![label("Node"), Span::raw(opt(&p.pod.node))]),
        Line::from(vec![label("Pod IP"), Span::raw(opt(&p.pod.ip))]),
        Line::from(vec![label("Created"), Span::raw(created)]),
        Line::raw(""),
        Line::styled(format!("Containers ({})", p.pod.containers.len()), Style::new().add_modifier(Modifier::BOLD)),
    ];
    for c in &p.pod.containers {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}", c.name), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", c.image), dim),
        ]));
        let mut l = vec![
            Span::raw("    requests "),
            Span::raw(resources(&c.requests)),
            Span::raw("   limits "),
            Span::raw(resources(&c.limits)),
        ];
        if c.restarts > 0 {
            l.push(Span::styled(format!("   restarts {}", c.restarts), Style::new().fg(pal.fix(WARN))));
        }
        lines.push(Line::from(l));
    }

    let block = popup_block("Pod details", color, pal);
    let inner = block.inner(area);
    f.render_widget(block, area);
    let [body, keys] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
    f.render_widget(Paragraph::new(Line::styled("Esc close · q quit", dim)), keys);
}

/// "100m CPU, 64.0 MiB" or "none".
fn resources(r: &Resources) -> String {
    let parts: Vec<String> = [r.cpu_millis.map(|m| format!("{} CPU", fmt_cpu(m))), r.mem_bytes.map(fmt_bytes)]
        .into_iter()
        .flatten()
        .collect();
    if parts.is_empty() { "none".into() } else { parts.join(", ") }
}

fn draw_help(f: &mut Frame, pal: &Palette) {
    let rows = [
        ("↑ ↓ PgUp PgDn Home End", "move (also j k g G)"),
        ("Enter", "pod details, or collapse / expand a group"),
        ("Space", "collapse / expand a namespace or workload"),
        ("c", "collapse every workload / expand everything"),
        ("/", "search pod, namespace, workload or node; Esc clears"),
        ("v or 1–2", "view: by workload, pods"),
        ("s", "sort: CPU, memory, name"),
        ("q, Ctrl+C", "quit"),
    ];
    let mut lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::styled(format!("{k:<24}"), Style::new().fg(pal.fix(ACCENT)).add_modifier(Modifier::BOLD)),
                Span::raw(*d),
            ])
        })
        .collect();
    let dim = Style::new().fg(pal.fix(DIM));
    lines.push(Line::raw(""));
    lines.push(Line::styled("CPU colors: blue (idle) → yellow → red (1000m = one full core)", dim));
    lines.push(Line::styled("— means no usage sample, never zero. ╌╌╌ is an unmeasured bar.", dim));
    lines.push(Line::styled("≥ marks a total missing some pods; MEASURED shows how many.", dim));
    lines.push(Line::styled("Gray italic values are stale (the age is in STATUS).", dim));
    let area = popup(f.area(), 82, lines.len() as u16 + 4);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(popup_block("Keys", pal.fix(ACCENT), pal)), area);
}
