#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo 'Usage: scripts/package.sh OWNER/REPO [OUTPUT_DIR]' >&2
  exit 2
}

(( $# >= 1 && $# <= 2 )) || usage
repository=$1
[[ $repository =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || usage

project_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_dir=${2:-$project_dir/dist}
mkdir -p -- "$output_dir"
output_dir=$(cd -- "$output_dir" && pwd)
if [[ -n ${BLS_PACKAGE_VERSION:-} ]]; then
  version=$BLS_PACKAGE_VERSION
else
  build_time=${BLS_BUILD_TIME:-$(date -u +%Y%m%d%H%M%S)}
  commit_ish=${BLS_COMMIT_ISH:-$(git -C "$project_dir" rev-parse --short=12 HEAD)}
  version="$build_time-$commit_ish"
fi
[[ $version =~ ^[0-9]{14}-[0-9a-fA-F]+$ ]] || { echo 'Package version must be YYYYMMDDHHMMSS-COMMIT_ISH' >&2; exit 1; }
timestamp=${version%%-*}
commit_ish=${version#*-}
iso_time="${timestamp:0:4}-${timestamp:4:2}-${timestamp:6:2} ${timestamp:8:2}:${timestamp:10:2}:${timestamp:12:2} UTC"
[[ $(date -u -d "$iso_time" +%Y%m%d%H%M%S 2>/dev/null) == "$timestamp" ]] || { echo 'Invalid UTC timestamp in package version' >&2; exit 1; }
echo "Package version: $version"

# RPM Version and Arch pkgver do not permit hyphens, so encode the same
# timestamp/commit pair with their supported separators in package metadata.
rpm_version=$timestamp
rpm_release="1.$commit_ish"
arch_pkgver="${timestamp}_${commit_ish}"

case $(uname -m) in
  x86_64) rpm_arch=x86_64; deb_arch=amd64 ;;
  aarch64) rpm_arch=aarch64; deb_arch=arm64 ;;
  *) echo 'Supported architectures: x86_64, aarch64' >&2; exit 1 ;;
esac

for tool in cargo dpkg-deb rpmbuild tar sha256sum; do
  command -v "$tool" >/dev/null || { echo "Missing build tool: $tool" >&2; exit 1; }
done

work_dir=$(mktemp -d)
trap 'rm -rf -- "$work_dir"' EXIT
CARGO_TARGET_DIR="$work_dir/target" cargo build --release --locked --manifest-path "$project_dir/Cargo.toml"
release_name="bls-v${version}-linux-${rpm_arch}"
release_dir="$work_dir/$release_name"
mkdir -p "$release_dir"
install -m 0755 "$work_dir/target/release/bls" "$release_dir/bls"
sed 's|/usr/local/bin/bls|/usr/bin/bls|' "$project_dir/bls.service" > "$release_dir/bls.service"
install -m 0644 "$project_dir/README.md" "$release_dir/README.md"
install -m 0644 "$project_dir/README.en.md" "$release_dir/README.en.md"
install -m 0644 "$project_dir/LICENSE" "$release_dir/LICENSE"
install -m 0644 "$project_dir/vendor/ntreg/LGPL.txt" "$release_dir/NTREG-LGPL.txt"
tar -C "$work_dir" -czf "$output_dir/$release_name.tar.gz" "$release_name"

package_root="$work_dir/deb-root"
install -Dm0755 "$release_dir/bls" "$package_root/usr/bin/bls"
install -Dm0644 "$release_dir/bls.service" "$package_root/usr/lib/systemd/system/bls.service"
install -Dm0644 "$release_dir/README.md" "$package_root/usr/share/doc/dual-boot-bluetooth-sync/README.md"
install -Dm0644 "$release_dir/README.en.md" "$package_root/usr/share/doc/dual-boot-bluetooth-sync/README.en.md"
install -Dm0644 "$release_dir/LICENSE" "$package_root/usr/share/doc/dual-boot-bluetooth-sync/copyright"
install -Dm0644 "$release_dir/NTREG-LGPL.txt" "$package_root/usr/share/doc/dual-boot-bluetooth-sync/NTREG-LGPL.txt"
mkdir -p "$package_root/DEBIAN"
installed_size=$(du -sk "$package_root/usr" | awk '{print $1}')
cat > "$package_root/DEBIAN/control" <<EOF
Package: dual-boot-bluetooth-sync
Version: $version
Architecture: $deb_arch
Maintainer: Dual Boot Bluetooth Sync contributors <noreply@github.com>
Section: utils
Priority: optional
Installed-Size: $installed_size
Depends: bluez, systemd, util-linux
Description: Synchronize Windows Bluetooth bonds and metadata into BlueZ
 Imports Classic and BLE bonds from an offline Windows SYSTEM registry hive.
EOF
dpkg-deb --build --root-owner-group "$package_root" "$output_dir/dual-boot-bluetooth-sync_${version}_${deb_arch}.deb"

rpm_top="$work_dir/rpmbuild"
mkdir -p "$rpm_top"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
mkdir -p "$work_dir/rpmdb"
cp "$output_dir/$release_name.tar.gz" "$rpm_top/SOURCES/"
cat > "$rpm_top/SPECS/dual-boot-bluetooth-sync.spec" <<EOF
%global debug_package %{nil}
Name: dual-boot-bluetooth-sync
Version: $rpm_version
Release: $rpm_release%{?dist}
Summary: Synchronize Windows Bluetooth bonds and metadata into BlueZ
License: GPL-3.0-only AND LGPL-2.1-only
URL: https://github.com/$repository
Source0: $release_name.tar.gz
BuildArch: $rpm_arch
Requires: bluez, systemd, util-linux

%description
Import Classic and BLE bonds from an offline Windows SYSTEM registry hive.

%prep
%setup -q -n $release_name

%build

%install
install -Dm0755 bls %{buildroot}/usr/bin/bls
install -Dm0644 bls.service %{buildroot}/usr/lib/systemd/system/bls.service
install -Dm0644 README.md %{buildroot}/usr/share/doc/dual-boot-bluetooth-sync/README.md
install -Dm0644 README.en.md %{buildroot}/usr/share/doc/dual-boot-bluetooth-sync/README.en.md
install -Dm0644 LICENSE %{buildroot}/usr/share/licenses/dual-boot-bluetooth-sync/LICENSE
install -Dm0644 NTREG-LGPL.txt %{buildroot}/usr/share/licenses/dual-boot-bluetooth-sync/NTREG-LGPL.txt

%files
/usr/bin/bls
/usr/lib/systemd/system/bls.service
%doc /usr/share/doc/dual-boot-bluetooth-sync/README.md
%doc /usr/share/doc/dual-boot-bluetooth-sync/README.en.md
%license /usr/share/licenses/dual-boot-bluetooth-sync/LICENSE
%license /usr/share/licenses/dual-boot-bluetooth-sync/NTREG-LGPL.txt
EOF
rpmbuild -bb --define "_topdir $rpm_top" --define "_dbpath $work_dir/rpmdb" "$rpm_top/SPECS/dual-boot-bluetooth-sync.spec"
find "$rpm_top/RPMS" -type f -name 'dual-boot-bluetooth-sync-[0-9]*.rpm' -exec cp -- '{}' "$output_dir/" \;

archive_hash=$(sha256sum "$output_dir/$release_name.tar.gz" | awk '{print $1}')
aur_dir="$work_dir/aur-bin"
mkdir -p "$aur_dir"
cat > "$aur_dir/PKGBUILD" <<EOF
# Maintainer: ${repository%%/*} <${repository%%/*}@users.noreply.github.com>
pkgname=dual-boot-bluetooth-sync-bin
pkgver=$arch_pkgver
pkgrel=1
pkgdesc='Synchronize Windows Bluetooth bonds and metadata into BlueZ'
arch=('$rpm_arch')
url='https://github.com/$repository'
license=('GPL-3.0-only' 'LGPL-2.1-only')
options=('!debug')
depends=('bluez' 'systemd' 'util-linux')
provides=('dual-boot-bluetooth-sync')
conflicts=('dual-boot-bluetooth-sync')
source_${rpm_arch}=('$release_name.tar.gz::https://github.com/$repository/releases/download/v$version/$release_name.tar.gz')
sha256sums_${rpm_arch}=('$archive_hash')

package() {
  install -Dm0755 "\$srcdir/$release_name/bls" "\$pkgdir/usr/bin/bls"
  install -Dm0644 "\$srcdir/$release_name/bls.service" "\$pkgdir/usr/lib/systemd/system/bls.service"
  install -Dm0644 "\$srcdir/$release_name/README.md" "\$pkgdir/usr/share/doc/dual-boot-bluetooth-sync/README.md"
  install -Dm0644 "\$srcdir/$release_name/README.en.md" "\$pkgdir/usr/share/doc/dual-boot-bluetooth-sync/README.en.md"
  install -Dm0644 "\$srcdir/$release_name/LICENSE" "\$pkgdir/usr/share/licenses/dual-boot-bluetooth-sync/LICENSE"
  install -Dm0644 "\$srcdir/$release_name/NTREG-LGPL.txt" "\$pkgdir/usr/share/licenses/dual-boot-bluetooth-sync/NTREG-LGPL.txt"
}
EOF
if command -v makepkg >/dev/null; then
  (cd "$aur_dir" && makepkg --printsrcinfo) > "$aur_dir/.SRCINFO"
else
  cat > "$aur_dir/.SRCINFO" <<EOF
pkgbase = dual-boot-bluetooth-sync-bin
  pkgdesc = Synchronize Windows Bluetooth bonds and metadata into BlueZ
  pkgver = $arch_pkgver
  pkgrel = 1
  url = https://github.com/$repository
  arch = $rpm_arch
  license = GPL-3.0-only
  license = LGPL-2.1-only
  options = !debug
  depends = bluez
  depends = systemd
  depends = util-linux
  provides = dual-boot-bluetooth-sync
  conflicts = dual-boot-bluetooth-sync
  source_$rpm_arch = $release_name.tar.gz::https://github.com/$repository/releases/download/v$version/$release_name.tar.gz
  sha256sums_$rpm_arch = $archive_hash

pkgname = dual-boot-bluetooth-sync-bin
EOF
fi
tar -C "$work_dir" -czf "$output_dir/dual-boot-bluetooth-sync-bin-$version-aur.tar.gz" aur-bin
echo "Packages written to $output_dir"
