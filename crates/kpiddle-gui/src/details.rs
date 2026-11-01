//! Pod details window, with evict, rollout restart and delete behind
//! `--allow-actions`.
//!
//! The safety rules are kpiddle-core's ([`assess`]), the same as the TUI's:
//! refused actions are disabled with the reason, risky ones need the name
//! typed, the rest a confirmation that defaults to Cancel. The window
//! follows its pod by UID, so a replacement with the same name (a
//! StatefulSet member, say) is a different pod and the window says its pod
//! is gone.

use std::cell::{Cell, RefCell};
use std::f64::consts::TAU;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, SystemTime};

use gtk::prelude::*;
use gtk::{gio, glib};

use kpiddle_core::actions::{Action, Assessment, Outcome, assess};
use kpiddle_core::model::{Measurement, Phase, PodView, fmt_age};
use kpiddle_core::source::Source;
use piddle_encode::{cpu_color, fmt_bytes};
use piddle_gtk::draw::glib_escape;

use crate::view::View;

const REFRESH: Duration = Duration::from_secs(1);
const POLL_OUTCOME: Duration = Duration::from_millis(100);

/// What the badge draws: a CPU-colored disc, or a hollow ring when there is
/// no usage to color by.
#[derive(Clone, Copy, PartialEq)]
enum Badge {
    Cpu(f64),
    Stale(f64),
    Hollow,
}

pub fn open(parent: &gtk::ApplicationWindow, view: &Rc<RefCell<View>>, src: &Rc<Source>, uid: Arc<str>) {
    let Some(first) = pod(view, &uid) else { return };

    let header = gtk::HeaderBar::new();
    let title = gtk::Label::new(Some("Pod details"));
    title.add_css_class("title");
    header.set_title_widget(Some(&title));

    let win = gtk::Window::builder()
        .title(format!("{} — {}", first.pod.name, first.pod.namespace))
        .transient_for(parent)
        .default_width(680)
        .titlebar(&header)
        .build();
    win.add_css_class("details");

    let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(20);
    root.set_margin_end(20);

    // ---- identity: CPU-colored badge, name, subtitle, status
    let badge_state = Rc::new(Cell::new(Badge::Hollow));
    let badge = gtk::DrawingArea::builder().content_width(44).content_height(44).valign(gtk::Align::Center).build();
    badge.set_draw_func(glib::clone!(
        #[strong]
        badge_state,
        move |_, cr, w, h| draw_badge(cr, w as f64, h as f64, badge_state.get())
    ));
    let name = gtk::Label::builder().label(&*first.pod.name).xalign(0.0).selectable(true).build();
    name.add_css_class("details-title");
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let subtitle = gtk::Label::builder().xalign(0.0).selectable(true).build();
    subtitle.add_css_class("details-subtitle");
    subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    names.set_hexpand(true);
    names.set_valign(gtk::Align::Center);
    names.append(&name);
    names.append(&subtitle);
    let pill = gtk::Label::new(None);
    pill.add_css_class("pill");
    pill.set_valign(gtk::Align::Center);
    let ident = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    ident.append(&badge);
    ident.append(&names);
    ident.append(&pill);
    root.append(&ident);

    // ---- stat cards
    let cards = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    cards.set_homogeneous(true);
    let (cpu_val, cpu_sub) = stat_card(&cards, "CPU");
    let (mem_val, mem_sub) = stat_card(&cards, "MEMORY");
    let (rst_val, rst_sub) = stat_card(&cards, "RESTARTS");
    let (age_val, age_sub) = stat_card(&cards, "AGE");
    root.append(&cards);

    // ---- info list
    let grid = gtk::Grid::builder().row_spacing(8).column_spacing(18).build();
    grid.add_css_class("details-card");
    let mut values: Vec<(&'static str, gtk::Label)> = Vec::new();
    for (row, key) in ["Workload", "Node", "Pod IP", "Usage"].into_iter().enumerate() {
        let k = gtk::Label::builder().label(key).xalign(0.0).yalign(0.0).build();
        k.add_css_class("info-key");
        // Equal minimum and maximum width, as in piddle-gui: a narrower
        // minimum makes GTK measure the window with extra wrapped lines.
        let v = gtk::Label::builder()
            .xalign(0.0)
            .selectable(true)
            .wrap(true)
            .hexpand(true)
            .width_chars(56)
            .max_width_chars(56)
            .build();
        v.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        v.add_css_class("info-val");
        grid.attach(&k, 0, row as i32, 1, 1);
        grid.attach(&v, 1, row as i32, 1, 1);
        values.push((key, v));
    }
    root.append(&grid);

    // ---- containers
    let ch = gtk::Label::builder().label("CONTAINERS").xalign(0.0).build();
    ch.add_css_class("card-label");
    let containers = gtk::Label::builder()
        .xalign(0.0)
        .yalign(0.0)
        .selectable(true)
        .wrap(true)
        .width_chars(56)
        .max_width_chars(56)
        .build();
    containers.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    containers.add_css_class("containers");
    let containers_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(40)
        .max_content_height(200)
        .propagate_natural_height(true)
        .child(&containers)
        .build();
    containers_scroll.add_css_class("cmdline-box");
    let cbox = gtk::Box::new(gtk::Orientation::Vertical, 6);
    cbox.append(&ch);
    cbox.append(&containers_scroll);
    root.append(&cbox);

    // ---- risk banner, shown for refused or risky pods
    let risk = gtk::Label::builder().xalign(0.0).wrap(true).width_chars(40).max_width_chars(70).visible(false).build();
    risk.add_css_class("risk-banner");
    root.append(&risk);

    // ---- footer: status on the left, actions on the right
    let status = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).width_chars(20).build();
    status.add_css_class("details-status");
    let evict = gtk::Button::with_label("Evict");
    evict.add_css_class("action");
    let restart = gtk::Button::with_label("Rollout restart");
    restart.add_css_class("action");
    let delete = gtk::Button::with_label("Delete");
    delete.add_css_class("action");
    delete.add_css_class("destructive-action");
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    footer.set_valign(gtk::Align::End);
    footer.set_vexpand(true);
    footer.append(&status);
    footer.append(&evict);
    footer.append(&restart);
    footer.append(&delete);
    root.append(&footer);
    win.set_child(Some(&root));

    let buttons = [(Action::Evict, evict), (Action::RolloutRestart, restart), (Action::Delete, delete)];
    let allowed = src.actions_allowed();
    if !allowed {
        status.set_text("Read-only: start kpiddle-gui with --allow-actions to evict, restart or delete.");
    }
    // An action this window started and hasn't heard back from.
    let busy = Rc::new(Cell::new(false));

    let refresh = {
        let (view, uid, busy, buttons) = (view.clone(), uid.clone(), busy.clone(), buttons.clone());
        let (badge, status) = (badge.clone(), status.clone());
        move || -> bool {
            let v = view.borrow();
            let snap = v.snap.as_ref();
            let Some((snap, p)) = snap.and_then(|s| Some((s, s.pods.iter().find(|p| p.pod.uid == uid)?))) else {
                set(&pill, "Gone");
                // Keep a just-reported success ("✓ Evicted …") in view.
                let text = status.text();
                if text.starts_with('✓') {
                    status.set_text(&format!("{text} The pod is gone."));
                } else {
                    status.remove_css_class("status-bad");
                    status.set_text("This pod no longer exists.");
                }
                risk.set_visible(false);
                for (_, b) in &buttons {
                    b.set_sensitive(false);
                }
                return false;
            };
            let now = snap.taken;

            badge_state.set(match p.usage {
                Measurement::Fresh(u) => Badge::Cpu(u.cpu_millis / 10.0),
                Measurement::Stale { usage, .. } => Badge::Stale(usage.cpu_millis / 10.0),
                _ => Badge::Hollow,
            });
            badge.queue_draw();
            set(&subtitle, &format!("{}  ·  {}", p.pod.namespace, p.workload.describe()));
            set(&pill, p.pod.reason.as_deref().unwrap_or(p.pod.phase.label()));
            if p.pod.reason.is_some() || p.pod.phase == Phase::Failed {
                pill.add_css_class("pill-bad");
            } else {
                pill.remove_css_class("pill-bad");
            }

            set(&cpu_val, &p.usage.cpu_text());
            set(&mem_val, &p.usage.mem_text());
            let freshness = match &p.usage {
                Measurement::Fresh(_) => "fresh sample".to_string(),
                Measurement::Stale { .. } => format!("stale: {}", p.usage.age_text().unwrap_or_default()),
                Measurement::Missing => "no sample".into(),
                Measurement::NotRunning => "not running".into(),
            };
            set(&cpu_sub, &freshness);
            set(&mem_sub, &memory_caption(p));
            set(&rst_val, &p.pod.restarts().to_string());
            set(&rst_sub, &format!("in {} container{}", p.pod.containers.len(), plural(p.pod.containers.len())));
            match p.pod.created {
                Some(c) => {
                    set(&age_val, &fmt_age(now.duration_since(c).unwrap_or_default()));
                    let (short, full) = created_text(c, now);
                    set(&age_sub, &short);
                    age_sub.set_tooltip_text(Some(&full));
                }
                None => {
                    set(&age_val, "—");
                    set(&age_sub, "");
                }
            }

            for (key, label) in &values {
                let text = match *key {
                    "Workload" => p.owner_text(),
                    "Node" => p.pod.node.as_deref().unwrap_or("— (not scheduled)").to_string(),
                    "Pod IP" => p.pod.ip.as_deref().unwrap_or("—").to_string(),
                    "Usage" => usage_text(p),
                    _ => String::new(),
                };
                set(label, &text);
            }
            set_markup(&containers, &containers_markup(p));

            // Evict carries the pod's own warnings; delete adds only its
            // PDB note, which belongs in its confirmation.
            let a = assess(snap, p, Action::Evict);
            match (&a.blocked, a.is_critical()) {
                (Some(why), _) => {
                    set(&risk, &format!("⛔  {why}"));
                    risk.set_visible(true);
                }
                (None, true) => {
                    set(&risk, &format!("⚠  Needs the name typed to evict or delete. {}", a.warnings.join(" ")));
                    risk.set_visible(true);
                }
                (None, false) => risk.set_visible(false),
            }
            for (action, b) in &buttons {
                let a = assess(snap, p, *action);
                b.set_sensitive(allowed && !busy.get() && a.blocked.is_none());
                let tip = match (&a.blocked, allowed) {
                    (Some(why), _) => format!("Not allowed: {why}"),
                    (None, false) => "Read-only: start kpiddle-gui with --allow-actions.".into(),
                    (None, true) => a.effect.clone(),
                };
                b.set_tooltip_text(Some(&tip));
            }
            true
        }
    };
    refresh();
    let weak = win.downgrade();
    let refresh = Rc::new(refresh);
    glib::timeout_add_local(
        REFRESH,
        glib::clone!(
            #[strong]
            refresh,
            move || {
                if weak.upgrade().is_none() || !refresh() {
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }
        ),
    );

    for (action, btn) in &buttons {
        let action = *action;
        btn.connect_clicked(glib::clone!(
            #[weak]
            win,
            #[strong]
            view,
            #[strong]
            src,
            #[strong]
            uid,
            #[strong]
            status,
            #[strong]
            busy,
            #[strong]
            refresh,
            move |_| {
                // Assess against the latest data, not what was on screen at
                // the last refresh.
                let Some(a) = view.borrow().snap.as_ref().and_then(|s| {
                    let p = s.pods.iter().find(|p| p.pod.uid == uid)?;
                    Some(assess(s, p, action))
                }) else {
                    return;
                };
                if a.blocked.is_some() {
                    return;
                }
                let run = {
                    let (win, src, status, busy, refresh) =
                        (win.clone(), src.clone(), status.clone(), busy.clone(), refresh.clone());
                    move |a: Assessment| start(&win, &src, a, &status, &busy, refresh.clone())
                };
                confirm(&win, &src.context, a, run);
            }
        ));
    }

    // Escape closes the window, like other GTK dialogs.
    let esc = gtk::ShortcutController::new();
    esc.set_propagation_phase(gtk::PropagationPhase::Capture);
    esc.add_shortcut(gtk::Shortcut::new(
        Some(gtk::KeyvalTrigger::new(gtk::gdk::Key::Escape, gtk::gdk::ModifierType::empty())),
        Some(gtk::CallbackAction::new(|w, _| {
            if let Some(w) = w.downcast_ref::<gtk::Window>() {
                w.close();
            }
            glib::Propagation::Stop
        })),
    ));
    win.add_controller(esc);
    win.present();
    // Nothing focused: not the selectable name (it would show a cursor),
    // and not an action button that Enter would press.
    gtk::prelude::GtkWindowExt::set_focus(&win, None::<&gtk::Widget>);
}

fn pod(view: &Rc<RefCell<View>>, uid: &str) -> Option<PodView> {
    view.borrow().snap.as_ref()?.pods.iter().find(|p| &*p.pod.uid == uid).cloned()
}

/// Hand a confirmed action to the source and report its outcome in the
/// window's status line; a failure also gets a dialog, since "a
/// PodDisruptionBudget said no" must not scroll past unnoticed.
fn start(
    win: &gtk::Window,
    src: &Source,
    a: Assessment,
    status: &gtk::Label,
    busy: &Rc<Cell<bool>>,
    refresh: Rc<impl Fn() -> bool + 'static>,
) {
    let rx = match src.act(a.clone()) {
        Ok(rx) => rx,
        Err(why) => {
            fail(win, status, &a, &why);
            return;
        }
    };
    status.remove_css_class("status-bad");
    status.set_text(&format!("{} {}…", a.action.label(), a.target.describe()));
    busy.set(true);
    refresh();
    let (win, status, busy) = (win.downgrade(), status.clone(), busy.clone());
    glib::timeout_add_local(POLL_OUTCOME, move || {
        let outcome = match poll(&rx, &a) {
            Some(o) => o,
            None => return glib::ControlFlow::Continue,
        };
        busy.set(false);
        let Some(win) = win.upgrade() else { return glib::ControlFlow::Break };
        if outcome.ok {
            status.remove_css_class("status-bad");
            status.set_text(&format!("✓ {}", outcome.message));
        } else {
            fail(&win, &status, &a, &outcome.message);
        }
        refresh();
        glib::ControlFlow::Break
    });
}

fn poll(rx: &Receiver<Outcome>, a: &Assessment) -> Option<Outcome> {
    match rx.try_recv() {
        Ok(o) => Some(o),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(Outcome {
            ok: false,
            message: format!("{} {}: the cluster connection stopped.", a.action.label(), a.target.describe()),
        }),
    }
}

fn fail(win: &gtk::Window, status: &gtk::Label, a: &Assessment, why: &str) {
    status.add_css_class("status-bad");
    status.set_text(&format!("✗ {why}"));
    gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("{} {} failed", a.action.label(), a.target.describe()))
        .detail(why)
        .build()
        .show(Some(win));
}

/// Ask before acting: a dialog that defaults to Cancel, or for a risky
/// action one where the name must be typed.
fn confirm(parent: &gtk::Window, context: &str, a: Assessment, on_confirm: impl Fn(Assessment) + 'static) {
    let mut detail = format!("Context {context}.\n\n{}", a.effect);
    if a.action == Action::Delete {
        detail.push_str("\n\nTo restart a pod, Evict is the safer choice.");
    }
    if a.is_critical() {
        confirm_typed(parent, &a, &detail, on_confirm);
        return;
    }
    let verb = a.action.label();
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(format!("{verb} {}?", a.target.describe()))
        .detail(detail)
        .buttons(["Cancel", verb])
        .cancel_button(0)
        .default_button(0)
        .build();
    dialog.choose(Some(parent), None::<&gio::Cancellable>, move |res| {
        if res == Ok(1) {
            on_confirm(a);
        }
    });
}

/// The stronger confirmation: the action's button stays disabled until the
/// pod (or workload) name is typed.
fn confirm_typed(parent: &gtk::Window, a: &Assessment, detail: &str, on_confirm: impl Fn(Assessment) + 'static) {
    let verb = a.action.label();
    let dlg = gtk::Window::builder().transient_for(parent).modal(true).resizable(false).title(verb).build();
    dlg.add_css_class("details");
    dlg.add_css_class("confirm");
    let header = gtk::HeaderBar::new();
    header.set_show_title_buttons(false);
    let t = gtk::Label::new(Some("Careful"));
    t.add_css_class("title");
    header.set_title_widget(Some(&t));
    dlg.set_titlebar(Some(&header));

    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(20);
    root.set_margin_end(20);
    let wrapped = |text: &str| {
        gtk::Label::builder().label(text).xalign(0.0).wrap(true).width_chars(44).max_width_chars(60).build()
    };
    let heading = wrapped(&format!("{verb} {}?", a.target.describe()));
    heading.add_css_class("confirm-heading");
    root.append(&heading);
    let warn = wrapped(&a.warnings.iter().map(|w| format!("⚠  {w}")).collect::<Vec<_>>().join("\n\n"));
    warn.add_css_class("risk-banner");
    root.append(&warn);
    let det = wrapped(detail.trim());
    det.add_css_class("card-sub");
    root.append(&det);
    let name = a.target.name().to_string();
    let prompt = gtk::Label::builder().xalign(0.0).build();
    prompt.set_markup(&format!("Type <b>{}</b> to confirm:", glib_escape(&name)));
    root.append(&prompt);
    let entry = gtk::Entry::builder().placeholder_text("name").build();
    entry.add_css_class("confirm-entry");
    root.append(&entry);

    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("action");
    let go = gtk::Button::with_label(verb);
    go.add_css_class("action");
    go.add_css_class("destructive-action");
    go.set_sensitive(false);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(6);
    buttons.append(&cancel);
    buttons.append(&go);
    root.append(&buttons);
    dlg.set_child(Some(&root));

    entry.connect_changed(glib::clone!(
        #[weak]
        go,
        move |e| go.set_sensitive(e.text().trim() == name)
    ));
    entry.connect_activate(glib::clone!(
        #[weak]
        go,
        move |_| {
            if go.is_sensitive() {
                go.emit_clicked();
            }
        }
    ));
    cancel.connect_clicked(glib::clone!(
        #[weak]
        dlg,
        move |_| dlg.close()
    ));
    let a = a.clone();
    go.connect_clicked(glib::clone!(
        #[weak]
        dlg,
        move |_| {
            dlg.close();
            on_confirm(a.clone());
        }
    ));
    let esc = gtk::EventControllerKey::new();
    esc.connect_key_pressed(glib::clone!(
        #[weak]
        dlg,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                dlg.close();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    dlg.add_controller(esc);
    dlg.present();
    entry.grab_focus();
}

/// The memory card's caption: usage against the pod's memory limit when
/// every container sets one (a pod without a full limit can't be compared).
fn memory_caption(p: &PodView) -> String {
    let limit: Option<u64> = p.pod.containers.iter().map(|c| c.limits.mem_bytes).sum();
    match (limit.filter(|_| !p.pod.containers.is_empty()), p.usage.usage()) {
        (Some(l), Some(u)) if l > 0 => {
            format!("{:.0}% of {} limit", u.mem_bytes as f64 / l as f64 * 100.0, fmt_bytes(l))
        }
        (Some(l), None) => format!("limit {}", fmt_bytes(l)),
        _ => "working set".into(),
    }
}

fn usage_text(p: &PodView) -> String {
    match &p.usage {
        Measurement::Fresh(_) => "fresh sample from metrics.k8s.io".into(),
        Measurement::Stale { .. } => {
            format!("stale: the newest sample is {}", p.usage.age_text().unwrap_or_default())
        }
        Measurement::Missing => {
            "no sample: metrics.k8s.io hasn't reported this pod (new pod, or metrics unavailable)".into()
        }
        Measurement::NotRunning => "not running, so no usage".into(),
    }
}

fn containers_markup(p: &PodView) -> String {
    if p.pod.containers.is_empty() {
        return "<span alpha='60%'>none</span>".into();
    }
    let lines: Vec<String> = p
        .pod
        .containers
        .iter()
        .map(|c| {
            let restarts = if c.restarts > 0 {
                format!("  <span foreground='#f0a03c'>restarts {}</span>", c.restarts)
            } else {
                String::new()
            };
            format!(
                "<b>{}</b>  <span alpha='60%'>{}</span>\n<span alpha='75%'>requests</span> {}  \
                 <span alpha='75%'>limits</span> {}{restarts}",
                glib_escape(&c.name),
                glib_escape(&c.image),
                glib_escape(&c.requests.text()),
                glib_escape(&c.limits.text()),
            )
        })
        .collect();
    lines.join("\n\n")
}

/// When the pod was created: short for the card ("since 14:02" today,
/// "since Oct 30" before), and in full for its tooltip.
fn created_text(c: SystemTime, now: SystemTime) -> (String, String) {
    let local = |t: SystemTime| {
        glib::DateTime::from_unix_local(t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64).ok()
    };
    let (Some(at), Some(today)) = (local(c), local(now)) else { return Default::default() };
    let fmt = |f: &str| at.format(f).map(|s| s.to_string()).unwrap_or_default();
    let same_day = (at.year(), at.day_of_year()) == (today.year(), today.day_of_year());
    let short = if same_day { fmt("since %H:%M") } else { fmt("since %b %-d") };
    (short, fmt("created %Y-%m-%d %H:%M:%S"))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn draw_badge(cr: &gtk::cairo::Context, w: f64, h: f64, b: Badge) {
    let (cx, cy, r) = (w / 2.0, h / 2.0, w.min(h) / 2.0 - 1.5);
    let (cpu, alpha) = match b {
        Badge::Cpu(c) => (c, 1.0),
        Badge::Stale(c) => (c, 0.55),
        Badge::Hollow => {
            // No usage: a dashed gray ring, like the bubble.
            cr.set_source_rgba(0.62, 0.65, 0.70, 0.85);
            cr.set_line_width(1.5);
            cr.set_dash(&[3.0, 2.5], 0.0);
            cr.arc(cx, cy, r - 0.75, 0.0, TAU);
            let _ = cr.stroke();
            return;
        }
    };
    let c = cpu_color(cpu);
    let g = gtk::cairo::RadialGradient::new(cx - r * 0.35, cy - r * 0.4, r * 0.05, cx, cy, r);
    g.add_color_stop_rgba(0.0, c.0 + (1.0 - c.0) * 0.35, c.1 + (1.0 - c.1) * 0.35, c.2 + (1.0 - c.2) * 0.35, alpha);
    g.add_color_stop_rgba(1.0, c.0 * 0.75, c.1 * 0.75, c.2 * 0.75, alpha);
    let _ = cr.set_source(&g);
    cr.arc(cx, cy, r, 0.0, TAU);
    let _ = cr.fill();
}

/// Adds a stat card to `row` and returns its (value, caption) labels.
fn stat_card(row: &gtk::Box, title: &str) -> (gtk::Label, gtk::Label) {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 2);
    card.add_css_class("details-card");
    let t = gtk::Label::builder().label(title).xalign(0.0).build();
    t.add_css_class("card-label");
    let v = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    v.add_css_class("big-num");
    let s = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    s.add_css_class("card-sub");
    card.append(&t);
    card.append(&v);
    card.append(&s);
    row.append(&card);
    (v, s)
}

fn set(label: &gtk::Label, text: &str) {
    if label.text() != text {
        label.set_text(text);
    }
}

fn set_markup(label: &gtk::Label, markup: &str) {
    if label.label() != markup {
        label.set_markup(markup);
    }
}
