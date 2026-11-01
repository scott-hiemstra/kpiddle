//! `kpiddle`: pods by CPU and memory, grouped by namespace and workload.
//!
//! A plain refreshing listing until the Ratatui interface lands. It shows
//! the same snapshot the interface will: unmeasured usage reads `—`, totals
//! say how many pods they cover, and the metrics source state is always in
//! the header.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::time::{Duration, Instant, SystemTime};

use kpiddle_core::model::{Measurement, MetricsState, PodsState, Snapshot, Total, fmt_age};
use kpiddle_core::source::{self, Options, POLL_EVERY};

const HELP: &str = "\
kpiddle: Kubernetes pods by CPU and memory (read-only)

Usage: kpiddle [OPTIONS]

      --context NAME     kubeconfig context (default: the current one)
  -n, --namespace NAME   one namespace (default: all namespaces)
      --once             print one listing and exit
  -h, --help             show this help
  -V, --version

Usage comes from metrics.k8s.io (metrics-server). Where it is missing the
listing shows — rather than 0, and totals say how many pods they cover.";

const REFRESH: Duration = Duration::from_secs(5);
/// With --once, how long to wait for the first pod list and metrics poll.
const ONCE_WAIT: Duration = Duration::from_secs(20);

fn main() {
    let opts = match parse_args() {
        Ok(Some(o)) => o,
        Ok(None) => return,
        Err(e) => {
            eprintln!("kpiddle: {e}\n\n{HELP}");
            std::process::exit(2);
        }
    };
    let once = opts.1;
    let src = source::spawn(opts.0).unwrap_or_else(|e| {
        eprintln!("kpiddle: {e}");
        std::process::exit(1);
    });

    if once {
        let start = Instant::now();
        loop {
            let s = src.lock().snapshot(SystemTime::now());
            let ready = s.pods_state != PodsState::Listing && s.metrics != MetricsState::Waiting;
            if ready || start.elapsed() > ONCE_WAIT {
                print!("{}", render(&src.context, src.namespace.as_deref(), &s));
                return;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    let tty = std::io::stdout().is_terminal();
    loop {
        let s = src.lock().snapshot(SystemTime::now());
        let mut out = std::io::stdout().lock();
        if tty {
            // Home and clear, so each listing replaces the previous one.
            let _ = write!(out, "\x1b[H\x1b[2J");
        }
        let _ = write!(out, "{}", render(&src.context, src.namespace.as_deref(), &s));
        let _ = out.flush();
        drop(out);
        // Refresh quickly until the first list and poll have arrived.
        let waiting = s.pods_state == PodsState::Listing || s.metrics == MetricsState::Waiting;
        std::thread::sleep(if waiting { Duration::from_millis(500) } else { REFRESH });
    }
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

const NAME_W: usize = 52;

fn render(context: &str, namespace: Option<&str>, s: &Snapshot) -> String {
    let mut o = String::new();
    let scope = namespace.map_or("all namespaces".to_string(), |n| format!("namespace {n}"));
    let _ = writeln!(o, "kpiddle · context {context} · {scope}");
    let _ = writeln!(o, "{}", metrics_line(&s.metrics, s.taken));
    match &s.pods_state {
        PodsState::Listing => {
            let _ = writeln!(o, "pods: listing…");
            return o;
        }
        PodsState::Failing { reason } => {
            let _ = writeln!(o, "pods: watch failing, list may be out of date: {reason}");
        }
        PodsState::Synced => {}
    }
    let _ = writeln!(
        o,
        "{} pods · CPU {} · memory {}{}",
        s.pods.len(),
        s.cluster.cpu_text(),
        s.cluster.mem_text(),
        total_marks(&s.cluster)
    );
    if s.suggest_requests_mode() {
        let _ = writeln!(
            o,
            "No running pod has usage, so CPU and memory read —. A labeled requests/limits mode is planned."
        );
    }
    let _ = writeln!(o);
    let _ = writeln!(o, "{:<NAME_W$} {:>12} {:>11}  STATUS", "NAMESPACE / WORKLOAD / POD", "CPU", "MEMORY");
    for ns in &s.namespaces {
        row(&mut o, &ns.name, &ns.total.cpu_text(), &ns.total.mem_text(), total_marks(&ns.total).trim_start());
        for w in &ns.workloads {
            let name = format!("  {}", w.workload.describe());
            row(&mut o, &name, &w.total.cpu_text(), &w.total.mem_text(), total_marks(&w.total).trim_start());
            for &i in &w.pods {
                let p = &s.pods[i];
                let mut status = p.pod.reason.as_deref().unwrap_or(p.pod.phase.label()).to_string();
                let restarts = p.pod.restarts();
                if restarts > 0 {
                    let _ = write!(status, ", {restarts} restarts");
                }
                if let Some(age) = p.usage.age_text() {
                    let _ = write!(status, " · usage {age}");
                } else if p.usage == Measurement::Missing {
                    status.push_str(" · no usage sample");
                }
                row(&mut o, &format!("    {}", p.pod.name), &p.usage.cpu_text(), &p.usage.mem_text(), &status);
            }
        }
    }
    o
}

fn row(o: &mut String, name: &str, cpu: &str, mem: &str, status: &str) {
    let name = truncate(name, NAME_W);
    let _ = writeln!(o, "{name:<NAME_W$} {cpu:>12} {mem:>11}  {status}");
}

fn truncate(s: &str, w: usize) -> String {
    if s.chars().count() <= w { s.to_string() } else { s.chars().take(w - 1).chain(['…']).collect() }
}

/// "  (7/10 pods measured, stale)" for a total that needs qualifying.
fn total_marks(t: &Total) -> String {
    let mut marks: Vec<String> = t.coverage_text().into_iter().collect();
    if t.is_stale() {
        marks.push(format!("{} stale", t.stale));
    }
    if marks.is_empty() { String::new() } else { format!("  ({})", marks.join(", ")) }
}

fn metrics_line(m: &MetricsState, now: SystemTime) -> String {
    match m {
        MetricsState::Waiting => "metrics: waiting for the first poll…".into(),
        MetricsState::Ok => format!("metrics: OK (polled every {}s)", POLL_EVERY.as_secs()),
        MetricsState::Stale { last_success, reason } => {
            let ago = fmt_age(now.duration_since(*last_success).unwrap_or_default());
            let why = reason.as_ref().map_or(String::new(), |r| format!(": {r}"));
            format!("metrics: STALE, last update {ago} ago{why}")
        }
        MetricsState::Unavailable { reason } => format!("metrics: UNAVAILABLE: {reason}"),
    }
}
