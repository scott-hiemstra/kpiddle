//! TUI state: rows for the current view, selection, search and the details
//! popup. Rendering lives in `ui.rs`.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use kpiddle_core::model::{Metric, PodView, Snapshot, SortKey, Total, Workload, WorkloadKind};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::TableState;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// Namespace → workload → pod.
    Workloads,
    Pods,
}

impl View {
    pub fn name(self) -> &'static str {
        match self {
            View::Workloads => "by workload",
            View::Pods => "pods",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    Cpu,
    Mem,
    Name,
}

impl Sort {
    pub fn name(self) -> &'static str {
        match self {
            Sort::Cpu => "CPU",
            Sort::Mem => "memory",
            Sort::Name => "name",
        }
    }

    fn next(self) -> Sort {
        match self {
            Sort::Cpu => Sort::Mem,
            Sort::Mem => Sort::Name,
            Sort::Name => Sort::Cpu,
        }
    }

    fn metric(self) -> Option<Metric> {
        match self {
            Sort::Cpu => Some(Metric::Cpu),
            Sort::Mem => Some(Metric::Mem),
            Sort::Name => None,
        }
    }
}

pub enum Row {
    Namespace {
        name: Arc<str>,
        total: Total,
        pods: usize,
        collapsed: bool,
    },
    Workload {
        namespace: Arc<str>,
        workload: Workload,
        total: Total,
        pods: usize,
        collapsed: bool,
    },
    /// `idx` points into `Snapshot::pods`; `depth` is the indentation level.
    Pod {
        idx: usize,
        depth: u8,
    },
}

impl Row {
    /// Identifies the row across rebuilds, to keep the selection on it.
    fn key(&self, snap: &Snapshot) -> String {
        match self {
            Row::Namespace { name, .. } => ns_key(name),
            Row::Workload { namespace, workload, .. } => workload_key(namespace, workload),
            Row::Pod { idx, .. } => format!("p/{}", snap.pods[*idx].pod.uid),
        }
    }
}

fn ns_key(ns: &str) -> String {
    format!("n/{ns}")
}

fn workload_key(ns: &str, w: &Workload) -> String {
    format!("w/{ns}/{}/{}", w.kind.label(), w.name)
}

/// A workload, the total over its visible pods, and those pods.
type WorkloadPods = (Workload, Total, Vec<usize>);

pub enum Mode {
    Normal,
    Search,
    Details,
    Help,
}

pub struct App {
    pub context: String,
    pub namespace: Option<String>,
    pub snap: Option<Snapshot>,
    pub rows: Vec<Row>,
    pub view: View,
    pub sort: Sort,
    pub query: String,
    pub table: TableState,
    pub mode: Mode,
    pub status: Option<(String, Instant)>,
    /// UID of the pod in the details popup.
    pub detail_uid: Option<Arc<str>>,
    selected_key: Option<String>,
    collapsed: HashSet<String>,
    pub quit: bool,
}

impl App {
    pub fn new(context: String, namespace: Option<String>) -> App {
        App {
            context,
            namespace,
            snap: None,
            rows: Vec::new(),
            view: View::Workloads,
            sort: Sort::Cpu,
            query: String::new(),
            table: TableState::default().with_selected(Some(0)),
            mode: Mode::Normal,
            status: None,
            detail_uid: None,
            selected_key: None,
            collapsed: HashSet::new(),
            quit: false,
        }
    }

    pub fn apply(&mut self, snap: Snapshot) {
        self.snap = Some(snap);
        self.rebuild();
    }

    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some((s.into(), Instant::now()));
    }

    /// Latest data for the pod in the details popup; `None` once it's gone.
    pub fn detail_pod(&self) -> Option<&PodView> {
        let uid = self.detail_uid.as_ref()?;
        self.snap.as_ref()?.pods.iter().find(|p| &p.pod.uid == uid)
    }

    fn visible(&self, p: &PodView) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let q = self.query.to_lowercase();
        [&p.pod.name, &p.pod.namespace, &p.workload.name].iter().any(|s| s.to_lowercase().contains(&q))
            || p.pod.node.as_ref().is_some_and(|n| n.to_lowercase().contains(&q))
    }

    /// Order by the sort key, then by name. Unmeasured items come after
    /// measured ones (see `SortKey`), so they never pass for idle.
    fn order(&self, a: (SortKey, &str), b: (SortKey, &str)) -> Ordering {
        let by_name = a.1.cmp(b.1);
        match self.sort.metric() {
            Some(_) => a.0.cmp(&b.0, true).then(by_name),
            None => by_name,
        }
    }

    fn pod_key<'a>(&self, p: &'a PodView) -> (SortKey, &'a str) {
        (self.sort.metric().map_or(SortKey::NotRunning, |m| p.usage.sort_key(m)), &p.pod.name)
    }

    fn total_key(&self, t: &Total) -> SortKey {
        self.sort.metric().map_or(SortKey::NotRunning, |m| t.sort_key(m))
    }

    /// Recompute rows for the current view, keeping the selection on the
    /// same namespace, workload or pod.
    pub fn rebuild(&mut self) {
        let Some(snap) = &self.snap else { return };
        let mut pods: Vec<usize> = (0..snap.pods.len()).filter(|&i| self.visible(&snap.pods[i])).collect();
        pods.sort_by(|&a, &b| self.order(self.pod_key(&snap.pods[a]), self.pod_key(&snap.pods[b])));
        let mut rows = Vec::with_capacity(pods.len() + 32);

        match self.view {
            View::Pods => rows.extend(pods.iter().map(|&idx| Row::Pod { idx, depth: 0 })),
            View::Workloads => {
                // Group the (already sorted) pods; totals cover the visible pods.
                let mut groups: Vec<(Arc<str>, Total, Vec<WorkloadPods>)> = Vec::new();
                for &i in &pods {
                    let p = &snap.pods[i];
                    let ns = match groups.iter_mut().position(|g| g.0 == p.pod.namespace) {
                        Some(n) => n,
                        None => {
                            groups.push((p.pod.namespace.clone(), Total::default(), Vec::new()));
                            groups.len() - 1
                        }
                    };
                    let g = &mut groups[ns];
                    g.1.add(&p.usage);
                    let w = match g.2.iter_mut().position(|w| w.0 == p.workload) {
                        Some(w) => w,
                        None => {
                            g.2.push((p.workload.clone(), Total::default(), Vec::new()));
                            g.2.len() - 1
                        }
                    };
                    g.2[w].1.add(&p.usage);
                    g.2[w].2.push(i);
                }
                groups.sort_by(|a, b| self.order((self.total_key(&a.1), &a.0), (self.total_key(&b.1), &b.0)));
                for (ns, total, mut workloads) in groups {
                    let collapsed = self.collapsed.contains(&ns_key(&ns));
                    let count = workloads.iter().map(|w| w.2.len()).sum();
                    rows.push(Row::Namespace { name: ns.clone(), total, pods: count, collapsed });
                    if collapsed {
                        continue;
                    }
                    workloads.sort_by(|a, b| {
                        self.order((self.total_key(&a.1), &a.0.name), (self.total_key(&b.1), &b.0.name))
                    });
                    for (workload, total, members) in workloads {
                        // A pod without a controller is its own row.
                        if workload.kind == WorkloadKind::Pod {
                            rows.extend(members.iter().map(|&idx| Row::Pod { idx, depth: 1 }));
                            continue;
                        }
                        let collapsed = self.collapsed.contains(&workload_key(&ns, &workload));
                        let n = members.len();
                        rows.push(Row::Workload { namespace: ns.clone(), workload, total, pods: n, collapsed });
                        if !collapsed {
                            rows.extend(members.iter().map(|&idx| Row::Pod { idx, depth: 2 }));
                        }
                    }
                }
            }
        }

        let idx = self
            .selected_key
            .as_ref()
            .and_then(|k| rows.iter().position(|r| &r.key(snap) == k))
            .or(self.table.selected())
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.table.select(Some(idx));
    }

    fn remember_selection(&mut self) {
        let (Some(snap), Some(i)) = (&self.snap, self.table.selected()) else { return };
        self.selected_key = self.rows.get(i).map(|r| r.key(snap));
    }

    fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let cur = self.table.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, self.rows.len() as isize - 1) as usize;
        self.table.select(Some(next));
        self.remember_selection();
    }

    fn toggle_collapse(&mut self) {
        let key = match self.table.selected().and_then(|i| self.rows.get(i)) {
            Some(Row::Namespace { name, .. }) => ns_key(name),
            Some(Row::Workload { namespace, workload, .. }) => workload_key(namespace, workload),
            _ => return,
        };
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        self.rebuild();
    }

    /// Collapse every workload, or expand everything if any is collapsed.
    fn toggle_all_workloads(&mut self) {
        let Some(snap) = &self.snap else { return };
        if self.collapsed.is_empty() {
            self.collapsed = snap
                .pods
                .iter()
                .filter(|p| p.workload.kind != WorkloadKind::Pod)
                .map(|p| workload_key(&p.pod.namespace, &p.workload))
                .collect();
            self.set_status("Workloads collapsed");
        } else {
            self.collapsed.clear();
            self.set_status("All expanded");
        }
        self.rebuild();
    }

    fn open_details(&mut self) {
        let Some(snap) = &self.snap else { return };
        if let Some(Row::Pod { idx, .. }) = self.table.selected().and_then(|i| self.rows.get(i)) {
            self.detail_uid = Some(snap.pods[*idx].pod.uid.clone());
            self.mode = Mode::Details;
        }
    }

    fn switch_view(&mut self, view: View) {
        self.view = view;
        self.rebuild();
        self.set_status(format!("View: {}", view.name()));
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Search => match key.code {
                KeyCode::Esc => {
                    self.query.clear();
                    self.rebuild();
                }
                KeyCode::Enter => {}
                KeyCode::Backspace => {
                    self.query.pop();
                    self.rebuild();
                    self.mode = Mode::Search;
                }
                KeyCode::Char(c) => {
                    self.query.push(c);
                    self.rebuild();
                    self.mode = Mode::Search;
                }
                KeyCode::Up | KeyCode::Down => {
                    self.move_by(if key.code == KeyCode::Up { -1 } else { 1 });
                    self.mode = Mode::Search;
                }
                _ => self.mode = Mode::Search,
            },
            Mode::Details => match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc | KeyCode::Enter => self.detail_uid = None,
                _ => self.mode = Mode::Details,
            },
            Mode::Help => {}
            Mode::Normal => self.on_normal_key(key),
        }
    }

    fn on_normal_key(&mut self, key: KeyEvent) {
        let page = 15;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.rebuild();
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Char('?') | KeyCode::F(1) => self.mode = Mode::Help,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-page),
            KeyCode::PageDown => self.move_by(page),
            KeyCode::Home | KeyCode::Char('g') => self.move_by(-(self.rows.len() as isize)),
            KeyCode::End | KeyCode::Char('G') => self.move_by(self.rows.len() as isize),
            KeyCode::Enter => match self.table.selected().and_then(|i| self.rows.get(i)) {
                Some(Row::Pod { .. }) => self.open_details(),
                Some(_) => self.toggle_collapse(),
                None => {}
            },
            KeyCode::Char(' ') => self.toggle_collapse(),
            KeyCode::Char('c') => self.toggle_all_workloads(),
            KeyCode::Char('v') => {
                let next = if self.view == View::Workloads { View::Pods } else { View::Workloads };
                self.switch_view(next);
            }
            KeyCode::Char('1') => self.switch_view(View::Workloads),
            KeyCode::Char('2') => self.switch_view(View::Pods),
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.rebuild();
                self.set_status(format!("Sorted by {}", self.sort.name()));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use kpiddle_core::model::{Cluster, Phase, Pod, PodSample, Usage};
    use ratatui::crossterm::event::KeyEvent;

    use super::*;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    fn pod(ns: &str, name: &str, owner: Option<&str>) -> Pod {
        Pod {
            uid: format!("{ns}/{name}").into(),
            namespace: ns.into(),
            name: name.into(),
            phase: Phase::Running,
            owner: owner.map(|o| Workload { kind: WorkloadKind::Deployment, name: o.into() }),
            ..Default::default()
        }
    }

    fn sample(ns: &str, name: &str, cpu: f64) -> PodSample {
        PodSample {
            namespace: ns.into(),
            name: name.into(),
            timestamp: t(0),
            usage: Usage { cpu_millis: cpu, mem_bytes: 1 },
        }
    }

    fn app_with(cluster: &Cluster) -> App {
        let mut app = App::new("test".into(), None);
        app.apply(cluster.snapshot(t(1)));
        app
    }

    fn labels(app: &App) -> Vec<String> {
        let snap = app.snap.as_ref().unwrap();
        app.rows
            .iter()
            .map(|r| match r {
                Row::Namespace { name, .. } => format!("ns {name}"),
                Row::Workload { workload, .. } => format!("w {}", workload.name),
                Row::Pod { idx, .. } => format!("p {}", snap.pods[*idx].pod.name),
            })
            .collect()
    }

    fn cluster() -> Cluster {
        let mut c = Cluster::new();
        c.replace_pods([
            pod("a", "idle-1", Some("idle")),
            pod("a", "busy-1", Some("busy")),
            pod("a", "dark-1", Some("dark")),
            pod("b", "solo", None),
        ]);
        c.apply_metrics(
            Ok(vec![sample("a", "idle-1", 0.0), sample("a", "busy-1", 900.0), sample("b", "solo", 5.0)]),
            t(0),
        );
        c
    }

    #[test]
    fn unmeasured_sorts_last_even_below_zero() {
        let app = app_with(&cluster());
        assert_eq!(
            labels(&app),
            ["ns a", "w busy", "p busy-1", "w idle", "p idle-1", "w dark", "p dark-1", "ns b", "p solo"]
        );
        let mut app = app;
        app.switch_view(View::Pods);
        assert_eq!(labels(&app), ["p busy-1", "p solo", "p idle-1", "p dark-1"]);
    }

    #[test]
    fn search_narrows_rows_and_totals() {
        let mut app = app_with(&cluster());
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "dark".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        assert_eq!(labels(&app), ["ns a", "w dark", "p dark-1"]);
        let Row::Namespace { total, .. } = &app.rows[0] else { panic!() };
        assert!(total.is_unmeasured());
    }

    #[test]
    fn selection_follows_the_pod_across_rebuilds() {
        let mut c = cluster();
        let mut app = app_with(&c);
        app.switch_view(View::Pods);
        app.move_by(2); // idle-1
        c.apply_metrics(Ok(vec![sample("a", "idle-1", 2000.0)]), t(0));
        app.apply(c.snapshot(t(1)));
        let Some(Row::Pod { idx, .. }) = app.table.selected().map(|i| &app.rows[i]) else { panic!() };
        assert_eq!(&*app.snap.as_ref().unwrap().pods[*idx].pod.name, "idle-1");
        assert_eq!(app.table.selected(), Some(0));
    }

    #[test]
    fn collapse_hides_pods() {
        let mut app = app_with(&cluster());
        app.move_by(1); // w busy
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(labels(&app)[..3], ["ns a", "w busy", "w idle"]);
    }
}
