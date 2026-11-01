//! kpiddle terminal front end: pods by CPU and memory, grouped by namespace
//! and workload. Read-only unless started with --allow-actions.

mod app;
mod text;
mod ui;

use std::io::IsTerminal;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kpiddle_core::model::{MetricsState, PodsState};
use kpiddle_core::source::{self, Options};
use piddle_encode::fmt_clock;
use ratatui::crossterm::event::{self, Event, KeyEventKind};

use app::App;

const HELP: &str = "\
kpiddle: Kubernetes pods by CPU and memory

Usage: kpiddle [OPTIONS]

      --context NAME     kubeconfig context (default: the current one)
  -n, --namespace NAME   one namespace (default: all namespaces)
      --allow-actions    allow evict, delete and rollout restart (each one
                         asks first); without it kpiddle only reads
      --once             print one plain listing and exit
  -h, --help             show this help
  -V, --version

Usage comes from metrics.k8s.io (metrics-server). Where it is missing,
kpiddle shows — rather than 0, and totals say how many pods they cover.
Inside the program, press ? for keys.";

/// How often the view is rebuilt from the latest data, so ages and the
/// metrics state stay current between updates.
const REFRESH: Duration = Duration::from_secs(1);
/// With --once, how long to wait for the first pod list and metrics poll.
const ONCE_WAIT: Duration = Duration::from_secs(20);

fn main() -> std::io::Result<()> {
    let (opts, once) = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => return Ok(()),
        Err(e) => {
            eprintln!("kpiddle: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    if !once && !std::io::stdout().is_terminal() {
        eprintln!("kpiddle: needs an interactive terminal (or use --once)");
        std::process::exit(2);
    }
    let src = source::spawn(opts).unwrap_or_else(|e| {
        eprintln!("kpiddle: {e}");
        std::process::exit(1);
    });

    if once {
        let start = Instant::now();
        loop {
            let s = src.lock().snapshot(SystemTime::now());
            let ready = s.pods_state != PodsState::Listing && s.metrics != MetricsState::Waiting;
            if ready || start.elapsed() > ONCE_WAIT {
                print!("{}", text::render(&src.context, src.namespace.as_deref(), &s));
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let mut app = App::new(src.context.clone(), src.namespace.clone(), src.actions_allowed());
    let palette = ui::Palette::detect();
    // ratatui::init installs a panic hook that restores the terminal.
    let mut terminal = ratatui::init();
    let result = (|| -> std::io::Result<()> {
        let mut next_refresh = Instant::now();
        let mut dirty = true;
        loop {
            if app.poll_actions() {
                dirty = true;
            }
            if Instant::now() >= next_refresh {
                // Refresh quickly until the first list and poll arrive, and
                // while an action runs, so its outcome shows promptly.
                let snap = src.lock().snapshot(SystemTime::now());
                let waiting = snap.pods_state == PodsState::Listing || snap.metrics == MetricsState::Waiting;
                app.apply(snap);
                let fast = waiting || app.busy();
                next_refresh = Instant::now() + if fast { Duration::from_millis(250) } else { REFRESH };
                dirty = true;
            }
            if dirty {
                terminal.draw(|f| ui::draw(f, &mut app, &palette))?;
                dirty = false;
            }
            while event::poll(next_refresh.saturating_duration_since(Instant::now()))? {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        app.on_key(k);
                        if let Some(a) = app.take_confirmed() {
                            let started = src.act(a.clone());
                            app.started(&a, started);
                            next_refresh = Instant::now();
                        }
                    }
                    Event::Resize(..) => {}
                    _ => continue,
                }
                dirty = true;
                if app.quit {
                    return Ok(());
                }
                // Draw before blocking again.
                break;
            }
        }
    })();
    ratatui::restore();
    result
}

fn parse_args() -> Result<Option<(Options, bool)>, String> {
    let mut opts = Options::default();
    let mut once = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--context" => opts.context = Some(value("--context")?),
            "-n" | "--namespace" => opts.namespace = Some(value("--namespace")?),
            "--once" => once = true,
            "--allow-actions" => opts.allow_actions = true,
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("kpiddle {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => {
                if let Some(v) = other.strip_prefix("--context=") {
                    opts.context = Some(v.into());
                } else if let Some(v) = other.strip_prefix("--namespace=") {
                    opts.namespace = Some(v.into());
                } else {
                    return Err(format!("unknown option {other}"));
                }
            }
        }
    }
    Ok(Some((opts, once)))
}

/// "OK", "STALE since 14:02:10: connection refused", "UNAVAILABLE: …".
pub fn metrics_text(m: &MetricsState) -> String {
    match m {
        MetricsState::Waiting => "waiting for the first poll…".into(),
        MetricsState::Ok => "OK".into(),
        MetricsState::Stale { last_success, reason } => {
            let us = last_success.duration_since(UNIX_EPOCH).unwrap_or_default().as_micros() as u64;
            let why = reason.as_ref().map_or(String::new(), |r| format!(": {r}"));
            format!("STALE since {}{why}", fmt_clock(us))
        }
        MetricsState::Unavailable { reason } => format!("UNAVAILABLE: {reason}"),
    }
}
