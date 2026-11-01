//! TUI state: rows for the current view, selection, search, the details
//! popup and action confirmations. Rendering lives in `ui.rs`.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use kpiddle_core::actions::{Action, Assessment, Outcome, assess};
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
    /// Yes/no confirmation (default No).
    Confirm(Assessment),
    /// Risky action: the pod or workload name must be typed.
    ConfirmTyped(Assessment, String),
    Help,
}

/// How long a status message stays in the footer; action outcomes stay
/// longer, so a refusal can be read.
const STATUS_FOR: Duration = Duration::from_secs(4);
const OUTCOME_FOR: Duration = Duration::from_secs(12);

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
    /// Footer message and when it expires.
    pub status: Option<(String, Instant)>,
    /// Started with --allow-actions.
    pub actions: bool,
    /// A confirmed action for the main loop to start (see `take_confirmed`).
    confirmed: Option<Assessment>,
    /// Actions started and not finished yet, with what they're doing.
    running: Vec<(String, Receiver<Outcome>)>,
    /// UID of the pod in the details popup.
    pub detail_uid: Option<Arc<str>>,
    selected_key: Option<String>,
    collapsed: HashSet<String>,
    pub quit: bool,
}

impl App {
    pub fn new(context: String, namespace: Option<String>, actions: bool) -> App {
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
            actions,
            confirmed: None,
            running: Vec::new(),
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
        self.status = Some((s.into(), Instant::now() + STATUS_FOR));
    }

    /// The action the user just confirmed, for the main loop to start.
    pub fn take_confirmed(&mut self) -> Option<Assessment> {
        self.confirmed.take()
    }

    /// Track an action the main loop started (or say why it couldn't).
    pub fn started(&mut self, a: &Assessment, result: Result<Receiver<Outcome>, String>) {
        match result {
            Ok(rx) => {
                let doing = format!("{} {}…", a.action.label(), a.target.describe());
                self.status = Some((doing.clone(), Instant::now() + ACTION_WAIT));
                self.running.push((doing, rx));
            }
            Err(why) => self.status = Some((why, Instant::now() + OUTCOME_FOR)),
        }
    }

    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// Show the outcome of any action that finished. True if one did.
    pub fn poll_actions(&mut self) -> bool {
        let mut changed = false;
        self.running.retain(|(doing, rx)| match rx.try_recv() {
            Ok(o) => {
                let mark = if o.ok { "✓" } else { "✗" };
                self.status = Some((format!("{mark} {}", o.message), Instant::now() + OUTCOME_FOR));
                changed = true;
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => {
                self.status =
                    Some((format!("✗ {doing} lost: the cluster connection stopped."), Instant::now() + OUTCOME_FOR));
                changed = true;
                false
            }
        });
        changed
    }

    /// Ask to run `action` on `target` (the pod in Details, or the selected
    /// row): refused, a yes/no, or type-the-name. The caller picks the pod,
    /// because the list selection can differ from the Details pod.
    fn request_action(&mut self, action: Action, target: Option<PodView>) {
        let Some(snap) = &self.snap else { return };
        let Some(pv) = target else { return };
        if !self.actions {
            self.set_status("Read-only: start kpiddle with --allow-actions to evict, delete or restart.");
            return;
        }
        let a = assess(snap, &pv, action);
        if let Some(why) = &a.blocked {
            self.status = Some((format!("Not allowed: {why}"), Instant::now() + OUTCOME_FOR));
            return;
        }
        self.mode = if a.is_critical() { Mode::ConfirmTyped(a, String::new()) } else { Mode::Confirm(a) };
    }

    fn selected_pod(&self) -> Option<PodView> {
        let snap = self.snap.as_ref()?;
        match self.table.selected().and_then(|i| self.rows.get(i))? {
            Row::Pod { idx, .. } => Some(snap.pods[*idx].clone()),
            _ => None,
        }
    }

    fn confirm(&mut self, a: Assessment) {
        self.confirmed = Some(a);
        self.mode = Mode::Normal;
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
                KeyCode::Char(c @ ('e' | 'd' | 'r')) => match self.detail_pod().cloned() {
                    Some(p) => {
                        self.request_action(action_for(c), Some(p));
                        // Refused or read-only: stay in the popup.
                        if matches!(self.mode, Mode::Normal) {
                            self.mode = Mode::Details;
                        }
                    }
                    // Gone: keep showing the popup that says so.
                    None => self.mode = Mode::Details,
                },
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Esc | KeyCode::Enter => self.detail_uid = None,
                _ => self.mode = Mode::Details,
            },
            Mode::Confirm(a) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.confirm(a);
                } else {
                    self.set_status("Cancelled.");
                }
            }
            Mode::ConfirmTyped(a, mut typed) => match key.code {
                KeyCode::Esc => self.set_status("Cancelled."),
                KeyCode::Enter if typed.trim() == a.target.name() => self.confirm(a),
                KeyCode::Enter => self.mode = Mode::ConfirmTyped(a, typed),
                KeyCode::Backspace => {
                    typed.pop();
                    self.mode = Mode::ConfirmTyped(a, typed);
                }
                KeyCode::Char(c) => {
                    typed.push(c);
                    self.mode = Mode::ConfirmTyped(a, typed);
                }
                _ => self.mode = Mode::ConfirmTyped(a, typed),
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
            KeyCode::Char(c @ ('e' | 'd' | 'r')) => {
                let pod = self.selected_pod();
                self.request_action(action_for(c), pod);
            }
            _ => {}
        }
    }
}

/// While an action runs, its "Evicting…" line stays up until the outcome.
const ACTION_WAIT: Duration = Duration::from_secs(60);

fn action_for(key: char) -> Action {
    match key {
        'e' => Action::Evict,
        'd' => Action::Delete,
        _ => Action::RolloutRestart,
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
        let mut app = App::new("test".into(), None, true);
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

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::from(c)
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    /// Select a pod by name in the pods view.
    fn select(app: &mut App, name: &str) {
        app.switch_view(View::Pods);
        let snap = app.snap.as_ref().unwrap();
        let i = app.rows.iter().position(|r| matches!(r, Row::Pod { idx, .. } if &*snap.pods[*idx].pod.name == name));
        app.table.select(i);
    }

    // Nothing here reaches a cluster: only what would be sent is checked.

    #[test]
    fn read_only_refuses_actions() {
        let c = cluster();
        let mut app = App::new("test".into(), None, false);
        app.apply(c.snapshot(t(1)));
        select(&mut app, "busy-1");
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.contains("--allow-actions")));
        assert!(app.take_confirmed().is_none());
    }

    #[test]
    fn evict_asks_yes_no_and_defaults_to_no() {
        let mut app = app_with(&cluster());
        select(&mut app, "busy-1");
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(&app.mode, Mode::Confirm(a) if a.action == Action::Evict));
        app.on_key(key(KeyCode::Enter)); // anything but y cancels
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.take_confirmed().is_none());

        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('y')));
        let a = app.take_confirmed().unwrap();
        assert_eq!(a.target.describe(), "a/busy-1");
    }

    #[test]
    fn delete_and_bare_pods_need_the_name() {
        let mut app = app_with(&cluster());
        select(&mut app, "busy-1");
        app.on_key(key(KeyCode::Char('d')));
        assert!(matches!(app.mode, Mode::ConfirmTyped(..)));
        type_text(&mut app, "busy");
        app.on_key(key(KeyCode::Enter)); // wrong name: still asking
        assert!(matches!(app.mode, Mode::ConfirmTyped(..)));
        type_text(&mut app, "-1");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.take_confirmed().map(|a| a.action), Some(Action::Delete));

        // "solo" has no controller: evicting it is permanent.
        select(&mut app, "solo");
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.mode, Mode::ConfirmTyped(..)));
        app.on_key(key(KeyCode::Esc));
        assert!(matches!(app.mode, Mode::Normal) && app.take_confirmed().is_none());
    }

    #[test]
    fn details_actions_target_the_details_pod() {
        let mut app = app_with(&cluster());
        select(&mut app, "idle-1");
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.mode, Mode::Details));
        // The list selection moves; the popup still shows idle-1.
        select(&mut app, "busy-1");
        app.mode = Mode::Details;
        app.on_key(key(KeyCode::Char('r')));
        let Mode::Confirm(a) = &app.mode else { panic!("expected a confirm") };
        assert_eq!(a.target.describe(), "a/deployment/idle");
        // Static pods are refused in the popup, which stays open.
        let mut c = cluster();
        c.upsert_pod(Pod { static_pod: true, ..pod("kube-system", "etcd-cp", None) });
        let mut app = app_with(&c);
        select(&mut app, "etcd-cp");
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('e')));
        assert!(matches!(app.mode, Mode::Details));
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("Not allowed: Static pod")));
    }

    #[test]
    fn outcomes_show_in_the_footer() {
        let mut app = app_with(&cluster());
        select(&mut app, "busy-1");
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Char('y')));
        let a = app.take_confirmed().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        app.started(&a, Ok(rx));
        assert!(app.busy() && !app.poll_actions());
        assert!(app.status.as_ref().is_some_and(|(s, _)| s == "Evict a/busy-1…"));
        tx.send(Outcome { ok: false, message: "Not evicted: a PodDisruptionBudget says no".into() }).unwrap();
        assert!(app.poll_actions() && !app.busy());
        assert!(app.status.as_ref().is_some_and(|(s, _)| s.starts_with("✗ Not evicted")));
    }

    #[test]
    fn collapse_hides_pods() {
        let mut app = app_with(&cluster());
        app.move_by(1); // w busy
        app.on_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(labels(&app)[..3], ["ns a", "w busy", "w idle"]);
    }
}
