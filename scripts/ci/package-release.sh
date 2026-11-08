#!/usr/bin/env bash
set -euo pipefail

version=$(sed -n 's/^version = "\(.*\)"/\1/p' server/Cargo.toml | head -1)
mkdir -p dist
install -m 0755 "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/ya-relay-server" dist/ya-relay-server
dist/ya-relay-server --version | tee dist/VERSION.txt
grep -F " $version " dist/VERSION.txt
dist/ya-relay-server --help > /dev/null
readelf --program-headers dist/ya-relay-server | grep -F '/lib64/ld-linux-x86-64.so.2'
readelf --version-info dist/ya-relay-server > dist/ELF-VERSIONS.txt
ldd dist/ya-relay-server | tee dist/LINKED-LIBRARIES.txt
if grep -F 'not found' dist/LINKED-LIBRARIES.txt; then
    exit 1
fi

package_root=$(mktemp -d)
metadata_root=$(mktemp -d)
trap 'rm -rf "$package_root" "$metadata_root"' EXIT
mkdir -p "$metadata_root/debian"
cat > "$metadata_root/debian/control" <<EOF
Source: ya-relay-server
Section: net
Priority: optional
Maintainer: Golem Factory <contact@golem.network>

Package: ya-relay-server
Architecture: any
Description: Golem network discovery and relay server
EOF
binary_path="$PWD/dist/ya-relay-server"
depends=$(cd "$metadata_root" && dpkg-shlibdeps -O -e"$binary_path" | sed -n 's/^shlibs:Depends=//p')
test -n "$depends"
install -Dm0755 dist/ya-relay-server "$package_root/usr/bin/ya-relay-server"
mkdir -p "$package_root/DEBIAN"
cat > "$package_root/DEBIAN/control" <<EOF
Package: ya-relay-server
Version: $version
Section: net
Priority: optional
Architecture: amd64
Depends: $depends
Maintainer: Golem Factory <contact@golem.network>
Description: Golem network discovery and relay server
EOF
find "$package_root" -print0 | xargs -0 touch --no-dereference --date="@$SOURCE_DATE_EPOCH"
dpkg-deb --root-owner-group --build "$package_root" "dist/ya-relay-server_${version}_amd64.deb"
dpkg-deb --info "dist/ya-relay-server_${version}_amd64.deb"
extract_root=$(mktemp -d)
dpkg-deb --extract "dist/ya-relay-server_${version}_amd64.deb" "$extract_root"
cmp dist/ya-relay-server "$extract_root/usr/bin/ya-relay-server"
rm -rf "$extract_root"
cp Cargo.lock dist/Cargo.lock
cp "docs/releases/${version}.md" dist/RELEASE-NOTES.md
python3 - <<'PY'
import hashlib
import json
import os
import pathlib
import subprocess

def output(*args):
    return subprocess.check_output(args, text=True).strip()

info = {
    'commit': output('git', 'rev-parse', 'HEAD'),
    'source_date_epoch': os.environ['SOURCE_DATE_EPOCH'],
    'cargo_lock_sha256': hashlib.sha256(pathlib.Path('Cargo.lock').read_bytes()).hexdigest(),
    'rustc': output('rustc', '+1.97.0', '-Vv'),
    'cargo': output('cargo', '+1.97.0', '--version'),
    'protoc': output(os.environ['PROTOC'], '--version'),
    'distribution': pathlib.Path('/etc/os-release').read_text(),
    'glibc': output('getconf', 'GNU_LIBC_VERSION'),
    'build_packages': output('dpkg-query', '-W', '-f=${Package}=${Version}\n'),
    'build_command': 'cargo +1.97.0 build --frozen --release --target x86_64-unknown-linux-gnu -p ya-relay-server',
    'target': 'x86_64-unknown-linux-gnu',
    'features': [],
    'cache_restored': False,
    'workflow_run': f"https://github.com/{os.environ['GITHUB_REPOSITORY']}/actions/runs/{os.environ['GITHUB_RUN_ID']}",
}
pathlib.Path('dist/BUILD-INFO.json').write_text(json.dumps(info, indent=2) + '\n')
PY
(cd dist && sha256sum ya-relay-server ./*.deb Cargo.lock BUILD-INFO.json RELEASE-NOTES.md ELF-VERSIONS.txt LINKED-LIBRARIES.txt > SHA256SUMS)
