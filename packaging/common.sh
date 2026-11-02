# Shared by the build-*.sh packaging scripts. Source it from the repo root.
#
# Two packages are produced per format and architecture:
#   kpiddle      terminal interface (`kpiddle`), a static binary with no
#                dependencies, fine for servers and jump hosts
#   kpiddle-gui  graphical interface (`kpiddle-gui`) + desktop entry; needs
#                GTK 4.14 or newer, and doesn't need the kpiddle package
set -euo pipefail

# Target architecture: KPIDDLE_ARCH (x86_64 or aarch64), default the host's.
ARCH=${KPIDDLE_ARCH:-$(uname -m)}
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64) DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture: $ARCH (x86_64 or aarch64)" >&2; exit 1 ;;
esac

# The terminal interface is a static musl binary so it runs on any Linux of
# that architecture. The GUI needs GTK anyway, so it links the system glibc.
MUSL=$ARCH-unknown-linux-musl
TUI_BIN=target/$MUSL/release
GUI_BIN=target/release

# Every crate carries kpiddle's version; the release tag must match it.
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/kpiddle-tui/Cargo.toml | head -1)
for c in kpiddle-core kpiddle-gui; do
  v=$(sed -n 's/^version = "\(.*\)"/\1/p' "crates/$c/Cargo.toml" | head -1)
  [ "$v" = "$VERSION" ] || { echo "crates/$c is version $v, but kpiddle-tui is $VERSION" >&2; exit 1; }
done

# Set KPIDDLE_SKIP_BUILD=1 to package existing binaries, e.g. when CI builds
# them in one job and packages them in another.
if [ -z "${KPIDDLE_SKIP_BUILD:-}" ]; then
  [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  if [ "$ARCH" != "$(uname -m)" ]; then
    echo "Building $ARCH packages needs an $ARCH machine (the GUI links that architecture's GTK)," >&2
    echo "or prebuilt binaries in $TUI_BIN and $GUI_BIN with KPIDDLE_SKIP_BUILD=1." >&2
    exit 1
  fi
  cargo build --release --locked --target "$MUSL" -p kpiddle-tui
  cargo build --release --locked -p kpiddle-gui
fi
for b in "$TUI_BIN/kpiddle" "$GUI_BIN/kpiddle-gui"; do
  [ -x "$b" ] || { echo "$b is missing" >&2; exit 1; }
done

MAINTAINER="Scott Hiemstra <scott@hiemstra.us>"
HOMEPAGE="https://github.com/scott-hiemstra/kpiddle"
DESKTOP=io.github.piddle.kpiddle.desktop

SUMMARY_TUI="Kubernetes pods by CPU and memory: terminal interface"
DESC_TUI="kpiddle is PIDdle for a Kubernetes cluster: pods grouped by namespace
and workload, colored by CPU (1000m, one full core, is red), with memory,
search and details. Missing or stale metrics never look like zero usage.
It only reads unless started with --allow-actions, which enables evict,
rollout restart and delete, each confirmed first.
This package provides the terminal interface, a static binary."

SUMMARY_GUI="Kubernetes pods by CPU and memory: graphical GTK 4 interface"
DESC_GUI="The graphical kpiddle interface: a cluster's pods as bubbles sized by
memory and colored by CPU, grouped by workload, namespace or node, with
search and a details window per pod. Pods without a usage sample are hollow
rings, never idle-looking bubbles. With --allow-actions the details window
can evict, rollout restart and delete pods, each confirmed first."

# Package contents as "source  destination  mode" (licenses are added per
# format, since Debian and RPM/Arch put them in different places).
TUI_FILES=(
  "$TUI_BIN/kpiddle         usr/bin/kpiddle                         755"
  "README.md                usr/share/doc/kpiddle/README.md         644"
)
GUI_FILES=(
  "$GUI_BIN/kpiddle-gui     usr/bin/kpiddle-gui                     755"
  "data/$DESKTOP            usr/share/applications/$DESKTOP         644"
)

# stage_files <root> <entries...>: install entries under <root>.
stage_files() {
  local root=$1 src dst mode
  shift
  for f in "$@"; do
    read -r src dst mode <<<"$f"
    install -Dm"$mode" "$src" "$root/$dst"
  done
}
