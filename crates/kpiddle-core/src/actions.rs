//! Pod actions and how careful each one needs to be.
//!
//! [`assess`] decides, without talking to the cluster, whether an action is
//! refused outright and what makes it risky. Risky actions need the name
//! typed to confirm; the rest a yes/no that defaults to no. RBAC stays the
//! real gate: a 403 is reported, never worked around.
//!
//! Evict and delete carry the pod's UID as a precondition, so a pod that
//! was replaced by a new one with the same name (a StatefulSet member, say)
//! is left alone: the Kubernetes version of PIDdle's PID-reuse guard.

use std::time::Duration;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::core::v1::Pod as KPod;
use kube::api::{DeleteParams, EvictParams, Preconditions};
use kube::{Api, Client};
use piddle_encode::sanitize;

use crate::model::{PodView, Snapshot, Workload, WorkloadKind};

const ACTION_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The eviction API: PodDisruptionBudgets are respected and the
    /// controller makes a replacement. The default way to restart a pod.
    Evict,
    /// Delete the pod, bypassing PodDisruptionBudgets.
    Delete,
    /// `kubectl rollout restart` of the pod's Deployment, StatefulSet or
    /// DaemonSet.
    RolloutRestart,
}

impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Action::Evict => "Evict",
            Action::Delete => "Delete",
            Action::RolloutRestart => "Rollout restart",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Pod { namespace: String, name: String, uid: String },
    Workload { namespace: String, workload: Workload },
}

impl Target {
    /// What to show and, for a critical action, what to type: the pod or
    /// workload name.
    pub fn name(&self) -> &str {
        match self {
            Target::Pod { name, .. } => name,
            Target::Workload { workload, .. } => &workload.name,
        }
    }

    /// kubectl style: "shop/web-0" or "shop/deployment/web".
    pub fn describe(&self) -> String {
        match self {
            Target::Pod { namespace, name, .. } => format!("{namespace}/{name}"),
            Target::Workload { namespace, workload } => format!("{namespace}/{}", workload.describe()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    pub action: Action,
    pub target: Target,
    /// The action is refused outright, with the reason.
    pub blocked: Option<String>,
    /// Reasons this is risky. Non-empty means the name must be typed.
    pub warnings: Vec<String>,
    /// What will happen, in a sentence.
    pub effect: String,
}

impl Assessment {
    pub fn is_critical(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// How risky `action` on `pv` is. `snap` gives the workload's pod count
/// for a rollout restart.
pub fn assess(snap: &Snapshot, pv: &PodView, action: Action) -> Assessment {
    let pod = &pv.pod;
    let ns = pod.namespace.to_string();
    let mut warnings = Vec::new();
    if &*pod.namespace == "kube-system" {
        warnings.push("In kube-system: cluster components run here.".to_string());
    }

    if action == Action::RolloutRestart {
        let workload = pv.workload.clone();
        let target = Target::Workload { namespace: ns, workload: workload.clone() };
        let restartable =
            matches!(workload.kind, WorkloadKind::Deployment | WorkloadKind::StatefulSet | WorkloadKind::DaemonSet);
        if !restartable {
            let why = match workload.kind {
                WorkloadKind::Pod => "This pod has no controller to restart.".to_string(),
                _ => format!(
                    "Only Deployments, StatefulSets and DaemonSets can be restarted; this pod belongs to {}.",
                    workload.describe()
                ),
            };
            return Assessment { action, target, blocked: Some(why), warnings, effect: String::new() };
        }
        if workload.kind == WorkloadKind::StatefulSet {
            warnings.push(
                "StatefulSet: members restart one at a time, so a clustered app briefly loses each in turn.".into(),
            );
        }
        let n = snap.pods.iter().filter(|p| p.pod.namespace == pod.namespace && p.workload == workload).count();
        let effect = format!(
            "Replaces all {n} pod{} of {}, following its update strategy.",
            if n == 1 { "" } else { "s" },
            workload.describe()
        );
        return Assessment { action, target, blocked: None, warnings, effect };
    }

    let target = Target::Pod { namespace: ns, name: pod.name.to_string(), uid: pod.uid.to_string() };
    let blocked = if pod.static_pod {
        Some(format!(
            "Static pod: the kubelet on {} runs it from a manifest file, so removing it through the API doesn't stop it.",
            pod.node.as_deref().unwrap_or("its node")
        ))
    } else if pod.reason.as_deref() == Some("Terminating") {
        Some("Already terminating.".to_string())
    } else {
        None
    };
    if pod.control_plane {
        warnings.push("Control-plane component.".into());
    }
    let replaced_by = match &pod.owner {
        None => {
            warnings.push("No controller: nothing recreates it, so removing it is permanent.".into());
            None
        }
        Some(owner) => {
            if pv.workload.kind == WorkloadKind::StatefulSet {
                warnings.push(
                    "StatefulSet pod: it comes back with the same name and storage; a clustered app loses this member until then."
                        .into(),
                );
            }
            // A Job replaces its own pods; otherwise name the top controller
            // (the Deployment rather than its ReplicaSet).
            Some(if owner.kind == WorkloadKind::Job { owner.describe() } else { pv.workload.describe() })
        }
    };
    if action == Action::Delete {
        warnings.push("Delete skips PodDisruptionBudgets; evict respects them.".into());
    }
    let after = match &replaced_by {
        Some(w) => format!("{w} creates a replacement"),
        None => "nothing recreates it".to_string(),
    };
    let effect = match action {
        Action::Evict => format!("Evicts {} if PodDisruptionBudgets allow it; {after}.", pod.name),
        _ => format!("Deletes {} with its normal grace period; {after}.", pod.name),
    };
    Assessment { action, target, blocked, warnings, effect }
}

/// The result of an action, ready to show: what happened or why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

/// Carry out an assessed, confirmed action. Refused assessments never
/// reach the API.
pub(crate) async fn execute(client: Client, a: &Assessment) -> Outcome {
    if let Some(why) = &a.blocked {
        return Outcome { ok: false, message: format!("Not allowed: {why}") };
    }
    let what = a.target.describe();
    let result = tokio::time::timeout(ACTION_TIMEOUT, run(client, a)).await;
    let (ok, message) = match result {
        Ok(Ok(())) => (
            true,
            match a.action {
                Action::Evict => format!("Evicted {what}."),
                Action::Delete => format!("Deleted {what}."),
                Action::RolloutRestart => format!("Restarting {what}."),
            },
        ),
        Ok(Err(why)) => (false, why),
        Err(_) => (false, format!("{} {what}: no answer within {}s.", a.action.label(), ACTION_TIMEOUT.as_secs())),
    };
    Outcome { ok, message: sanitize(&message) }
}

async fn run(client: Client, a: &Assessment) -> Result<(), String> {
    let done = match &a.target {
        Target::Pod { namespace, name, uid } => {
            let api: Api<KPod> = Api::namespaced(client, namespace);
            let only_this_pod = DeleteParams {
                preconditions: Some(Preconditions { uid: Some(uid.clone()), resource_version: None }),
                ..Default::default()
            };
            match a.action {
                Action::Evict => {
                    let ep = EvictParams { delete_options: Some(only_this_pod), ..Default::default() };
                    api.evict(name, &ep).await.map(drop)
                }
                _ => api.delete(name, &only_this_pod).await.map(drop),
            }
        }
        Target::Workload { namespace, workload } => match workload.kind {
            WorkloadKind::Deployment => {
                Api::<Deployment>::namespaced(client, namespace).restart(&workload.name).await.map(drop)
            }
            WorkloadKind::StatefulSet => {
                Api::<StatefulSet>::namespaced(client, namespace).restart(&workload.name).await.map(drop)
            }
            WorkloadKind::DaemonSet => {
                Api::<DaemonSet>::namespaced(client, namespace).restart(&workload.name).await.map(drop)
            }
            // assess() blocks the rest; this is only reached if a caller
            // built an Assessment by hand.
            _ => return Err(format!("{} can't be restarted.", workload.describe())),
        },
    };
    done.map_err(|e| describe_error(a, &e))
}

fn describe_error(a: &Assessment, e: &kube::Error) -> String {
    let what = a.target.describe();
    let verb = a.action.label();
    match e {
        kube::Error::Api(s) if s.code == 403 => format!("{verb} {what}: forbidden: {}", s.message),
        kube::Error::Api(s) if s.code == 404 => format!("{verb} {what}: it no longer exists."),
        // Too Many Requests from the eviction API means a budget said no.
        kube::Error::Api(s) if s.code == 429 && a.action == Action::Evict => {
            format!("Not evicted: a PodDisruptionBudget doesn't allow disrupting {what} right now. Not retried.")
        }
        // A failed UID precondition: the name now belongs to a new pod.
        kube::Error::Api(s) if s.code == 409 && matches!(a.target, Target::Pod { .. }) => {
            format!("{what} was replaced by a new pod with the same name; nothing was done.")
        }
        kube::Error::Api(s) => format!("{verb} {what}: {} ({})", s.message, s.code),
        e => format!("{verb} {what}: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::SystemTime;

    use super::*;
    use crate::model::{Cluster, Phase, Pod};

    fn snap(pods: Vec<Pod>) -> Snapshot {
        let mut c = Cluster::new();
        c.replace_owners(
            &WorkloadKind::ReplicaSet,
            [(
                "shop".into(),
                Workload { kind: WorkloadKind::ReplicaSet, name: "web-7d9f".into() },
                Workload { kind: WorkloadKind::Deployment, name: "web".into() },
            )],
        );
        c.replace_pods(pods);
        c.snapshot(SystemTime::now())
    }

    fn pod(ns: &str, name: &str, owner: Option<(WorkloadKind, &str)>) -> Pod {
        Pod {
            uid: format!("uid-{name}").into(),
            namespace: ns.into(),
            name: name.into(),
            phase: Phase::Running,
            owner: owner.map(|(kind, n)| Workload { kind, name: Arc::from(n) }),
            node: Some("node-1".into()),
            ..Default::default()
        }
    }

    fn assess_named(s: &Snapshot, name: &str, action: Action) -> Assessment {
        assess(s, s.pods.iter().find(|p| &*p.pod.name == name).unwrap(), action)
    }

    #[test]
    fn evicting_a_deployment_pod_is_a_plain_confirm() {
        let s = snap(vec![
            pod("shop", "web-a", Some((WorkloadKind::ReplicaSet, "web-7d9f"))),
            pod("shop", "web-b", Some((WorkloadKind::ReplicaSet, "web-7d9f"))),
        ]);
        let a = assess_named(&s, "web-a", Action::Evict);
        assert_eq!(a.blocked, None);
        assert!(!a.is_critical(), "{:?}", a.warnings);
        assert_eq!(a.target, Target::Pod { namespace: "shop".into(), name: "web-a".into(), uid: "uid-web-a".into() });
        assert!(a.effect.contains("deployment/web creates a replacement"), "{}", a.effect);
    }

    #[test]
    fn delete_is_always_critical() {
        let s = snap(vec![pod("shop", "web-a", Some((WorkloadKind::ReplicaSet, "web-7d9f")))]);
        let a = assess_named(&s, "web-a", Action::Delete);
        assert!(a.is_critical());
        assert!(a.warnings.iter().any(|w| w.contains("PodDisruptionBudgets")));
    }

    #[test]
    fn risky_pods_need_the_name_typed() {
        let s = snap(vec![
            pod("shop", "lonely", None),
            pod("db", "pg-0", Some((WorkloadKind::StatefulSet, "pg"))),
            pod("kube-system", "coredns-x", Some((WorkloadKind::Deployment, "coredns"))),
            Pod { control_plane: true, ..pod("infra", "apiserver-proxy", Some((WorkloadKind::DaemonSet, "proxy"))) },
        ]);
        for (name, needle) in [
            ("lonely", "permanent"),
            ("pg-0", "StatefulSet"),
            ("coredns-x", "kube-system"),
            ("apiserver-proxy", "Control-plane"),
        ] {
            let a = assess_named(&s, name, Action::Evict);
            assert!(a.is_critical(), "{name}");
            assert!(a.warnings.iter().any(|w| w.contains(needle)), "{name}: {:?}", a.warnings);
        }
        assert!(assess_named(&s, "lonely", Action::Evict).effect.contains("nothing recreates it"));
    }

    #[test]
    fn static_and_terminating_pods_are_refused() {
        let s = snap(vec![
            Pod { static_pod: true, control_plane: true, ..pod("kube-system", "etcd-cp", None) },
            Pod { reason: Some("Terminating".into()), ..pod("shop", "old", Some((WorkloadKind::Deployment, "x"))) },
        ]);
        for action in [Action::Evict, Action::Delete] {
            let a = assess_named(&s, "etcd-cp", action);
            assert!(a.blocked.as_deref().is_some_and(|b| b.contains("Static pod") && b.contains("node-1")));
            assert_eq!(assess_named(&s, "old", action).blocked.as_deref(), Some("Already terminating."));
        }
    }

    #[test]
    fn rollout_restart_targets_the_workload() {
        let s = snap(vec![
            pod("shop", "web-a", Some((WorkloadKind::ReplicaSet, "web-7d9f"))),
            pod("shop", "web-b", Some((WorkloadKind::ReplicaSet, "web-7d9f"))),
            pod("db", "pg-0", Some((WorkloadKind::StatefulSet, "pg"))),
            pod("shop", "lonely", None),
            pod("batch", "nightly-1", Some((WorkloadKind::Job, "nightly-1"))),
        ]);
        let a = assess_named(&s, "web-a", Action::RolloutRestart);
        let web = Workload { kind: WorkloadKind::Deployment, name: "web".into() };
        assert_eq!(a.target, Target::Workload { namespace: "shop".into(), workload: web });
        assert_eq!(a.target.name(), "web");
        assert!(!a.is_critical() && a.blocked.is_none());
        assert!(a.effect.contains("all 2 pods of deployment/web"), "{}", a.effect);

        assert!(assess_named(&s, "pg-0", Action::RolloutRestart).is_critical());
        assert!(assess_named(&s, "lonely", Action::RolloutRestart).blocked.is_some());
        let job = assess_named(&s, "nightly-1", Action::RolloutRestart);
        assert!(job.blocked.as_deref().is_some_and(|b| b.contains("job/nightly-1")), "{:?}", job.blocked);
    }
}
