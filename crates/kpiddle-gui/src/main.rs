//! kpiddle graphical front end: a Kubernetes cluster's pods as bubbles,
//! sized by memory and colored by CPU, grouped by workload, namespace or
//! node. Click a pod for its details. Read-only unless started with
//! `--allow-actions`, which enables evict, rollout restart and delete in
//! the details window.

mod details;
mod view;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk::prelude::*;
use gtk::{gdk, glib};

use kpiddle_core::model::{MetricsState, PodsState, Snapshot};
use kpiddle_core::source::{self, Options, Source};
use piddle_encode::fmt_clock;
use piddle_gtk::Canvas;
use piddle_gtk::draw::glib_escape;
use view::{Layout, Motion, View};

const APP_ID: &str = "io.github.piddle.kpiddle";

const HELP: &str = "\
kpiddle-gui: Kubernetes pods as bubbles

Usage: kpiddle-gui [OPTIONS]

      --context NAME     kubeconfig context (default: the current one)
  -n, --namespace NAME   one namespace (default: all namespaces)
      --allow-actions    allow evict, rollout restart and delete from a pod's
                         details window (each one asks first; risky ones
                         need the name typed)
      --snapshot FILE [workload|namespace|node|free] [query]
                         render once to a PNG and exit (needs a display)
  -h, --help             show this help
  -V, --version

Size is memory, color is CPU (1000m, one full core, is red). Pods without a
usage sample are hollow gray rings, never idle-looking bubbles. Click a pod
for its details.";

/// How often the view is rebuilt from the latest data.
const REFRESH: Duration = Duration::from_secs(1);

const CSS: &str = r#"
window, .main { background: #0b0d12; color: #e6e9ef; }
headerbar, headerbar label { color: #e6e9ef; }
headerbar { background: #12151c; }
headerbar button, headerbar dropdown > button {
  background: #1d222c; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.10);
  border-radius: 7px; box-shadow: none; padding: 4px 10px; min-height: 26px; margin: 0;
}
headerbar button:hover, headerbar dropdown > button:hover { background: #272d3a; }
headerbar windowcontrols button { background: transparent; border: none; padding: 4px; min-height: 0; margin: 0; }
headerbar searchentry {
  background: #0f1218; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.12); border-radius: 7px;
  box-shadow: none; min-height: 28px;
}
headerbar searchentry:focus-within { border-color: #3b82f6; }
popover > contents, popover > arrow { background: #1a1e27; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.10); }
popover row:selected, popover row:hover { background: rgba(59,130,246,0.25); }
.brand { font-weight: 800; font-size: 15px; letter-spacing: 0.5px; }
.matches { color: rgba(200,210,230,0.65); font-size: 12px; }
.banner { background: #11141b; border-bottom: 1px solid rgba(255,255,255,0.06); padding: 6px 14px; }
.banner label { font-size: 12.5px; }
.legend { color: rgba(200,210,230,0.55); font-size: 11.5px; }
tooltip, tooltip.background { background: #232834; color: #e6e9ef; }
scrollbar, scrollbar trough { background: transparent; }
scrollbar slider { background: rgba(255,255,255,0.18); min-width: 6px; min-height: 6px; border-radius: 3px; margin: 0; border: none; }
window.details, .details { background: #0f1218; color: #e6e9ef; }
.details headerbar { background: #0f1218; border-bottom: 1px solid rgba(255,255,255,0.05); }
headerbar .title { font-weight: 800; letter-spacing: 0.5px; }
.details-title { font-size: 21px; font-weight: 800; }
.details-subtitle { color: rgba(200,210,230,0.65); }
.pill { background: rgba(255,255,255,0.07); border: 1px solid rgba(255,255,255,0.10); border-radius: 999px;
  padding: 3px 10px; font-size: 11px; font-weight: 600; color: rgba(220,226,238,0.9); }
.pill.pill-bad { background: rgba(240,90,90,0.14); border-color: rgba(240,90,90,0.45); color: #f6a3a3; }
.details-card { background: #161a22; border: 1px solid rgba(255,255,255,0.06); border-radius: 10px; padding: 12px 14px; }
.card-label { font-size: 10px; font-weight: 700; letter-spacing: 1px; color: rgba(158,173,204,0.9); }
.card-sub { font-size: 11px; color: rgba(200,210,230,0.55); }
.big-num { font-size: 22px; font-weight: 800; font-feature-settings: "tnum"; margin-top: 2px; }
.info-key { color: rgba(200,210,230,0.55); }
.info-val { color: #e6e9ef; }
.cmdline-box { background: #0a0c11; border: 1px solid rgba(255,255,255,0.08); border-radius: 8px; }
.containers { font-size: 12px; color: #c9d4e5; padding: 8px 10px; }
.details-status { color: rgba(200,210,230,0.7); font-size: 12px; }
.details-status.status-bad { color: #f6a3a3; }
button.action { background: #1d222c; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.12); border-radius: 8px;
  padding: 6px 16px; box-shadow: none; margin: 0; }
button.action:hover { background: #272d3a; }
button.action.destructive-action { background: #b3261e; border-color: #c9372d; color: #ffffff; }
button.action.destructive-action:hover { background: #c9372d; }
button.action:disabled { opacity: 0.4; }
.risk-banner { background: rgba(245,158,11,0.10); border: 1px solid rgba(245,158,11,0.35); border-radius: 8px;
  padding: 9px 12px; color: #f5c56b; }
.confirm-heading { font-size: 16px; font-weight: 800; }
.confirm-entry { background: #0a0c11; color: #e6e9ef; border: 1px solid rgba(255,255,255,0.15); border-radius: 7px; min-height: 30px; }
.confirm-entry:focus-within { border-color: #c9372d; }
.confirm-entry text placeholder { color: rgba(230,233,239,0.3); font-style: italic; }
"#;

struct Args {
    opts: Options,
    snapshot: Option<(String, Layout, String)>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut opts = Options::default();
    let mut snapshot = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--context" => opts.context = Some(value("--context")?),
            "-n" | "--namespace" => opts.namespace = Some(value("--namespace")?),
            "--allow-actions" => opts.allow_actions = true,
            "--snapshot" => {
                let path = value("--snapshot")?;
                let layout = match args.peek().and_then(|l| Layout::parse(l)) {
                    Some(l) => {
                        args.next();
                        l
                    }
                    None => Layout::Workload,
                };
                let query = args.next_if(|q| !q.starts_with('-')).unwrap_or_default();
                snapshot = Some((path, layout, query));
            }
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("kpiddle-gui {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(Some(Args { opts, snapshot }))
}

fn main() -> glib::ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return glib::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kpiddle-gui: {e}\n\n{HELP}");
            return glib::ExitCode::from(2);
        }
    };
    let src = match source::spawn(args.opts) {
        Ok(s) => Rc::new(s),
        Err(e) => {
            eprintln!("kpiddle-gui: {e}");
            return glib::ExitCode::FAILURE;
        }
    };
    if let Some((path, layout, query)) = args.snapshot {
        snapshot(&src, &path, layout, &query);
        return glib::ExitCode::SUCCESS;
    }

    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
        }
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_application_prefer_dark_theme(true);
        }
    });
    app.connect_activate(move |app| build_ui(app, src.clone()));
    // Our arguments are parsed above; don't let GTK parse them again.
    app.run_with_args(&[] as &[&str])
}

/// Drives the scene with the frame clock until it comes to rest.
struct Animator {
    canvas: Canvas,
    view: Rc<RefCell<View>>,
    ticking: Cell<bool>,
    last_frame: Cell<i64>,
    last_draw: Cell<i64>,
}

/// Minimum time between redraws while only colors are easing (~20 fps).
const SUBTLE_FRAME_US: i64 = 50_000;

impl Animator {
    fn kick(self: &Rc<Self>) {
        if self.ticking.replace(true) {
            return;
        }
        self.last_frame.set(0);
        let this = self.clone();
        self.canvas.add_tick_callback(move |w, clock| {
            let t = clock.frame_time();
            let last = this.last_frame.replace(t);
            let dt = if last == 0 { 1.0 / 60.0 } else { (t - last) as f64 / 1e6 };
            let motion = this.view.borrow_mut().step(dt);
            if motion != Motion::Subtle || t - this.last_draw.get() >= SUBTLE_FRAME_US {
                this.last_draw.set(t);
                w.queue_draw();
            }
            if motion == Motion::Idle {
                this.ticking.set(false);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }
}

fn build_ui(app: &gtk::Application, src: Rc<Source>) {
    let view = Rc::new(RefCell::new(View::new(Layout::Workload)));

    // ---------------------------------------------------------------- header
    let header = gtk::HeaderBar::new();
    let brand = gtk::Label::new(None);
    brand.set_markup("<span foreground='#f2c94c'>k</span>PID<span foreground='#f2c94c'>dle</span>");
    brand.add_css_class("brand");
    header.set_title_widget(Some(&brand));

    let search = gtk::SearchEntry::builder().placeholder_text("Search pod, namespace, workload or node…").build();
    let matches = gtk::Label::new(None);
    matches.add_css_class("matches");
    header.pack_start(&search);
    header.pack_start(&matches);

    let names: Vec<&str> = Layout::ALL.iter().map(|l| l.name()).collect();
    let layout_dd = gtk::DropDown::from_strings(&names);
    layout_dd.set_tooltip_text(Some("Group bubbles"));
    header.pack_end(&layout_dd);

    // ---------------------------------------------------------------- banner
    // Always visible: where the data comes from and whether usage is current.
    let banner = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    banner.add_css_class("banner");
    let context = gtk::Label::new(None);
    let scope = src.namespace.as_ref().map_or("all namespaces".to_string(), |n| format!("namespace {n}"));
    // Say so when this window can change the cluster.
    let mode = if src.actions_allowed() {
        " <span foreground='#f0a03c' weight='bold'>· actions on</span>"
    } else {
        " <span alpha='60%'>· read-only</span>"
    };
    context.set_markup(&format!(
        "<span alpha='60%'>context</span> <b>{}</b> <span alpha='60%'>· {}</span>{mode}",
        glib_escape(&src.context),
        glib_escape(&scope)
    ));
    let metrics = gtk::Label::new(None);
    let totals = gtk::Label::new(None);
    totals.set_hexpand(true);
    totals.set_xalign(0.0);
    totals.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let legend = gtk::Label::new(Some("size = memory · color = CPU (1000m = red) · hollow ring = no usage sample"));
    legend.add_css_class("legend");
    banner.append(&context);
    banner.append(&metrics);
    banner.append(&totals);
    banner.append(&legend);

    // ---------------------------------------------------------------- canvas
    let canvas = Canvas::new(view.clone());
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    canvas.set_focusable(true);
    let anim = Rc::new(Animator {
        canvas: canvas.clone(),
        view: view.clone(),
        ticking: Cell::new(false),
        last_frame: Cell::new(0),
        last_draw: Cell::new(0),
    });
    canvas.connect_resized(glib::clone!(
        #[weak]
        anim,
        move || anim.kick()
    ));

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.add_css_class("main");
    body.append(&banner);
    body.append(&canvas);

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title(format!("kpiddle · {}", src.context))
        .default_width(1360)
        .default_height(860)
        .child(&body)
        .build();
    window.set_titlebar(Some(&header));

    // ---------------------------------------------------------------- input
    let motion = gtk::EventControllerMotion::new();
    motion.connect_motion(glib::clone!(
        #[strong]
        anim,
        move |_, x, y| {
            let mut v = anim.view.borrow_mut();
            v.scene.mouse = (x, y);
            let h = v.pick(x, y);
            let changed = h != v.scene.hover;
            v.scene.hover = h.clone();
            drop(v);
            if changed || h.is_some() {
                anim.canvas.queue_draw();
            }
        }
    ));
    motion.connect_leave(glib::clone!(
        #[strong]
        anim,
        move |_| {
            anim.view.borrow_mut().scene.hover = None;
            anim.canvas.queue_draw();
        }
    ));
    canvas.add_controller(motion);

    let click = gtk::GestureClick::new();
    click.connect_released(glib::clone!(
        #[strong]
        view,
        #[strong]
        src,
        #[weak]
        window,
        move |_, _, x, y| {
            let uid = view.borrow().pick(x, y);
            if let Some(uid) = uid {
                details::open(&window, &view, &src, uid);
            }
        }
    ));
    canvas.add_controller(click);

    let update_matches = {
        let view = view.clone();
        let matches = matches.clone();
        move || {
            let v = view.borrow();
            matches.set_text(&if v.has_query() {
                format!("{} match{}", v.scene.matches, if v.scene.matches == 1 { "" } else { "es" })
            } else {
                String::new()
            });
        }
    };
    search.connect_search_changed(glib::clone!(
        #[strong]
        anim,
        #[strong]
        update_matches,
        move |e| {
            anim.view.borrow_mut().set_query(&e.text());
            update_matches();
            anim.canvas.queue_draw();
        }
    ));
    layout_dd.connect_selected_notify(glib::clone!(
        #[strong]
        anim,
        move |d| {
            let l = Layout::ALL.get(d.selected() as usize).copied().unwrap_or(Layout::Workload);
            anim.view.borrow_mut().set_layout(l);
            anim.kick();
        }
    ));

    // ---------------------------------------------------------------- data
    let refresh = move || {
        let snap = src.lock().snapshot(SystemTime::now());
        set_banner(&metrics, &totals, &snap);
        view.borrow_mut().apply(snap);
        update_matches();
        anim.kick();
    };
    refresh();
    glib::timeout_add_local(REFRESH, move || {
        refresh();
        glib::ControlFlow::Continue
    });

    window.present();
    canvas.grab_focus();
}

fn set_banner(metrics: &gtk::Label, totals: &gtk::Label, s: &Snapshot) {
    let (color, text) = match &s.metrics {
        MetricsState::Waiting => ("#8c96a8", "waiting for the first poll…".to_string()),
        MetricsState::Ok => ("#78c8a0", "OK".to_string()),
        MetricsState::Stale { last_success, reason } => {
            let us = last_success.duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
            let why = reason.as_ref().map_or(String::new(), |r| format!(": {r}"));
            ("#f0a03c", format!("STALE since {}{why}", fmt_clock(us)))
        }
        MetricsState::Unavailable { reason } => ("#f05a5a", format!("UNAVAILABLE: {reason}")),
    };
    let mut markup = format!(
        "<span alpha='60%'>metrics</span> <span foreground='{color}' weight='bold'>{}</span>",
        glib_escape(&text)
    );
    match &s.pods_state {
        PodsState::Listing => markup.push_str(" <span alpha='60%'>· listing pods…</span>"),
        PodsState::Failing { reason } => {
            markup.push_str(&format!(" <span foreground='#f05a5a'>· pod watch failing: {}</span>", glib_escape(reason)))
        }
        PodsState::Synced => {}
    }
    metrics.set_markup(&markup);

    let c = &s.cluster;
    let mut t = format!(
        "{} pods · CPU <b>{}</b> · memory <b>{}</b>",
        s.pods.len(),
        glib_escape(&c.cpu_text()),
        glib_escape(&c.mem_text())
    );
    if let Some(cov) = c.coverage_text() {
        t.push_str(&format!(" · <span foreground='#f0a03c'>{cov}</span>"));
    }
    if c.not_running > 0 {
        t.push_str(&format!(" <span alpha='60%'>· {} not running</span>", c.not_running));
    }
    totals.set_markup(&t);
}

/// Headless render for testing and docs.
fn snapshot(src: &Source, path: &str, layout: Layout, query: &str) {
    gtk::init().expect("GTK init (needs a display)");
    let (w, h) = (1100.0, 760.0);
    // Wait for the first pod list and metrics poll.
    let start = Instant::now();
    let snap = loop {
        let s = src.lock().snapshot(SystemTime::now());
        if (s.pods_state != PodsState::Listing && s.metrics != MetricsState::Waiting)
            || start.elapsed() > Duration::from_secs(20)
        {
            break s;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut v = View::new(layout);
    v.scene.size = (w, h);
    v.apply(snap);
    v.set_query(query);
    let mut frames = 0;
    while v.step(1.0 / 60.0) != Motion::Idle && frames < 3000 {
        frames += 1;
    }
    eprintln!("settled after {frames} frames, {} bubbles", v.scene.bubbles.len());
    if query.is_empty()
        && let Some(b) = v.scene.bubbles.iter().max_by(|a, b| a.cpu.total_cmp(&b.cpu))
    {
        let uid: Arc<str> = b.item.pod.pod.uid.clone();
        v.scene.mouse = v.scene.screen_pos(&uid).unwrap_or((w / 2.0, h / 2.0));
        v.scene.hover = Some(uid);
    }

    let snap = gtk::Snapshot::new();
    let pctx = pangocairo::FontMap::default().create_context();
    v.render(&snap, &pctx);
    let node = snap.to_node().expect("render node");
    let renderer = gtk::gsk::CairoRenderer::new();
    renderer.realize_for_display(&gdk::Display::default().expect("display")).expect("renderer");
    let tex = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    renderer.unrealize();
    tex.save_to_png(path).expect("write png");
}
