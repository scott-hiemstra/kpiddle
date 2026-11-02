#!/usr/bin/env bash
# Build RPMs: target/rpm/kpiddle-<version>-1.<arch>.rpm and
# target/rpm/kpiddle-gui-<version>-1.<arch>.rpm
cd "$(dirname "$0")/.."
. packaging/common.sh

top=$PWD/target/rpm/build
rm -rf "$top"
mkdir -p "$top"/{SPECS,RPMS}
stage=$top/stage
stage_files "$stage" "${TUI_FILES[@]}" "${GUI_FILES[@]}"
install -Dm644 LICENSE "$stage/usr/share/licenses/kpiddle/LICENSE"

cat >"$top/SPECS/kpiddle.spec" <<SPEC
%global debug_package %{nil}
%global _build_id_links none

Name:           kpiddle
Version:        $VERSION
Release:        1
Summary:        $SUMMARY_TUI
License:        MIT
URL:            $HOMEPAGE
Packager:       $MAINTAINER

%description
$DESC_TUI

%package gui
Summary:        $SUMMARY_GUI

%description gui
$DESC_GUI

Requires GTK 4.14 or newer.

%install
cp -a $stage/. %{buildroot}/

%files
/usr/bin/kpiddle
%doc /usr/share/doc/kpiddle/README.md
%license /usr/share/licenses/kpiddle/LICENSE

%files gui
/usr/bin/kpiddle-gui
/usr/share/applications/$DESKTOP
%license /usr/share/licenses/kpiddle/LICENSE
SPEC

# Library requirements (libgtk-4.so.1 etc.) are generated from each binary.
rpmbuild --quiet -bb --target "$ARCH" --define "_topdir $top" --define "_dbpath $top/rpmdb" "$top/SPECS/kpiddle.spec"
for f in $(find "$top/RPMS" -name '*.rpm'); do
  mv "$f" target/rpm/
  echo "target/rpm/$(basename "$f")"
done
