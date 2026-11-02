# kpiddle: PIDdle for Kubernetes

Status: kpiddle 0.2.0 (2026-11-01) has the TUI and the GUI, both with pod
details and with evict, rollout restart and delete behind `--allow-actions`,
packaged for Debian, Fedora and Arch. The sections below are the original design; the code follows it
except where noted in [CHANGELOG.md](../CHANGELOG.md).

Update, 2026-11-01: kpiddle has since moved to its own repo, as anticipated
below. The shared view code that the design calls `piddle-view` is now the
[piddle-common](https://github.com/scott-hiemstra/piddle-common) crates
(`piddle-encode`, `piddle-scene`, `piddle-gtk`), and kpiddle's tags are
plain `vX.Y.Z`.
Parent project: [PIDdle](https://github.com/scott-hiemstra/piddle), v0.3.2 at the time of writing.

## The pitch

PIDdle shows a machine's processes as bubbles: size is memory, color is CPU
load (100% = one core). kpiddle does the same for a cluster. Pods become
bubbles, and namespaces, workloads and nodes become the groupings.

## Three possible shapes

1. **PIDdle for a cluster (pods instead of processes).** *Recommended first step.*
   - Each bubble is a pod (or container). Size is memory working set, color is
     CPU in cores, where 1000m = 100% to match PIDdle's scale.
   - Data comes from the API server (pods, nodes, owner references) plus
     `metrics.k8s.io` (metrics-server) for usage.
   - Tree: namespace → Deployment/StatefulSet/DaemonSet/Job → ReplicaSet → pod → container.
   - Grouping by namespace, node, workload or app label (`app.kubernetes.io/name`).
   - Runs in both the GUI and the TUI. The TUI works well as a `kubectl piddle` plugin.
2. **PIDdle inside a cluster.** A container image and DaemonSet (`hostPID: true`)
   that runs today's PIDdle on each node. You see the node's real processes, and
   each one is mapped to its pod and container via `/proc/<pid>/cgroup`.
3. **Both, layered.** Start in the cluster view, then pick a node or pod and
   drill into its real process view through the option 2 agent.

## Design notes for option 1

**Architecture.** Add a second data source behind the existing views.
`piddle-core` already keeps collection separate from display, so the plan is:
- A new crate, `kpiddle-core`, that produces a snapshot shaped like PIDdle's:
  `Vec<Item>` with id, parent, name, group keys, CPU, memory and status.
- Generalize the views slightly so a "process" can be any item with those
  fields; don't fork them.
- Keep the launcher rule: neither `piddle` nor the TUI links GTK.

**Data.**
- Use `kube-rs` + `k8s-openapi` and watch pods and nodes with a reflector, so
  updates are cheap.
- Poll `metrics.k8s.io/v1beta1` PodMetrics every 5–15 s. metrics-server
  refreshes at about 15 s anyway, so faster polling buys nothing.
- Without metrics-server, don't fail, and don't silently swap usage for
  requests: that would change what the picture means. Offer an explicit,
  clearly labeled **Requests/limits mode** instead. A banner in the header
  says the bubbles show requested resources, and CPU color is gray. The same
  mode is selectable when metrics are available, for capacity planning.
- Read the context from kubeconfig, with `--context` and `--namespace` flags
  like kubectl.

**Missing or stale metrics never look like zero.** If an unavailable metrics
source rendered as 0%, a busy cluster would look reassuringly idle. Every
pod's usage is one of three states:
- **fresh:** a PodMetrics sample within about 2× the scrape interval
  (≤ ~60 s old);
- **stale:** a sample exists but is older than that;
- **missing:** no sample, because the pod is new, metrics-server is down or
  slow, or the pod is excluded.

How each state is shown:
- **Pods.**
  - Missing usage shows as `—`, never `0`, and the bubble is hollow and gray.
  - Stale values are dimmed and carry their age ("2m old").
  - Sizes for missing pods come from nothing: the bubble keeps a fixed
    minimal size, rather than borrowing requests outside the explicit
    requests mode.
- **Totals (namespace, workload, node, cluster).**
  - Missing members are never summed as 0.
  - A total with unmeasured members is marked partial with its coverage,
    e.g. `≥ 3.2 cores · 7/10 pods measured`.
  - A total with no measured members shows `—`.
  - A total with stale members is marked stale.
- **Sorting.** Unmeasured items don't sort as idle. They go in their own
  bucket after measured ones, labeled as such.
- **Header.** The metrics source state is always visible: *metrics OK*, *stale
  since 14:02*, or *metrics unavailable* (with the reason, e.g. no
  metrics.k8s.io API or forbidden).
- **Cluster-wide outage.** When every pod is missing, kpiddle suggests the
  labeled requests/limits mode instead of showing a field of gray. It doesn't
  switch modes silently.

**Encoding.**
- Size is memory working set. An optional ring shows the memory limit, so
  you see usage inside its limit.
- Color is CPU cores used (same blue→red scale). An optional mode colors by
  usage ÷ request or ÷ limit.
- Requests and limits get their own visual treatment. Two cues stand out:
  - a bubble nearly filling its ring (close to OOM), and
  - a bubble far smaller than its request (over-provisioned).
- CPU throttling is shown when that telemetry is available
  (`container_cpu_cfs_throttled_*` via the kubelet's cAdvisor endpoint).
- Status overlays: Pending (hollow), CrashLoopBackOff (pulsing red outline),
  OOMKilled recently (badge), restart count shown in the details.

**Scale.**
- Default to one aggregate bubble per workload (Deployment, StatefulSet and
  so on). Its size and color are the sum of its pods.
- Clicking a workload expands it into its pods.
- A thousand individual pod bubbles would drown the signal. This matches the
  group hulls PIDdle already draws in the by-app view.

**Details panel.**
- Pod spec summary, containers with images, requests/limits, node, IP, owner
  chain, recent events, restart history and last termination reason.
- Logs: the tail of container logs (`pods/log`, follow mode), sanitized. This
  is the same pane as PIDdle's journald logs (see `docs/ROADMAP.md`).

**Actions, replacing terminate/kill, with the same safety tiers:**
- **Evict pod** (the default "restart"). This uses the eviction API, so
  PodDisruptionBudgets are respected, and the controller recreates the pod.
  A PDB refusal (429) is shown as such, not retried.
- **Delete pod.** A separate, explicit choice that bypasses PDBs. It is always
  in the critical tier: type the name to confirm.
- `kubectl rollout restart` for the owning workload.
- Scale the workload (later, maybe).
- Blocked: nothing here, since RBAC is the real gate. Surface 403s clearly.
- Critical (type the name to confirm): `kube-system`, control-plane
  components, pods with no controller (deleting a bare pod is permanent), and
  StatefulSet pods.
- Normal: a confirmation that defaults to No.

**Security.**
- Read-only by default; actions need an explicit `--allow-actions` or a
  toggle in the UI.
- Never log or display tokens from kubeconfig.
- Sanitize names and labels, reusing `encode::sanitize()`. They're
  user-controlled strings, just like a process comm.

## Decided: separate application, same repo

kpiddle is its own application, with its own binaries, packages and release
tags, but it lives in the piddle workspace so it can share the view code
without publishing a library.

```
crates/
  piddle-core      /proc sampling                    piddle only
  piddle-view      layout, physics, colors, sanitize  shared (new)
  piddle-gui / piddle-tui / piddle
  kpiddle-core     kube-rs data source
  kpiddle-gui / kpiddle-tui / kpiddle
```

- The kube-rs dependencies are compiled only for the kpiddle crates, so piddle
  packages stay the same size.
- Tags: `vX.Y.Z` for piddle, `kpiddle-vX.Y.Z` for kpiddle. The release
  workflow builds only the app that was tagged.
- CI uses path filters, so a change that only touches kpiddle doesn't rebuild
  piddle's packages, and the reverse.
- If the shared crate settles and the apps drift apart, kpiddle can move to
  its own repo later; git keeps the history.

**Extract as needed, not up front.** Build kpiddle's first working screen
and move code into `piddle-view` only when kpiddle actually needs it. A
second real consumer shows what the shared abstraction should be, which is
better than guessing in advance. The result is a views layer that takes a
generic item (id, parent, name, group keys, cpu, mem, status) rather than
`ProcInfo`. piddle-net then builds on that abstraction.

## Open questions

- Is cluster scale OK? Thousands of pods means the layout needs grouping or
  level-of-detail. The by-app hull packing already helps.
- For option 2, how much privilege is acceptable (hostPID, read-only
  `/proc`), and how does the drill-down connect (port-forward to the agent,
  or `kubectl exec` the TUI)?

## Suggested first milestone

A TUI-only `kpiddle` (or `piddle --k8s`) that:
- lists pods with CPU and memory from metrics-server,
- groups them by namespace and workload,
- shows a details popup,
- performs no actions.

Acceptance criteria, tested against a fake metrics source, so the sampling
and display logic can be checked without a cluster:
- all metrics missing: every pod and total shows `—`, the header says
  unavailable, and nothing reads 0%;
- partial: totals are marked partial with their coverage, and unmeasured
  pods sort after measured ones;
- stale: values carry their age and the header says stale since when;
- recovery: when fresh samples return, the stale and partial marks clear.

The GUI bubbles come next, then actions with the safety tiers.
