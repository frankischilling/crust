#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "${script_dir}/.." && pwd)"
cd "${repo_root}"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo is required but not found in PATH" >&2
    exit 1
fi

if ! command -v dpkg-deb >/dev/null 2>&1; then
    echo "dpkg-deb is required but not found in PATH" >&2
    exit 1
fi

echo "[1/5] Building crust release binary..."
cargo build -p crust --release

bin_path="${repo_root}/target/release/crust"
if [[ ! -f "${bin_path}" ]]; then
    echo "Release binary not found at ${bin_path}" >&2
    exit 1
fi

pkgid="$(cargo pkgid -p crust)"
version="${pkgid##*@}"
arch="$(dpkg --print-architecture)"

artifact_base="crust-v${version}-debian-${arch}"
dist_root="${repo_root}/dist/debian"
stage_root="${dist_root}/${artifact_base}"
pkg_root="${stage_root}/pkg"
deb_path="${dist_root}/${artifact_base}.deb"

echo "[2/5] Staging Debian package layout..."
rm -rf "${stage_root}"
mkdir -p "${pkg_root}/DEBIAN"
mkdir -p "${pkg_root}/usr/bin"
mkdir -p "${pkg_root}/usr/share/applications"
mkdir -p "${pkg_root}/usr/share/doc/crust"
mkdir -p "${pkg_root}/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "${bin_path}" "${pkg_root}/usr/bin/crust"
install -m 0644 "${repo_root}/README.md" "${pkg_root}/usr/share/doc/crust/README.md"
install -m 0644 "${repo_root}/LICENSE" "${pkg_root}/usr/share/doc/crust/LICENSE"
install -m 0644 "${repo_root}/crates/app/resources/crust.desktop" \
    "${pkg_root}/usr/share/applications/crust.desktop"
install -m 0644 "${repo_root}/crates/app/resources/crust.svg" \
    "${pkg_root}/usr/share/icons/hicolor/scalable/apps/crust.svg"

cat >"${pkg_root}/DEBIAN/control" <<EOF
Package: crust
Version: ${version}
Section: net
Priority: optional
Architecture: ${arch}
Maintainer: crust contributors <noreply@github.com>
Depends: libc6, libgcc-s1, libstdc++6
Description: Native Twitch chat client desktop application
 Crust is a native desktop chat client focused on Twitch.
EOF

echo "[3/5] Building .deb artifact..."
rm -f "${deb_path}"
dpkg-deb --build --root-owner-group "${pkg_root}" "${deb_path}"

echo "[4/5] Writing SHA256 digest..."
sha256sum "${deb_path}" > "${deb_path}.sha256"

echo "[5/5] Done"
echo "Binary: ${bin_path}"
echo "Package: ${deb_path}"
echo "Digest: ${deb_path}.sha256"
