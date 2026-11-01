//! Feeds a [`Cluster`] from a live API server.
//!
//! Pods are watched, ReplicaSets and Jobs are watched as metadata only (to
//! resolve pods to their Deployments and CronJobs), and metrics.k8s.io is
//! polled every [`POLL_EVERY`]. Everything runs on one background thread;
//! views lock the cluster and take a [`Cluster::snapshot`] per frame.
//!
//! Credentials are only ever handed to kube-rs. Nothing here prints the
//! kubeconfig, and error text is sanitized by the [`Cluster`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use futures::StreamExt;
use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Pod as KPod, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, PartialObjectMeta};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::runtime::WatchStreamExt;
use kube::runtime::watcher::{self, Event, watcher};
use kube::{Api, Client, Config, Resource};
use serde::Deserialize;

use crate::model::{Cluster, Container, Phase, Pod, PodSample, PodsState, Resources, Usage, Workload, WorkloadKind};
use crate::quantity;

/// metrics-server refreshes about every 15 s, so polling faster buys nothing.
pub const POLL_EVERY: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// kubeconfig context; the current one when `None`.
    pub context: Option<String>,
    /// Watch one namespace; all namespaces when `None`.
    pub namespace: Option<String>,
}

/// A running connection to one cluster.
pub struct Source {
    pub cluster: Arc<Mutex<Cluster>>,
    /// The kubeconfig context name, or "in-cluster".
    pub context: String,
    pub namespace: Option<String>,
}

impl Source {
    pub fn lock(&self) -> MutexGuard<'_, Cluster> {
        lock(&self.cluster)
    }
}

/// Connect using the kubeconfig (or the in-cluster service account) and
/// start feeding a [`Cluster`] on a background thread. Fails only if no
/// client can be configured; API errors after that show up in the
/// snapshot's [`PodsState`] and metrics state.
pub fn spawn(opts: Options) -> Result<Source, String> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let (client, context) = rt.block_on(connect(&opts))?;
    let cluster = Arc::new(Mutex::new(Cluster::new()));
    let feed = cluster.clone();
    let namespace = opts.namespace.clone();
    std::thread::Builder::new()
        .name("kpiddle-source".into())
        .spawn(move || rt.block_on(run(client, namespace, feed)))
        .map_err(|e| e.to_string())?;
    Ok(Source { cluster, context, namespace: opts.namespace })
}

async fn connect(opts: &Options) -> Result<(Client, String), String> {
    let config = match &opts.context {
        Some(ctx) => {
            let o = KubeConfigOptions { context: Some(ctx.clone()), ..Default::default() };
            Config::from_kubeconfig(&o).await.map_err(|e| format!("kubeconfig context {ctx}: {e}"))?
        }
        None => Config::infer().await.map_err(|e| format!("no cluster configured: {e}"))?,
    };
    let context = opts
        .context
        .clone()
        .or_else(|| Kubeconfig::read().ok().and_then(|k| k.current_context))
        .unwrap_or_else(|| "in-cluster".into());
    let client = Client::try_from(config).map_err(|e| e.to_string())?;
    Ok((client, context))
}

async fn run(client: Client, ns: Option<String>, cluster: Arc<Mutex<Cluster>>) {
    tokio::join!(
        watch_pods(api(&client, &ns), cluster.clone()),
        watch_owners(api::<PartialObjectMeta<ReplicaSet>>(&client, &ns), WorkloadKind::ReplicaSet, cluster.clone()),
        watch_owners(api::<PartialObjectMeta<Job>>(&client, &ns), WorkloadKind::Job, cluster.clone()),
        poll_metrics(client, ns, cluster),
    );
}

fn api<K>(client: &Client, ns: &Option<String>) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>,
{
    match ns {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

fn lock(cluster: &Mutex<Cluster>) -> MutexGuard<'_, Cluster> {
    // A panic while holding the lock leaves plain data behind; keep going.
    cluster.lock().unwrap_or_else(|e| e.into_inner())
}

async fn watch_pods(api: Api<KPod>, cluster: Arc<Mutex<Cluster>>) {
    let mut events = watcher(api, watcher::Config::default()).default_backoff().boxed();
    let mut relist = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => relist.clear(),
            Ok(Event::InitApply(p)) => relist.extend(convert_pod(&p)),
            Ok(Event::InitDone) => {
                let mut c = lock(&cluster);
                c.replace_pods(relist.drain(..));
                c.set_pods_state(PodsState::Synced);
            }
            Ok(Event::Apply(p)) => {
                let mut c = lock(&cluster);
                c.upsert_pod(convert_pod(&p).unwrap_or_default());
                c.set_pods_state(PodsState::Synced);
            }
            Ok(Event::Delete(p)) => {
                let mut c = lock(&cluster);
                if let Some(uid) = &p.metadata.uid {
                    c.remove_pod(uid);
                }
                c.set_pods_state(PodsState::Synced);
            }
            Err(e) => lock(&cluster).set_pods_state(PodsState::Failing { reason: describe_watch_error(&e) }),
        }
    }
}

/// Track which controller owns each ReplicaSet or Job. Failures only cost
/// grouping (pods stay under their ReplicaSet), so they aren't surfaced.
async fn watch_owners<K>(api: Api<PartialObjectMeta<K>>, kind: WorkloadKind, cluster: Arc<Mutex<Cluster>>)
where
    K: Resource<DynamicType = ()> + Clone + serde::de::DeserializeOwned + std::fmt::Debug + Send + 'static,
{
    let mut events = watcher(api, watcher::Config::default()).default_backoff().boxed();
    let child = |m: &ObjectMeta| Workload { kind: kind.clone(), name: m.name.clone().unwrap_or_default().into() };
    let mut relist = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            Ok(Event::Init) => relist.clear(),
            Ok(Event::InitApply(o)) => {
                if let Some(parent) = controller(&o.metadata) {
                    relist.push((namespace(&o.metadata), child(&o.metadata), parent));
                }
            }
            Ok(Event::InitDone) => lock(&cluster).replace_owners(&kind, relist.drain(..)),
            Ok(Event::Apply(o)) => {
                lock(&cluster).set_owner(&namespace(&o.metadata), child(&o.metadata), controller(&o.metadata))
            }
            Ok(Event::Delete(o)) => lock(&cluster).remove_owner(&namespace(&o.metadata), child(&o.metadata)),
            Err(_) => {}
        }
    }
}

async fn poll_metrics(client: Client, ns: Option<String>, cluster: Arc<Mutex<Cluster>>) {
    let gvk = GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics");
    let ar = ApiResource::from_gvk_with_plural(&gvk, "pods");
    let api: Api<DynamicObject> = match &ns {
        Some(ns) => Api::namespaced_with(client, ns, &ar),
        None => Api::all_with(client, &ar),
    };
    let mut tick = tokio::time::interval(POLL_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, api.list(&ListParams::default())).await {
            Ok(Ok(list)) => Ok(list.items.iter().filter_map(convert_metrics).collect()),
            Ok(Err(e)) => Err(describe_metrics_error(&e)),
            Err(_) => Err(format!("no answer from metrics.k8s.io within {}s", REQUEST_TIMEOUT.as_secs())),
        };
        lock(&cluster).apply_metrics(result, SystemTime::now());
    }
}

fn describe_metrics_error(e: &kube::Error) -> String {
    match e {
        kube::Error::Api(s) if s.code == 404 => "no metrics.k8s.io API (is metrics-server installed?)".into(),
        kube::Error::Api(s) if s.code == 403 => format!("forbidden: {}", s.message),
        kube::Error::Api(s) if s.code == 503 => "metrics-server is not responding (503)".into(),
        e => e.to_string(),
    }
}

fn describe_watch_error(e: &watcher::Error) -> String {
    match e {
        watcher::Error::InitialListFailed(kube::Error::Api(s)) if s.code == 403 => {
            format!("forbidden: {} (try --namespace)", s.message)
        }
        e => e.to_string(),
    }
}

/// metrics.k8s.io/v1beta1 PodMetrics, past the object metadata.
#[derive(Deserialize)]
struct PodMetricsData {
    /// The end of the sampling window.
    timestamp: Time,
    #[serde(default)]
    containers: Vec<ContainerMetrics>,
}

#[derive(Deserialize)]
struct ContainerMetrics {
    #[serde(default)]
    usage: BTreeMap<String, String>,
}

/// A pod's usage summed over its containers. A sample with any value that
/// doesn't parse is dropped, so the pod shows as missing rather than as
/// using less than it does.
fn convert_metrics(o: &DynamicObject) -> Option<PodSample> {
    let data: PodMetricsData = serde_json::from_value(o.data.clone()).ok()?;
    let mut usage = Usage::default();
    for c in &data.containers {
        usage.cpu_millis += quantity::cpu_millis(c.usage.get("cpu")?)?;
        usage.mem_bytes += quantity::bytes(c.usage.get("memory")?)?;
    }
    Some(PodSample {
        namespace: o.metadata.namespace.as_deref()?.into(),
        name: o.metadata.name.as_deref()?.into(),
        timestamp: SystemTime::from(data.timestamp.0),
        usage,
    })
}

fn convert_pod(p: &KPod) -> Option<Pod> {
    let meta = &p.metadata;
    let status = p.status.as_ref();
    let statuses = status.and_then(|s| s.container_statuses.as_deref()).unwrap_or_default();
    let containers = p
        .spec
        .iter()
        .flat_map(|s| &s.containers)
        .map(|c| Container {
            name: c.name.as_str().into(),
            image: c.image.as_deref().unwrap_or_default().into(),
            requests: resources(c.resources.as_ref(), |r| r.requests.as_ref()),
            limits: resources(c.resources.as_ref(), |r| r.limits.as_ref()),
            restarts: statuses.iter().find(|s| s.name == c.name).map_or(0, |s| s.restart_count.max(0) as u32),
        })
        .collect();
    Some(Pod {
        uid: meta.uid.as_deref()?.into(),
        namespace: meta.namespace.as_deref()?.into(),
        name: meta.name.as_deref()?.into(),
        phase: match status.and_then(|s| s.phase.as_deref()) {
            Some("Pending") => Phase::Pending,
            Some("Running") => Phase::Running,
            Some("Succeeded") => Phase::Succeeded,
            Some("Failed") => Phase::Failed,
            _ => Phase::Unknown,
        },
        reason: pod_reason(p).map(Into::into),
        owner: controller(meta),
        node: p.spec.as_ref().and_then(|s| s.node_name.as_deref()).map(Into::into),
        ip: status.and_then(|s| s.pod_ip.as_deref()).map(Into::into),
        created: meta.creation_timestamp.as_ref().map(|t| SystemTime::from(t.0)),
        containers,
    })
}

/// What `kubectl get pods` would show instead of the phase, if anything:
/// "Terminating", "CrashLoopBackOff", "ImagePullBackOff", "Evicted".
fn pod_reason(p: &KPod) -> Option<&str> {
    if p.metadata.deletion_timestamp.is_some() {
        return Some("Terminating");
    }
    let status = p.status.as_ref()?;
    let waiting =
        status.container_statuses.iter().flatten().find_map(|s| s.state.as_ref()?.waiting.as_ref()?.reason.as_deref());
    waiting.or(status.reason.as_deref())
}

fn resources(
    r: Option<&ResourceRequirements>,
    pick: impl Fn(
        &ResourceRequirements,
    ) -> Option<&BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>>,
) -> Resources {
    let Some(map) = r.and_then(pick) else { return Resources::default() };
    Resources {
        cpu_millis: map.get("cpu").and_then(|q| quantity::cpu_millis(&q.0)),
        mem_bytes: map.get("memory").and_then(|q| quantity::bytes(&q.0)),
    }
}

/// The owner reference marked as controller. Static pods name their Node
/// as controller; each one is really a workload of its own.
fn controller(meta: &ObjectMeta) -> Option<Workload> {
    let r = meta.owner_references.iter().flatten().find(|r| r.controller == Some(true))?;
    let kind = match r.kind.as_str() {
        "Node" => return None,
        "Deployment" => WorkloadKind::Deployment,
        "StatefulSet" => WorkloadKind::StatefulSet,
        "DaemonSet" => WorkloadKind::DaemonSet,
        "CronJob" => WorkloadKind::CronJob,
        "Job" => WorkloadKind::Job,
        "ReplicaSet" => WorkloadKind::ReplicaSet,
        "ReplicationController" => WorkloadKind::ReplicationController,
        // kubectl style: "rollout/web".
        other => WorkloadKind::Other(other.to_lowercase().into()),
    };
    Some(Workload { kind, name: r.name.as_str().into() })
}

fn namespace(meta: &ObjectMeta) -> Arc<str> {
    meta.namespace.as_deref().unwrap_or_default().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pod_metrics_sum_containers() {
        let o: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "metrics.k8s.io/v1beta1",
            "kind": "PodMetrics",
            "metadata": { "name": "web-0", "namespace": "shop" },
            "timestamp": "2026-11-01T12:00:00Z",
            "window": "15s",
            "containers": [
                { "name": "app", "usage": { "cpu": "250000000n", "memory": "100Mi" } },
                { "name": "sidecar", "usage": { "cpu": "5m", "memory": "20Mi" } }
            ]
        }))
        .unwrap();
        let s = convert_metrics(&o).unwrap();
        assert_eq!((&*s.namespace, &*s.name), ("shop", "web-0"));
        assert!((s.usage.cpu_millis - 255.0).abs() < 1e-6);
        assert_eq!(s.usage.mem_bytes, 120 << 20);
    }

    #[test]
    fn unparseable_metrics_drop_the_sample() {
        let o: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "metrics.k8s.io/v1beta1",
            "kind": "PodMetrics",
            "metadata": { "name": "web-0", "namespace": "shop" },
            "timestamp": "2026-11-01T12:00:00Z",
            "containers": [
                { "name": "app", "usage": { "cpu": "250m", "memory": "100Mi" } },
                { "name": "odd", "usage": { "cpu": "lots", "memory": "1Mi" } }
            ]
        }))
        .unwrap();
        assert_eq!(convert_metrics(&o), None);
    }

    #[test]
    fn pod_conversion() {
        let p: KPod = serde_json::from_value(serde_json::json!({
            "metadata": {
                "name": "web-7d9f-abc", "namespace": "shop", "uid": "u1",
                "creationTimestamp": "2026-11-01T11:00:00Z",
                "ownerReferences": [
                    { "apiVersion": "v1", "kind": "Node", "name": "n", "uid": "x" },
                    { "apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-7d9f", "uid": "y", "controller": true }
                ]
            },
            "spec": {
                "nodeName": "node-1",
                "containers": [{ "name": "app", "image": "web:1",
                    "resources": { "requests": { "cpu": "100m", "memory": "64Mi" }, "limits": { "memory": "128Mi" } } }]
            },
            "status": {
                "phase": "Running", "podIP": "10.0.0.5",
                "containerStatuses": [{ "name": "app", "image": "web:1", "imageID": "", "ready": false,
                    "restartCount": 4, "state": { "waiting": { "reason": "CrashLoopBackOff" } } }]
            }
        }))
        .unwrap();
        let pod = convert_pod(&p).unwrap();
        assert_eq!(pod.phase, Phase::Running);
        assert_eq!(pod.reason.as_deref(), Some("CrashLoopBackOff"));
        assert_eq!(pod.owner.as_ref().unwrap().describe(), "replicaset/web-7d9f");
        assert_eq!(pod.restarts(), 4);
        let c = &pod.containers[0];
        assert_eq!(c.requests, Resources { cpu_millis: Some(100.0), mem_bytes: Some(64 << 20) });
        assert_eq!(c.limits, Resources { cpu_millis: None, mem_bytes: Some(128 << 20) });
        assert_eq!(pod.node.as_deref(), Some("node-1"));
    }
}
