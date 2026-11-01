//! `kpiddle --once`: one plain listing for scripts and pipes. It shows the
//! same snapshot as the interface: unmeasured usage reads `—`, totals say
//! how many pods they cover, and the metrics state comes first.

use std::fmt::Write as _;

use kpiddle_core::model::{Measurement, PodsState, Snapshot, Total};

use crate::metrics_text;

const NAME_W: usize = 52;

pub fn render(context: &str, namespace: Option<&str>, s: &Snapshot) -> String {
    let mut o = String::new();
    let scope = namespace.map_or("all namespaces".to_string(), |n| format!("namespace {n}"));
    let _ = writeln!(o, "kpiddle · context {context} · {scope}");
    let _ = writeln!(o, "metrics: {}", metrics_text(&s.metrics));
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

/// "  (7/10 eligible pods measured, 2 stale)" for a total that needs qualifying.
fn total_marks(t: &Total) -> String {
    let mut marks: Vec<String> = t.coverage_text().into_iter().collect();
    if t.is_stale() {
        marks.push(format!("{} stale", t.stale));
    }
    if t.not_running > 0 {
        marks.push(format!("{} not running", t.not_running));
    }
    if marks.is_empty() { String::new() } else { format!("  ({})", marks.join(", ")) }
}
