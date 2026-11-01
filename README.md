# kpiddle

[![CI](https://github.com/scott-hiemstra/kpiddle/actions/workflows/ci.yml/badge.svg)](https://github.com/scott-hiemstra/kpiddle/actions/workflows/ci.yml)

**kpiddle** is [PIDdle](https://github.com/scott-hiemstra/piddle) for a Kubernetes cluster: pods instead of processes, grouped by namespace and workload, with the same CPU colors (1000m, one full core, is red). Version 0.1.0 is read-only: a terminal interface (`kpiddle`), and a first graphical one (`kpiddle-gui`, built from source for now). Pod actions come later (see [docs/DESIGN.md](docs/DESIGN.md)).

```sh
kpiddle                          # current kubeconfig context, all namespaces
kpiddle --context prod -n shop   # like kubectl's --context and --namespace
kpiddle --once                   # one plain listing, for scripts
kpiddle-gui                      # pods as bubbles: size is memory, color is CPU
```

Usage comes from metrics.k8s.io, so the cluster needs [metrics-server](https://github.com/kubernetes-sigs/metrics-server). **Missing or stale metrics never look like zero usage:**

- A pod without a sample shows `—` with a hollow bar (a hollow gray ring in the GUI), and sorts after every measured pod.
- Stale values are dimmed and show their age.
- A total over a partly measured group is marked `≥` with its coverage, e.g. `≥ 1.4 cores · 7/10 eligible pods measured`. Only eligible pods count: running ones, plus Unknown ones on an unreachable node. Pending and finished pods have no usage to measure, and the header says how many are not running.
- The header always shows whether metrics are OK, stale since when, or unavailable and why (no metrics-server, forbidden, no answer).

kpiddle only reads (pods, ReplicaSet and Job metadata, pod metrics), so a read-only role is enough. Without permission to list pods cluster-wide, use `--namespace`.

## Install

Each release has static `kpiddle` binaries for x86_64 and ARM64 Linux (`.tar.gz` with `.sha256`), which run on any distribution. `kpiddle-gui` is built from source for now.

## Release

Add each change to the Unreleased section of [CHANGELOG.md](CHANGELOG.md) as it lands. To release, bump `version` in the three `crates/*/Cargo.toml`, date the section, and push a `vX.Y.Z` tag. The **Release** workflow checks the tag against `crates/kpiddle-tui/Cargo.toml`, builds the static binaries and publishes them with that section as the notes. Check the notes locally with `scripts/release-notes.sh 0.1.0`. Running the workflow by hand builds the tarballs as artifacts and publishes nothing.

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
    model.rs        pods + metrics samples → snapshots (fresh / stale / missing usage, totals with coverage)
    source.rs       kube-rs: watch pods, ReplicaSets and Jobs; poll metrics.k8s.io
    quantity.rs     Kubernetes quantities ("250m", "128Mi") as numbers
    tests/          acceptance tests for missing and stale metrics, against a fake metrics source
  kpiddle-tui/      Ratatui interface (binary `kpiddle`)
    app.rs          state, views, search, keys
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
