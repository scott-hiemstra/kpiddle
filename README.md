# kpiddle

[![CI](https://github.com/scott-hiemstra/kpiddle/actions/workflows/ci.yml/badge.svg)](https://github.com/scott-hiemstra/kpiddle/actions/workflows/ci.yml)

**kpiddle** is [PIDdle](https://github.com/scott-hiemstra/piddle) for a Kubernetes cluster: pods instead of processes, grouped by namespace and workload, with the same CPU colors (1000m, one full core, is red). Version 0.1.0 has a terminal interface (`kpiddle`) that can also evict, restart and delete pods when you ask it to, and a graphical one (`kpiddle-gui`) where clicking a pod opens its details. See [docs/DESIGN.md](docs/DESIGN.md) for where it's going.

```sh
kpiddle                          # current kubeconfig context, all namespaces
kpiddle --context prod -n shop   # like kubectl's --context and --namespace
kpiddle --once                   # one plain listing, for scripts
kpiddle --allow-actions          # also evict, restart and delete pods (each asks first)
kpiddle-gui                      # pods as bubbles: size is memory, color is CPU
kpiddle-gui --allow-actions      # click a pod, then evict, restart or delete it
```

Usage comes from metrics.k8s.io, so the cluster needs [metrics-server](https://github.com/kubernetes-sigs/metrics-server). **Missing or stale metrics never look like zero usage:**

- A pod without a sample shows `—` with a hollow bar (a hollow gray ring in the GUI), and sorts after every measured pod.
- Stale values are dimmed and show their age.
- A total over a partly measured group is marked `≥` with its coverage, e.g. `≥ 1.4 cores · 7/10 eligible pods measured`. Only eligible pods count: running ones, plus Unknown ones on an unreachable node. Pending and finished pods have no usage to measure, and the header says how many are not running.
- The header always shows whether metrics are OK, stale since when, or unavailable and why (no metrics-server, forbidden, no answer).

By default kpiddle only reads (pods, ReplicaSet and Job metadata, pod metrics), so a read-only role is enough. Without permission to list pods cluster-wide, use `--namespace`.

## Actions

Started with `--allow-actions`, either interface can change the cluster, and its header says **actions on**. In the terminal, select a pod in the list or open its details, then press a key. In the GUI, click a pod and use the buttons in its details window: a refused action's button is disabled, and its tooltip says why.

| Key / button | Action | Confirmation |
|---|---|---|
| `e` / Evict | **Evict** the pod. The eviction API respects PodDisruptionBudgets, and the controller makes a replacement. This is the way to restart a pod. | yes/no, default no |
| `r` / Rollout restart | **Rollout restart** its Deployment, StatefulSet or DaemonSet, following the update strategy. | yes/no, default no |
| `d` / Delete | **Delete** the pod, bypassing PodDisruptionBudgets. | always type the name |

The pod or workload name must be typed for anything in `kube-system`, control-plane pods, pods with no controller (removing one is permanent) and StatefulSet pods. Static pods are refused: the kubelet runs them from a manifest, so removing one through the API doesn't stop it.

- **The pod you confirmed, or nothing.** Evict and delete carry the pod's UID. If a new pod with the same name has replaced it in the meantime (as StatefulSet pods do), nothing happens and kpiddle says so.
- **Refusals are reported, not retried.** A PodDisruptionBudget that doesn't allow the eviction, or a role that doesn't allow the action (403), is shown with the API's reason. RBAC remains the real gate; kpiddle never works around it.

Evicting needs `create` on `pods/eviction`, deleting needs `delete` on `pods`, and a rollout restart needs `patch` on the workload.

## Install

Each release has packages for x86_64 and ARM64 Linux. The two packages are independent; install either or both.

| Package | Contains | Needs |
|---|---|---|
| `kpiddle` | the terminal interface, a static binary | nothing |
| `kpiddle-gui` | the graphical interface and its menu entry | GTK 4.14 or newer (Ubuntu 24.04, Fedora 40, current Arch) |

The easiest way is the **[package repository](https://scott-hiemstra.github.io/kpiddle/)** for apt, dnf and pacman: set it up once with a few commands, and updates arrive with your normal system updates. It is signed with this key, the same one as PIDdle's:

```
CDC7 835A 1155 6B6D BD74  08E0 D4E5 629D 1E38 47D0   PIDdle packages <scott@hiemstra.us>
```

Or download the files from the [latest release](https://github.com/scott-hiemstra/kpiddle/releases/latest) and install them with the package manager, so the GTK dependencies come along:

```sh
sudo apt install ./kpiddle_*.deb ./kpiddle-gui_*.deb        # Debian, Ubuntu
sudo dnf install ./kpiddle-*.rpm                             # Fedora, RHEL
sudo pacman -U kpiddle-*.pkg.tar.zst                         # Arch
```

There is also a static `kpiddle` tarball (`.tar.gz` with `.sha256`) that runs on any distribution: unpack it and put `kpiddle` on your `PATH`.

## Release

Add each change to the Unreleased section of [CHANGELOG.md](CHANGELOG.md) as it lands. To release, bump `version` in the three `crates/*/Cargo.toml`, date the section, and push a `vX.Y.Z` tag. The **Release** workflow checks the tag against `crates/kpiddle-tui/Cargo.toml`, builds the tarballs and packages and publishes them with that section as the notes. Check the notes locally with `scripts/release-notes.sh 0.1.0`. Running the workflow by hand builds everything as artifacts and publishes nothing.

After publishing, the workflow updates the package repository on GitHub Pages (`.github/workflows/pages.yml`, which can also be run by hand to republish). It signs the RPMs and the apt, dnf and pacman metadata with the `PACKAGES_GPG_KEY` secret, which must match `packaging/signing-key.asc`. The repository carries the latest release only.

Build the packages locally with `packaging/build-deb.sh`, `packaging/build-rpm.sh` (needs `rpmbuild`) or `packaging/build-arch.sh` (needs `makepkg`). They land in `target/debian`, `target/rpm` and `target/arch`.

## Build and run from source

Requirements: Rust (stable, 2024 edition). The GUI needs GTK ≥ 4.14 development files (`libgtk-4-dev` on Debian/Ubuntu); the terminal interface needs no GTK. Static (musl) builds also need `musl-gcc` (`musl-tools` on Debian/Ubuntu), because the TLS library compiles C.

```sh
cargo run --release -p kpiddle-tui     # terminal interface (needs a kubeconfig)
cargo run --release -p kpiddle-gui     # graphical interface
cargo build --release --target x86_64-unknown-linux-musl -p kpiddle-tui   # static binary
cargo test --workspace
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings   # what CI checks
```

The bubble scene, the color and size encodings and the GTK canvas come from [piddle-common](https://github.com/scott-hiemstra/piddle-common), pinned to a release tag's commit in `Cargo.toml`. To change shared code and kpiddle together, check out piddle-common next to this repo and run `scripts/use-local-common.sh on` (and `off` before committing). After a piddle-common release, `scripts/bump-common.sh vX.Y.Z` moves the pin.

`kpiddle-gui --snapshot out.png [workload|namespace|node|free] [query]` renders the canvas off-screen to a PNG using live data. It still needs a display connection.

## Project layout

```
crates/
  kpiddle-core/     kpiddle's data, UI-independent
    actions.rs      evict / delete / rollout restart: risk assessment and the API calls
    model.rs        pods + metrics samples → snapshots (fresh / stale / missing usage, totals with coverage)
    source.rs       kube-rs: watch pods, ReplicaSets and Jobs; poll metrics.k8s.io
    quantity.rs     Kubernetes quantities ("250m", "128Mi") as numbers
    tests/          acceptance tests for missing and stale metrics, against a fake metrics source
  kpiddle-tui/      Ratatui interface (binary `kpiddle`)
    app.rs          state, views, search, keys, confirm flows
    ui.rs           rendering
    text.rs         the --once listing
    main.rs         arguments and event loop
  kpiddle-gui/      GTK 4 interface (binary `kpiddle-gui`)
    view.rs         pods as scene items, layouts, hollow/stale rendering, hover card
    main.rs         window, status banner, input, refresh, --snapshot
.github/workflows/  ci.yml (fmt, clippy, test, static build; x86_64 + ARM64), release.yml (tarballs + GitHub release)
docs/DESIGN.md      design notes
scripts/            release-notes.sh, use-local-common.sh, bump-common.sh
```

## History

kpiddle started in the [piddle](https://github.com/scott-hiemstra/piddle) repo and moved here with its history: the design notes and their review, then each step of the TUI and GUI.

## Contributors

- **Scott Hiemstra** ([scott@hiemstra.us](mailto:scott@hiemstra.us)): author and maintainer; product direction, design and testing
- **Claude** (Anthropic): AI pair programmer; implementation

## License

MIT, see [LICENSE](LICENSE).
