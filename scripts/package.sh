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
  build_number=${BLS_BUILD_NUMBER:-${GITHUB_RUN_NUMBER:-1}}
  version="$build_time.$build_number"
fi
[[ $version =~ ^[0-9]{14}\.[1-9][0-9]*$ ]] || { echo 'Package version must be YYYYMMDDHHMMSS.BUILD_NUMBER' >&2; exit 1; }
timestamp=${version%%.*}
iso_time="${timestamp:0:4}-${timestamp:4:2}-${timestamp:6:2} ${timestamp:8:2}:${timestamp:10:2}:${timestamp:12:2} UTC"
[[ $(date -u -d "$iso_time" +%Y%m%d%H%M%S 2>/dev/null) == "$timestamp" ]] || { echo 'Invalid UTC timestamp in package version' >&2; exit 1; }
echo "Package version: $version"

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
install -m 0644 "$project_dir/README.zh-CN.md" "$release_dir/README.zh-CN.md"
install -m 0644 "$project_dir/LICENSE" "$release_dir/LICENSE"
tar -C "$work_dir" -czf "$output_dir/$release_name.tar.gz" "$release_name"

package_root="$work_dir/deb-root"
install -Dm0755 "$release_dir/bls" "$package_root/usr/bin/bls"
install -Dm0644 "$release_dir/bls.service" "$package_root/usr/lib/systemd/system/bls.service"
install -Dm0644 "$release_dir/README.md" "$package_root/usr/share/doc/linux-bluetooth-sync/README.md"
install -Dm0644 "$release_dir/README.zh-CN.md" "$package_root/usr/share/doc/linux-bluetooth-sync/README.zh-CN.md"
install -Dm0644 "$release_dir/LICENSE" "$package_root/usr/share/doc/linux-bluetooth-sync/copyright"
mkdir -p "$package_root/DEBIAN"
installed_size=$(du -sk "$package_root/usr" | awk '{print $1}')
cat > "$package_root/DEBIAN/control" <<EOF
Package: linux-bluetooth-sync
Version: $version
Architecture: $deb_arch
Maintainer: Linux Bluetooth Sync contributors <noreply@github.com>
Section: utils
Priority: optional
Installed-Size: $installed_size
Depends: bluez, chntpw, systemd, util-linux
Description: Synchronize Windows Bluetooth bonds and metadata into BlueZ
 Imports Classic and BLE bonds from an offline Windows SYSTEM registry hive.
EOF
dpkg-deb --build --root-owner-group "$package_root" "$output_dir/linux-bluetooth-sync_${version}_${deb_arch}.deb"

rpm_top="$work_dir/rpmbuild"
mkdir -p "$rpm_top"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS}
mkdir -p "$work_dir/rpmdb"
cp "$output_dir/$release_name.tar.gz" "$rpm_top/SOURCES/"
cat > "$rpm_top/SPECS/linux-bluetooth-sync.spec" <<EOF
%global debug_package %{nil}
Name: linux-bluetooth-sync
Version: $version
Release: 1%{?dist}
Summary: Synchronize Windows Bluetooth bonds and metadata into BlueZ
License: MIT
URL: https://github.com/$repository
Source0: $release_name.tar.gz
BuildArch: $rpm_arch
Requires: bluez, systemd, util-linux, /usr/bin/reged

%description
Import Classic and BLE bonds from an offline Windows SYSTEM registry hive.

%prep
%setup -q -n $release_name

%build

%install
install -Dm0755 bls %{buildroot}/usr/bin/bls
install -Dm0644 bls.service %{buildroot}/usr/lib/systemd/system/bls.service
install -Dm0644 README.md %{buildroot}/usr/share/doc/linux-bluetooth-sync/README.md
install -Dm0644 README.zh-CN.md %{buildroot}/usr/share/doc/linux-bluetooth-sync/README.zh-CN.md
install -Dm0644 LICENSE %{buildroot}/usr/share/licenses/linux-bluetooth-sync/LICENSE

%files
/usr/bin/bls
/usr/lib/systemd/system/bls.service
%doc /usr/share/doc/linux-bluetooth-sync/README.md
%doc /usr/share/doc/linux-bluetooth-sync/README.zh-CN.md
%license /usr/share/licenses/linux-bluetooth-sync/LICENSE
EOF
rpmbuild -bb --define "_topdir $rpm_top" --define "_dbpath $work_dir/rpmdb" "$rpm_top/SPECS/linux-bluetooth-sync.spec"
find "$rpm_top/RPMS" -type f -name 'linux-bluetooth-sync-[0-9]*.rpm' -exec cp -- '{}' "$output_dir/" \;

archive_hash=$(sha256sum "$output_dir/$release_name.tar.gz" | awk '{print $1}')
aur_dir="$work_dir/aur-bin"
mkdir -p "$aur_dir"
cat > "$aur_dir/PKGBUILD" <<EOF
# Maintainer: ${repository%%/*} <${repository%%/*}@users.noreply.github.com>
pkgname=linux-bluetooth-sync-bin
pkgver=$version
pkgrel=1
pkgdesc='Synchronize Windows Bluetooth bonds and metadata into BlueZ'
arch=('$rpm_arch')
url='https://github.com/$repository'
license=('MIT')
options=('!debug')
depends=('bluez' 'chntpw' 'systemd' 'util-linux')
provides=('linux-bluetooth-sync')
conflicts=('linux-bluetooth-sync')
source_${rpm_arch}=('$release_name.tar.gz::https://github.com/$repository/releases/download/v$version/$release_name.tar.gz')
sha256sums_${rpm_arch}=('$archive_hash')

package() {
  install -Dm0755 "\$srcdir/$release_name/bls" "\$pkgdir/usr/bin/bls"
  install -Dm0644 "\$srcdir/$release_name/bls.service" "\$pkgdir/usr/lib/systemd/system/bls.service"
  install -Dm0644 "\$srcdir/$release_name/README.md" "\$pkgdir/usr/share/doc/linux-bluetooth-sync/README.md"
  install -Dm0644 "\$srcdir/$release_name/README.zh-CN.md" "\$pkgdir/usr/share/doc/linux-bluetooth-sync/README.zh-CN.md"
  install -Dm0644 "\$srcdir/$release_name/LICENSE" "\$pkgdir/usr/share/licenses/linux-bluetooth-sync/LICENSE"
}
EOF
if command -v makepkg >/dev/null; then
  (cd "$aur_dir" && makepkg --printsrcinfo) > "$aur_dir/.SRCINFO"
else
  cat > "$aur_dir/.SRCINFO" <<EOF
pkgbase = linux-bluetooth-sync-bin
  pkgdesc = Synchronize Windows Bluetooth bonds and metadata into BlueZ
  pkgver = $version
  pkgrel = 1
  url = https://github.com/$repository
  arch = $rpm_arch
  license = MIT
  options = !debug
  depends = bluez
  depends = chntpw
  depends = systemd
  depends = util-linux
  provides = linux-bluetooth-sync
  conflicts = linux-bluetooth-sync
  source_$rpm_arch = $release_name.tar.gz::https://github.com/$repository/releases/download/v$version/$release_name.tar.gz
  sha256sums_$rpm_arch = $archive_hash

pkgname = linux-bluetooth-sync-bin
EOF
fi
tar -C "$work_dir" -czf "$output_dir/linux-bluetooth-sync-bin-$version-aur.tar.gz" aur-bin
echo "Packages written to $output_dir"
