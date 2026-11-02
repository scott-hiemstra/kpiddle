#!/usr/bin/env bash
# Build Debian packages: target/debian/kpiddle_<version>_<arch>.deb and
# target/debian/kpiddle-gui_<version>_<arch>.deb
cd "$(dirname "$0")/.."
. packaging/common.sh

arch=$DEB_ARCH
out=target/debian
mkdir -p "$out"

# Shared-library dependencies of the given binaries, computed by dpkg-shlibdeps.
shlibdeps() {
  local work
  work=$(mktemp -d)
  mkdir -p "$work/debian"
  printf 'Source: kpiddle\n\nPackage: kpiddle\nArchitecture: any\n' >"$work/debian/control"
  (cd "$work" && dpkg-shlibdeps -O "$@" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')
  rm -rf "$work"
}

# build_deb <package> <summary> <description> <extra fields> <file entries...>
build_deb() {
  local pkg=$1 summary=$2 desc=$3 extra=$4
  shift 4
  local root="$out/${pkg}_${VERSION}_${arch}"
  rm -rf "$root"
  stage_files "$root" "$@"
  install -Dm644 LICENSE "$root/usr/share/doc/$pkg/copyright"

  local bins=()
  for f in "$root"/usr/bin/*; do bins+=("$PWD/$f"); done
  # A static binary has no library dependencies, so this is empty for kpiddle.
  local depends
  depends=$(shlibdeps "${bins[@]}" | sed 's/,/, /g; s/,  /, /g')

  mkdir -p "$root/DEBIAN"
  {
    echo "Package: $pkg"
    echo "Version: $VERSION"
    echo "Architecture: $arch"
    echo "Maintainer: $MAINTAINER"
    echo "Installed-Size: $(du -sk "$root" | cut -f1)"
    [ -n "$depends" ] && echo "Depends: $depends"
    [ -n "$extra" ] && echo "$extra"
    echo "Section: admin"
    echo "Priority: optional"
    echo "Homepage: $HOMEPAGE"
    echo "Description: $summary"
    # Debian descriptions mark blank lines with " ."; ours have none.
    sed 's/^/ /' <<<"$desc"
  } >"$root/DEBIAN/control"

  dpkg-deb --root-owner-group --build "$root" "$out/" >/dev/null
  rm -rf "$root"
  echo "$out/${pkg}_${VERSION}_${arch}.deb"
}

build_deb kpiddle "$SUMMARY_TUI" "$DESC_TUI" "Suggests: kpiddle-gui" "${TUI_FILES[@]}"
build_deb kpiddle-gui "$SUMMARY_GUI" "$DESC_GUI" "Suggests: kpiddle" "${GUI_FILES[@]}"
