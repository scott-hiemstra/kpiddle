//! Pods, their metrics, and snapshots grouped by namespace and workload.
//!
//! Pods (from the API server) and usage samples (from metrics.k8s.io) arrive
//! independently. [`Cluster`] keeps the latest of each and joins them in
//! [`Cluster::snapshot`], which decides per pod whether its usage is fresh,
//! stale or missing at that moment. Usage is never filled in with zero, or
//! with requests, when a sample is absent.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use piddle_encode::{fmt_bytes, sanitize};

/// A sample at most this old is fresh. metrics-server scrapes about every
/// 15 s over a window of up to 30 s, so this is roughly two scrape cycles.
pub const FRESH_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// CPU in millicores; 1000 is one fully used core.
    pub cpu_millis: f64,
    /// Memory working set in bytes.
    pub mem_bytes: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Usage) {
        self.cpu_millis += o.cpu_millis;
        self.mem_bytes += o.mem_bytes;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Running,
    Succeeded,
    Failed,
    #[default]
    Unknown,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Pending => "Pending",
            Phase::Running => "Running",
            Phase::Succeeded => "Succeeded",
            Phase::Failed => "Failed",
            Phase::Unknown => "Unknown",
        }
    }

    /// metrics-server reports only pods whose containers run. `Unknown` (node
    /// unreachable) counts too, so a lost node shows as unmeasured pods
    /// rather than vanishing from coverage.
    pub fn expects_metrics(self) -> bool {
        matches!(self, Phase::Running | Phase::Unknown)
    }
}

/// The controller a pod belongs to, resolved to the top of its owner chain
/// (a Deployment rather than its ReplicaSet).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkloadKind {
    Deployment,
    StatefulSet,
    DaemonSet,
    CronJob,
    Job,
    ReplicaSet,
    ReplicationController,
    /// A controller kpiddle doesn't know, such as a custom resource.
    Other(Arc<str>),
    /// No controller: the pod is its own workload.
    Pod,
}

impl WorkloadKind {
    pub fn label(&self) -> &str {
        match self {
            WorkloadKind::Deployment => "deployment",
            WorkloadKind::StatefulSet => "statefulset",
            WorkloadKind::DaemonSet => "daemonset",
            WorkloadKind::CronJob => "cronjob",
            WorkloadKind::Job => "job",
            WorkloadKind::ReplicaSet => "replicaset",
            WorkloadKind::ReplicationController => "replicationcontroller",
            WorkloadKind::Other(kind) => kind,
            WorkloadKind::Pod => "pod",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Workload {
    pub kind: WorkloadKind,
    pub name: Arc<str>,
}

impl Workload {
    /// kubectl style: "deployment/web".
    pub fn describe(&self) -> String {
        format!("{}/{}", self.kind.label(), self.name)
    }
}

/// Requested or limited resources; `None` where the spec sets nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Resources {
    pub cpu_millis: Option<f64>,
    pub mem_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Container {
    pub name: Arc<str>,
    pub image: Arc<str>,
    pub requests: Resources,
    pub limits: Resources,
    pub restarts: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pod {
    pub uid: Arc<str>,
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    pub phase: Phase,
    /// Why the pod is unhealthy, when it is: "CrashLoopBackOff", "OOMKilled".
    pub reason: Option<Arc<str>>,
    /// `None` for a pod without a controller.
    pub owner: Option<Workload>,
    pub node: Option<Arc<str>>,
    pub ip: Option<Arc<str>>,
    pub created: Option<SystemTime>,
    pub containers: Vec<Container>,
}

impl Pod {
    /// The pod's controller, or the pod itself when it has none.
    pub fn workload(&self) -> Workload {
        self.owner.clone().unwrap_or_else(|| Workload { kind: WorkloadKind::Pod, name: self.name.clone() })
    }

    pub fn restarts(&self) -> u32 {
        self.containers.iter().map(|c| c.restarts).sum()
    }

    /// Names, labels, images and reasons are chosen by whoever creates the
    /// pod, so they are sanitized like a process's comm before display.
    fn sanitized(mut self) -> Pod {
        let clean = |s: &Arc<str>| -> Arc<str> { sanitize(s).into() };
        self.uid = clean(&self.uid);
        self.namespace = clean(&self.namespace);
        self.name = clean(&self.name);
        self.reason = self.reason.as_ref().map(clean);
        self.node = self.node.as_ref().map(clean);
        self.ip = self.ip.as_ref().map(clean);
        self.owner = self.owner.map(sanitize_workload);
        for c in &mut self.containers {
            c.name = clean(&c.name);
            c.image = clean(&c.image);
        }
        self
    }
}

fn sanitize_workload(w: Workload) -> Workload {
    let kind = match w.kind {
        WorkloadKind::Other(kind) => WorkloadKind::Other(sanitize(&kind).into()),
        k => k,
    };
    Workload { kind, name: sanitize(&w.name).into() }
}

/// One pod's usage from metrics.k8s.io, summed over its containers.
#[derive(Clone, Debug, PartialEq)]
pub struct PodSample {
    pub namespace: Arc<str>,
    pub name: Arc<str>,
    /// When metrics-server took the sample (the end of its window).
    pub timestamp: SystemTime,
    pub usage: Usage,
}

/// A pod's usage as of one snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Measurement {
    Fresh(Usage),
    /// The newest sample is older than [`FRESH_FOR`].
    Stale {
        usage: Usage,
        age: Duration,
    },
    /// The pod should have usage but there is no sample for it.
    Missing,
    /// The pod isn't running (pending or finished), so it has no usage.
    NotRunning,
}

impl Measurement {
    pub fn usage(&self) -> Option<Usage> {
        match *self {
            Measurement::Fresh(u) | Measurement::Stale { usage: u, .. } => Some(u),
            Measurement::Missing | Measurement::NotRunning => None,
        }
    }

    /// "250m", "1.5 cores"; "—" when missing, never "0m".
    pub fn cpu_text(&self) -> String {
        match self.usage() {
            Some(u) => fmt_cpu(u.cpu_millis),
            None => self.absent_text().into(),
        }
    }

    pub fn mem_text(&self) -> String {
        match self.usage() {
            Some(u) => fmt_bytes(u.mem_bytes),
            None => self.absent_text().into(),
        }
    }

    /// "3m old" for stale usage.
    pub fn age_text(&self) -> Option<String> {
        match self {
            Measurement::Stale { age, .. } => Some(format!("{} old", fmt_age(*age))),
            _ => None,
        }
    }

    pub fn sort_key(&self, metric: Metric) -> SortKey {
        match self {
            Measurement::Missing => SortKey::Unmeasured,
            Measurement::NotRunning => SortKey::NotRunning,
            m => SortKey::Measured(metric.of(m.usage().unwrap_or_default())),
        }
    }

    fn absent_text(&self) -> &'static str {
        if *self == Measurement::NotRunning { "" } else { MISSING }
    }
}

/// Shown wherever usage is unknown.
pub const MISSING: &str = "—";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Cpu,
    Mem,
}

impl Metric {
    fn of(self, u: Usage) -> f64 {
        match self {
            Metric::Cpu => u.cpu_millis,
            Metric::Mem => u.mem_bytes as f64,
        }
    }
}

/// Sorting by usage. Unmeasured items never sort as idle: they come after
/// every measured item, whichever way the measured ones are ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SortKey {
    Measured(f64),
    Unmeasured,
    NotRunning,
}

impl SortKey {
    pub fn cmp(&self, other: &SortKey, descending: bool) -> Ordering {
        let rank = |k: &SortKey| match k {
            SortKey::Measured(_) => 0,
            SortKey::Unmeasured => 1,
            SortKey::NotRunning => 2,
        };
        match (self, other) {
            (SortKey::Measured(a), SortKey::Measured(b)) => {
                let o = a.total_cmp(b);
                if descending { o.reverse() } else { o }
            }
            _ => rank(self).cmp(&rank(other)),
        }
    }
}

/// Usage summed over a group of pods, with how much of the group it covers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Total {
    /// Sum over measured (fresh or stale) members only.
    pub usage: Usage,
    /// Members with a fresh or stale sample.
    pub measured: usize,
    /// Members eligible for usage: running, or Unknown (node unreachable).
    pub expected: usize,
    /// Measured members whose sample is stale.
    pub stale: usize,
    /// Members that aren't running (pending or finished), so have no usage
    /// and are left out of `expected`.
    pub not_running: usize,
}

impl Total {
    pub fn add(&mut self, m: &Measurement) {
        if *m == Measurement::NotRunning {
            self.not_running += 1;
            return;
        }
        self.expected += 1;
        if let Some(u) = m.usage() {
            self.usage += u;
            self.measured += 1;
        }
        if matches!(m, Measurement::Stale { .. }) {
            self.stale += 1;
        }
    }

    /// Some members should have usage, but none was measured.
    pub fn is_unmeasured(&self) -> bool {
        self.expected > 0 && self.measured == 0
    }

    /// Some, but not all, members were measured: the sum is a lower bound.
    pub fn is_partial(&self) -> bool {
        self.measured > 0 && self.measured < self.expected
    }

    pub fn is_stale(&self) -> bool {
        self.stale > 0
    }

    /// "1.5 cores", "≥ 1.5 cores" when partial, "—" when unmeasured.
    pub fn cpu_text(&self) -> String {
        self.text(fmt_cpu(self.usage.cpu_millis))
    }

    pub fn mem_text(&self) -> String {
        self.text(fmt_bytes(self.usage.mem_bytes))
    }

    /// "7/10 eligible pods measured" when not every member was measured.
    pub fn coverage_text(&self) -> Option<String> {
        (self.measured < self.expected).then(|| format!("{}/{} eligible pods measured", self.measured, self.expected))
    }

    pub fn sort_key(&self, metric: Metric) -> SortKey {
        if self.expected == 0 {
            SortKey::NotRunning
        } else if self.measured == 0 {
            SortKey::Unmeasured
        } else {
            SortKey::Measured(metric.of(self.usage))
        }
    }

    fn text(&self, value: String) -> String {
        if self.expected == 0 {
            String::new()
        } else if self.measured == 0 {
            MISSING.into()
        } else if self.is_partial() {
            format!("≥ {value}")
        } else {
            value
        }
    }
}

/// Whether metrics.k8s.io is answering, shown in the header at all times.
#[derive(Clone, Debug, PartialEq)]
pub enum MetricsState {
    /// No poll has completed yet.
    Waiting,
    Ok,
    /// The last successful poll is older than [`FRESH_FOR`]. `reason` is the
    /// latest error, if polls have been failing.
    Stale {
        last_success: SystemTime,
        reason: Option<String>,
    },
    /// No poll has ever succeeded: no metrics.k8s.io API, forbidden, ...
    Unavailable {
        reason: String,
    },
}

/// Whether the pod list can be trusted, so an empty or old list isn't
/// mistaken for an empty or unchanged cluster.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PodsState {
    /// The first list hasn't completed.
    #[default]
    Listing,
    Synced,
    /// Watching pods is failing; the list is as of the last success.
    Failing {
        reason: String,
    },
}

/// The latest pods and samples, joined into snapshots on demand.
#[derive(Debug, Default)]
pub struct Cluster {
    pods: HashMap<Arc<str>, Arc<Pod>>,
    pods_state: PodsState,
    samples: HashMap<(Arc<str>, Arc<str>), PodSample>,
    /// (namespace, controller) → the controller that owns it, such as a
    /// ReplicaSet's Deployment or a Job's CronJob.
    owners: HashMap<(Arc<str>, Workload), Workload>,
    last_success: Option<SystemTime>,
    last_error: Option<String>,
}

impl Cluster {
    pub fn new() -> Cluster {
        Cluster::default()
    }

    pub fn upsert_pod(&mut self, pod: Pod) {
        let pod = pod.sanitized();
        self.pods.insert(pod.uid.clone(), Arc::new(pod));
    }

    pub fn remove_pod(&mut self, uid: &str) {
        if let Some(pod) = self.pods.remove(uid) {
            self.samples.remove(&(pod.namespace.clone(), pod.name.clone()));
        }
    }

    /// Replace every pod, as after a full relist.
    pub fn replace_pods(&mut self, pods: impl IntoIterator<Item = Pod>) {
        self.pods.clear();
        for pod in pods {
            self.upsert_pod(pod);
        }
        let live: std::collections::HashSet<_> =
            self.pods.values().map(|p| (p.namespace.clone(), p.name.clone())).collect();
        self.samples.retain(|k, _| live.contains(k));
    }

    pub fn set_pods_state(&mut self, state: PodsState) {
        self.pods_state = match state {
            PodsState::Failing { reason } => PodsState::Failing { reason: sanitize(&reason) },
            s => s,
        };
    }

    /// Record that `child` (a ReplicaSet, a Job) is owned by `parent`, or by
    /// nothing.
    pub fn set_owner(&mut self, namespace: &str, child: Workload, parent: Option<Workload>) {
        let key = (Arc::from(sanitize(namespace)), sanitize_workload(child));
        match parent {
            Some(p) => self.owners.insert(key, sanitize_workload(p)),
            None => self.owners.remove(&key),
        };
    }

    pub fn remove_owner(&mut self, namespace: &str, child: Workload) {
        self.owners.remove(&(Arc::from(sanitize(namespace)), sanitize_workload(child)));
    }

    /// Replace every owner record for controllers of `kind`, as after a
    /// full relist.
    pub fn replace_owners(
        &mut self,
        kind: &WorkloadKind,
        owners: impl IntoIterator<Item = (Arc<str>, Workload, Workload)>,
    ) {
        self.owners.retain(|(_, child), _| child.kind != *kind);
        for (ns, child, parent) in owners {
            self.set_owner(&ns, child, Some(parent));
        }
    }

    /// Record the outcome of one metrics poll that finished at `at`. Pods the
    /// poll left out keep their previous sample, which ages into stale.
    pub fn apply_metrics(&mut self, result: Result<Vec<PodSample>, String>, at: SystemTime) {
        match result {
            Ok(samples) => {
                for s in samples {
                    let key = (Arc::from(sanitize(&s.namespace)), Arc::from(sanitize(&s.name)));
                    if self.samples.get(&key).is_none_or(|old| old.timestamp <= s.timestamp) {
                        self.samples.insert(key, s);
                    }
                }
                self.last_success = Some(at);
                self.last_error = None;
            }
            Err(reason) => self.last_error = Some(sanitize(&reason)),
        }
    }

    pub fn metrics_state(&self, now: SystemTime) -> MetricsState {
        match (self.last_success, &self.last_error) {
            (None, None) => MetricsState::Waiting,
            (None, Some(reason)) => MetricsState::Unavailable { reason: reason.clone() },
            (Some(t), reason) if age(t, now) > FRESH_FOR => {
                MetricsState::Stale { last_success: t, reason: reason.clone() }
            }
            (Some(_), _) => MetricsState::Ok,
        }
    }

    pub fn snapshot(&self, now: SystemTime) -> Snapshot {
        let mut pods: Vec<PodView> = self
            .pods
            .values()
            .map(|pod| PodView { usage: self.measure(pod, now), workload: self.workload_of(pod), pod: pod.clone() })
            .collect();
        pods.sort_by(|a, b| {
            (&a.pod.namespace, &a.workload, &a.pod.name).cmp(&(&b.pod.namespace, &b.workload, &b.pod.name))
        });

        let mut cluster = Total::default();
        let mut namespaces: Vec<NamespaceGroup> = Vec::new();
        for (i, p) in pods.iter().enumerate() {
            cluster.add(&p.usage);
            if namespaces.last().is_none_or(|ns| ns.name != p.pod.namespace) {
                namespaces.push(NamespaceGroup { name: p.pod.namespace.clone(), ..Default::default() });
            }
            let ns = namespaces.last_mut().expect("pushed above");
            ns.total.add(&p.usage);
            if ns.workloads.last().is_none_or(|w| w.workload != p.workload) {
                ns.workloads.push(WorkloadGroup {
                    workload: p.workload.clone(),
                    total: Total::default(),
                    pods: Vec::new(),
                });
            }
            let w = ns.workloads.last_mut().expect("pushed above");
            w.total.add(&p.usage);
            w.pods.push(i);
        }

        Snapshot {
            taken: now,
            metrics: self.metrics_state(now),
            pods_state: self.pods_state.clone(),
            pods,
            namespaces,
            cluster,
        }
    }

    /// The top of the pod's owner chain as far as it is known: a Deployment
    /// once its ReplicaSet has been seen, the ReplicaSet until then.
    fn workload_of(&self, pod: &Pod) -> Workload {
        let mut w = pod.workload();
        // Real chains are at most two links (pod → ReplicaSet → Deployment);
        // the bound only guards against a cycle.
        for _ in 0..4 {
            match self.owners.get(&(pod.namespace.clone(), w.clone())) {
                Some(parent) => w = parent.clone(),
                None => break,
            }
        }
        w
    }

    fn measure(&self, pod: &Pod, now: SystemTime) -> Measurement {
        if !pod.phase.expects_metrics() {
            return Measurement::NotRunning;
        }
        let Some(s) = self.samples.get(&(pod.namespace.clone(), pod.name.clone())) else {
            return Measurement::Missing;
        };
        // A StatefulSet pod recreated under the same name must not inherit
        // the previous pod's sample.
        if pod.created.is_some_and(|c| s.timestamp < c) {
            return Measurement::Missing;
        }
        let age = age(s.timestamp, now);
        if age > FRESH_FOR { Measurement::Stale { usage: s.usage, age } } else { Measurement::Fresh(s.usage) }
    }
}

#[derive(Clone, Debug)]
pub struct PodView {
    pub pod: Arc<Pod>,
    pub workload: Workload,
    pub usage: Measurement,
}

#[derive(Clone, Debug)]
pub struct WorkloadGroup {
    pub workload: Workload,
    pub total: Total,
    /// Indices into [`Snapshot::pods`].
    pub pods: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct NamespaceGroup {
    pub name: Arc<str>,
    pub total: Total,
    pub workloads: Vec<WorkloadGroup>,
}

/// Everything a view needs for one frame.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub taken: SystemTime,
    pub metrics: MetricsState,
    pub pods_state: PodsState,
    /// Sorted by namespace, workload and name.
    pub pods: Vec<PodView>,
    pub namespaces: Vec<NamespaceGroup>,
    pub cluster: Total,
}

impl Snapshot {
    /// Pods should have usage but none has any: suggest the labeled
    /// requests/limits mode rather than showing a field of gray.
    pub fn suggest_requests_mode(&self) -> bool {
        self.cluster.is_unmeasured()
    }
}

/// Clock skew can put a sample in the future; treat that as brand new.
fn age(t: SystemTime, now: SystemTime) -> Duration {
    now.duration_since(t).unwrap_or_default()
}

/// "250m" below one core (`"<1m"` for a trickle), "1.5 cores" above.
pub fn fmt_cpu(millis: f64) -> String {
    if millis >= 1000.0 {
        format!("{:.1} cores", millis / 1000.0)
    } else if millis > 0.0 && millis < 1.0 {
        "<1m".into()
    } else {
        format!("{millis:.0}m")
    }
}

/// "45s", "3m", "2h".
pub fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_000_000 + secs)
    }

    fn pod(ns: &str, name: &str, phase: Phase) -> Pod {
        Pod { uid: format!("{ns}/{name}").into(), namespace: ns.into(), name: name.into(), phase, ..Default::default() }
    }

    fn sample(ns: &str, name: &str, at: SystemTime, cpu: f64) -> PodSample {
        PodSample {
            namespace: ns.into(),
            name: name.into(),
            timestamp: at,
            usage: Usage { cpu_millis: cpu, mem_bytes: 1 << 20 },
        }
    }

    #[test]
    fn not_running_pods_are_outside_coverage() {
        let mut c = Cluster::new();
        c.upsert_pod(pod("ns", "run", Phase::Running));
        c.upsert_pod(pod("ns", "wait", Phase::Pending));
        c.upsert_pod(pod("ns", "done", Phase::Succeeded));
        c.apply_metrics(Ok(vec![sample("ns", "run", t(0), 100.0)]), t(0));
        let s = c.snapshot(t(1));
        assert_eq!((s.cluster.measured, s.cluster.expected, s.cluster.not_running), (1, 1, 2));
        assert!(!s.cluster.is_partial());
        let wait = s.pods.iter().find(|p| &*p.pod.name == "wait").unwrap();
        assert_eq!(wait.usage, Measurement::NotRunning);
        assert_eq!(wait.usage.cpu_text(), "");
    }

    #[test]
    fn recreated_pod_does_not_inherit_old_sample() {
        let mut c = Cluster::new();
        c.apply_metrics(Ok(vec![sample("db", "pg-0", t(0), 300.0)]), t(0));
        c.upsert_pod(Pod { created: Some(t(10)), ..pod("db", "pg-0", Phase::Running) });
        assert_eq!(c.snapshot(t(20)).pods[0].usage, Measurement::Missing);
    }

    #[test]
    fn older_sample_does_not_replace_newer() {
        let mut c = Cluster::new();
        c.upsert_pod(pod("ns", "a", Phase::Running));
        c.apply_metrics(Ok(vec![sample("ns", "a", t(30), 200.0)]), t(30));
        c.apply_metrics(Ok(vec![sample("ns", "a", t(15), 100.0)]), t(31));
        assert_eq!(c.snapshot(t(31)).pods[0].usage.cpu_text(), "200m");
    }

    #[test]
    fn untrusted_text_is_sanitized() {
        let mut c = Cluster::new();
        c.upsert_pod(Pod {
            reason: Some("Crash\x1b[2J".into()),
            owner: Some(Workload { kind: WorkloadKind::Other("Ev\u{202E}il".into()), name: "x\ry".into() }),
            ..pod("ns", "a", Phase::Running)
        });
        c.apply_metrics(Err("forbidden\x1b]0;pwned\x07".into()), t(0));
        let s = c.snapshot(t(0));
        let p = &s.pods[0];
        assert_eq!(p.pod.reason.as_deref(), Some("Crash\u{FFFD}[2J"));
        assert_eq!(p.workload.describe(), "Evil/x\u{FFFD}y");
        assert_eq!(s.metrics, MetricsState::Unavailable { reason: "forbidden\u{FFFD}]0;pwned\u{FFFD}".into() });
    }

    #[test]
    fn owner_chain_resolves_to_top_controller() {
        let rs = Workload { kind: WorkloadKind::ReplicaSet, name: "web-7d9f".into() };
        let deploy = Workload { kind: WorkloadKind::Deployment, name: "web".into() };
        let mut c = Cluster::new();
        c.upsert_pod(Pod { owner: Some(rs.clone()), ..pod("shop", "web-7d9f-abc", Phase::Running) });
        // Until the ReplicaSet is seen, the pod groups under it.
        assert_eq!(c.snapshot(t(0)).pods[0].workload, rs);
        c.set_owner("shop", rs.clone(), Some(deploy.clone()));
        assert_eq!(c.snapshot(t(0)).pods[0].workload, deploy);
        // Same names in another namespace are a different ReplicaSet.
        c.upsert_pod(Pod {
            uid: "other".into(),
            owner: Some(rs.clone()),
            ..pod("dev", "web-7d9f-abc", Phase::Running)
        });
        let s = c.snapshot(t(0));
        assert_eq!(s.pods.iter().find(|p| &*p.pod.namespace == "dev").unwrap().workload, rs);
        // A relist without the ReplicaSet forgets its owner.
        c.replace_owners(&WorkloadKind::ReplicaSet, []);
        assert!(c.snapshot(t(0)).pods.iter().all(|p| p.workload == rs));
    }

    #[test]
    fn owner_cycle_terminates() {
        let a = Workload { kind: WorkloadKind::Other("A".into()), name: "a".into() };
        let b = Workload { kind: WorkloadKind::Other("B".into()), name: "b".into() };
        let mut c = Cluster::new();
        c.set_owner("ns", a.clone(), Some(b.clone()));
        c.set_owner("ns", b, Some(a.clone()));
        c.upsert_pod(Pod { owner: Some(a), ..pod("ns", "p", Phase::Running) });
        c.snapshot(t(0));
    }

    #[test]
    fn pods_state_is_reported_and_sanitized() {
        let mut c = Cluster::new();
        assert_eq!(c.snapshot(t(0)).pods_state, PodsState::Listing);
        c.set_pods_state(PodsState::Failing { reason: "forbidden\x1b[0m".into() });
        assert_eq!(c.snapshot(t(0)).pods_state, PodsState::Failing { reason: "forbidden\u{FFFD}[0m".into() });
    }

    #[test]
    fn bare_pod_is_its_own_workload() {
        let p = pod("ns", "debug", Phase::Running);
        assert_eq!(p.workload().describe(), "pod/debug");
    }

    #[test]
    fn remove_pod_drops_its_sample() {
        let mut c = Cluster::new();
        c.upsert_pod(pod("ns", "a", Phase::Running));
        c.apply_metrics(Ok(vec![sample("ns", "a", t(0), 1.0)]), t(0));
        c.remove_pod("ns/a");
        c.upsert_pod(pod("ns", "a", Phase::Running));
        assert_eq!(c.snapshot(t(1)).pods[0].usage, Measurement::Missing);
    }

    #[test]
    fn cpu_and_age_formatting() {
        assert_eq!(fmt_cpu(0.0), "0m");
        assert_eq!(fmt_cpu(0.4), "<1m");
        assert_eq!(fmt_cpu(250.0), "250m");
        assert_eq!(fmt_cpu(1500.0), "1.5 cores");
        assert_eq!(fmt_age(Duration::from_secs(45)), "45s");
        assert_eq!(fmt_age(Duration::from_secs(180)), "3m");
        assert_eq!(fmt_age(Duration::from_secs(7300)), "2h");
    }
}
