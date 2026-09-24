#!/usr/bin/env bash
set -euo pipefail

if (( EUID != 0 )); then
  echo 'Run as root: sudo scripts/install.sh' >&2
  exit 1
fi

project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cargo build --release --manifest-path "$project_dir/Cargo.toml"
install -D -m 0755 "$project_dir/target/release/bls" /usr/local/bin/bls
# Remove the older bluetooth.service.wants link before changing the unit's install target.
if systemctl is-enabled --quiet bls.service; then
  systemctl disable bls.service
fi
install -D -m 0644 "$project_dir/bls.service" /etc/systemd/system/bls.service

systemctl daemon-reload
systemctl enable bls.service
echo 'Installed bls.service. Run sudo bls config --device DEVICE before the first sync.'
