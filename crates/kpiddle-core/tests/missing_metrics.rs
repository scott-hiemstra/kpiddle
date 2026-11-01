//! Acceptance tests for kpiddle's first screen (docs/kpiddle/DESIGN.md):
//! missing or stale metrics never look like zero usage, in pods or totals.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kpiddle_core::model::{
    Cluster, MISSING, Measurement, Metric, MetricsState, Phase, Pod, PodSample, Snapshot, Usage, Workload, WorkloadKind,
};

fn t(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

/// Stands in for metrics-server: answers polls with samples for the pods it
/// knows, or with an error.
struct FakeMetrics {
    /// (namespace, pod name, CPU millicores)
    pods: Vec<(String, String, f64)>,
    error: Option<&'static str>,
}

impl FakeMetrics {
    fn poll(&self, cluster: &mut Cluster, at: SystemTime) {
        let result = match self.error {
            Some(e) => Err(e.to_string()),
            None => Ok(self
                .pods
                .iter()
                .map(|(ns, name, cpu)| PodSample {
                    namespace: ns.as_str().into(),
                    name: name.as_str().into(),
                    timestamp: at,
                    usage: Usage { cpu_millis: *cpu, mem_bytes: 64 << 20 },
                })
                .collect()),
        };
        cluster.apply_metrics(result, at);
    }
}

fn shop(name: &str, cpu: f64) -> (String, String, f64) {
    ("shop".into(), name.into(), cpu)
}

fn deployment(ns: &str, deploy: &str, n: usize) -> Vec<Pod> {
    (0..n)
        .map(|i| Pod {
            uid: format!("{ns}-{deploy}-{i}").into(),
            namespace: ns.into(),
            name: format!("{deploy}-{i}").into(),
            phase: Phase::Running,
            owner: Some(Workload { kind: WorkloadKind::Deployment, name: deploy.into() }),
            ..Default::default()
        })
        .collect()
}

/// Every piece of usage text the first screen shows.
fn all_usage_text(s: &Snapshot) -> Vec<String> {
    let mut out = vec![s.cluster.cpu_text(), s.cluster.mem_text()];
    for p in &s.pods {
        out.extend([p.usage.cpu_text(), p.usage.mem_text()]);
    }
    for ns in &s.namespaces {
        out.extend([ns.total.cpu_text(), ns.total.mem_text()]);
        for w in &ns.workloads {
            out.extend([w.total.cpu_text(), w.total.mem_text()]);
        }
    }
    out
}

fn cluster_with_web_pods() -> Cluster {
    let mut c = Cluster::new();
    c.replace_pods(deployment("shop", "web", 10).into_iter().chain(deployment("shop", "cart", 2)));
    c
}

#[test]
fn all_metrics_missing() {
    let mut c = cluster_with_web_pods();
    let source = FakeMetrics { pods: vec![], error: Some("the server could not find the requested resource") };
    source.poll(&mut c, t(0));
    let s = c.snapshot(t(5));

    assert!(s.pods.iter().all(|p| p.usage == Measurement::Missing));
    for text in all_usage_text(&s) {
        assert_eq!(text, MISSING, "usage text must be {MISSING} when nothing was measured");
    }
    assert!(s.namespaces.iter().all(|ns| ns.total.is_unmeasured() && ns.total.coverage_text().is_some()));
    assert_eq!(
        s.metrics,
        MetricsState::Unavailable { reason: "the server could not find the requested resource".into() }
    );
    assert!(s.suggest_requests_mode());
}

#[test]
fn partial_metrics() {
    let mut c = cluster_with_web_pods();
    // 7 of web's 10 pods are measured; both cart pods are.
    let mut pods: Vec<_> = (0..7).map(|i| shop(&format!("web-{i}"), 200.0)).collect();
    pods.extend([shop("cart-0", 50.0), shop("cart-1", 50.0)]);
    FakeMetrics { pods, error: None }.poll(&mut c, t(0));
    let s = c.snapshot(t(5));

    let web = s.namespaces[0].workloads.iter().find(|w| &*w.workload.name == "web").unwrap();
    assert!(web.total.is_partial());
    assert_eq!(web.total.cpu_text(), "≥ 1.4 cores");
    assert_eq!(web.total.coverage_text().as_deref(), Some("7/10 eligible pods measured"));

    let cart = s.namespaces[0].workloads.iter().find(|w| &*w.workload.name == "cart").unwrap();
    assert!(!cart.total.is_partial());
    assert_eq!(cart.total.cpu_text(), "100m");
    assert_eq!(cart.total.coverage_text(), None);

    let shop = &s.namespaces[0].total;
    assert_eq!(shop.cpu_text(), "≥ 1.5 cores");
    assert_eq!(shop.coverage_text().as_deref(), Some("9/12 eligible pods measured"));
    assert_eq!(s.metrics, MetricsState::Ok);
    assert!(!s.suggest_requests_mode());

    // Unmeasured pods sort after measured ones, ascending or descending, so
    // they never pass for the idlest pods.
    for descending in [true, false] {
        let mut order: Vec<_> = s.pods.iter().collect();
        order.sort_by(|a, b| a.usage.sort_key(Metric::Cpu).cmp(&b.usage.sort_key(Metric::Cpu), descending));
        let first_missing = order.iter().position(|p| p.usage == Measurement::Missing).unwrap();
        assert_eq!(first_missing, 9);
        assert!(order[first_missing..].iter().all(|p| p.usage == Measurement::Missing));
    }
}

#[test]
fn stale_metrics() {
    let mut c = cluster_with_web_pods();
    let mut source = FakeMetrics { pods: vec![shop("web-0", 300.0), shop("cart-0", 20.0)], error: None };
    source.poll(&mut c, t(0));
    // metrics-server goes away; polls fail from then on.
    source.error = Some("connection refused");
    source.poll(&mut c, t(30));
    source.poll(&mut c, t(170));
    let s = c.snapshot(t(180));

    let web0 = s.pods.iter().find(|p| &*p.pod.name == "web-0").unwrap();
    assert!(matches!(web0.usage, Measurement::Stale { .. }));
    assert_eq!(web0.usage.cpu_text(), "300m");
    assert_eq!(web0.usage.age_text().as_deref(), Some("3m old"));

    assert_eq!(s.metrics, MetricsState::Stale { last_success: t(0), reason: Some("connection refused".into()) });
    assert!(s.namespaces[0].total.is_stale());
    assert!(s.namespaces[0].workloads.iter().all(|w| w.total.is_stale()));
    assert!(s.cluster.is_stale());
}

#[test]
fn recovery_clears_stale_and_partial_marks() {
    let mut c = cluster_with_web_pods();
    let mut source = FakeMetrics { pods: vec![shop("web-0", 300.0)], error: None };
    source.poll(&mut c, t(0));
    source.error = Some("connection refused");
    source.poll(&mut c, t(120));
    let before = c.snapshot(t(180));
    assert!(before.cluster.is_stale() && before.cluster.is_partial());

    let every_pod = before.pods.iter().map(|p| shop(&p.pod.name, 100.0)).collect();
    source = FakeMetrics { pods: every_pod, error: None };
    source.poll(&mut c, t(195));
    let after = c.snapshot(t(200));

    assert_eq!(after.metrics, MetricsState::Ok);
    assert!(after.pods.iter().all(|p| matches!(p.usage, Measurement::Fresh(_))));
    assert!(after.pods.iter().all(|p| p.usage.age_text().is_none()));
    for total in std::iter::once(&after.cluster)
        .chain(after.namespaces.iter().map(|ns| &ns.total))
        .chain(after.namespaces.iter().flat_map(|ns| ns.workloads.iter().map(|w| &w.total)))
    {
        assert!(!total.is_stale() && !total.is_partial() && total.coverage_text().is_none());
        assert!(!total.cpu_text().starts_with('≥'));
    }
    assert_eq!(after.cluster.cpu_text(), "1.2 cores");
}
