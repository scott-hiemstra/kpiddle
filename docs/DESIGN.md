# kpiddle: PIDdle for Kubernetes

Status: design only, no code yet (2026-10-31).
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
- A new crate, `piddle-k8s`, that produces a snapshot shaped like PIDdle's:
  `Vec<Item>` with id, parent, name, group keys, CPU, memory and status.
- Generalize the views slightly so a "process" can be any item with those
  fields; don't fork them.
- Keep the launcher rule: neither `piddle` nor the TUI links GTK.

**Data.**
- Use `kube-rs` + `k8s-openapi` and watch pods and nodes with a reflector, so
  updates are cheap.
- Poll `metrics.k8s.io/v1beta1` PodMetrics every 5–15 s. metrics-server
  refreshes at about 15 s anyway, so faster polling buys nothing.
- Without metrics-server, show the bubbles with requests/limits as sizes and
  gray "no metrics" coloring. Don't fail.
- Read the context from kubeconfig, with `--context` and `--namespace` flags
  like kubectl.

**Encoding.**
- Size is memory working set. An optional ring shows the memory limit.
- Color is CPU cores used (same blue→red scale). An optional mode colors by
  usage ÷ limit.
- Status overlays: Pending (hollow), CrashLoopBackOff (pulsing red outline),
  OOMKilled recently (badge), restart count shown in the details.

**Details panel.** Pod spec summary, containers with images, requests/limits,
node, IP, owner chain, recent events, restart history and last termination
reason.

**Actions, replacing terminate/kill, with the same safety tiers:**
- Delete pod. The controller recreates it, which acts as a "restart".
- `kubectl rollout restart` for the owning workload.
- Scale the workload (later, maybe).
- Blocked: nothing here, since RBAC is the real gate. Surface 403s clearly.
- Critical (type the name to confirm): `kube-system`, control-plane
  components, pods with no controller (deleting a bare pod is permanent), and
  StatefulSet pods.
- Normal: a confirmation that defaults to No.
- Respect PodDisruptionBudgets by using the eviction API instead of a raw
  delete where possible.

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

**Step 0** (useful even if kpiddle never ships): move the layout, physics,
encoding and shared widgets out of piddle-gui/piddle-tui into `piddle-view`.
The views should then accept a generic item (id, parent, name, group keys,
cpu, mem, status) rather than `ProcInfo`. The same abstraction serves the
network version (see `docs/piddle-net/DESIGN.md` on that branch).

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

The GUI bubbles come next, then actions with the safety tiers.
