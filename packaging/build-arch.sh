#!/usr/bin/env bash
# Build Arch Linux packages: target/arch/kpiddle-<version>-1-<arch>.pkg.tar.zst
# and target/arch/kpiddle-gui-<version>-1-<arch>.pkg.tar.zst
cd "$(dirname "$0")/.."
. packaging/common.sh

work=$PWD/target/arch/build
rm -rf "$work"
mkdir -p "$work"
sources=(kpiddle kpiddle-gui "$DESKTOP" README.md LICENSE)
cp "$TUI_BIN/kpiddle" "$GUI_BIN/kpiddle-gui" "data/$DESKTOP" README.md LICENSE "$work/"
sums=$(cd "$work" && sha256sum "${sources[@]}" | awk '{printf "'"'"'%s'"'"' ", $1}')

cat >"$work/PKGBUILD" <<PKG
# Maintainer: $MAINTAINER
pkgbase=kpiddle
pkgname=('kpiddle' 'kpiddle-gui')
pkgver=$VERSION
pkgrel=1
arch=('$ARCH')
url="$HOMEPAGE"
license=('MIT')
options=('!strip' '!debug')
source=(${sources[*]})
sha256sums=($sums)

package_kpiddle() {
  pkgdesc="$SUMMARY_TUI"
  # Static binary: no runtime dependencies.
  optdepends=('kpiddle-gui: graphical interface')
  install -Dm755 kpiddle "\$pkgdir/usr/bin/kpiddle"
  install -Dm644 README.md "\$pkgdir/usr/share/doc/kpiddle/README.md"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/kpiddle/LICENSE"
}

package_kpiddle-gui() {
  pkgdesc="$SUMMARY_GUI"
  depends=('gtk4>=4.14' 'glib2' 'cairo' 'pango' 'graphene' 'glibc' 'gcc-libs')
  optdepends=('kpiddle: terminal interface')
  install -Dm755 kpiddle-gui "\$pkgdir/usr/bin/kpiddle-gui"
  install -Dm644 $DESKTOP "\$pkgdir/usr/share/applications/$DESKTOP"
  install -Dm644 LICENSE "\$pkgdir/usr/share/licenses/kpiddle-gui/LICENSE"
}
PKG

# --nodeps and PACMAN=true: this may run on a non-Arch host with no pacman
# database; dependencies are declared above and resolved at install time.
# makepkg is chatty, so keep its log and only show it if the build fails.
if ! (cd "$work" && CARCH=$ARCH PKGEXT=.pkg.tar.zst PACKAGER="$MAINTAINER" PACMAN=true \
  makepkg --force --nodeps --noconfirm >makepkg.log 2>&1); then
  cat "$work/makepkg.log" >&2
  exit 1
fi
mkdir -p target/arch
for f in "$work"/kpiddle-*.pkg.tar.zst; do
  mv "$f" target/arch/
  echo "target/arch/$(basename "$f")"
done
