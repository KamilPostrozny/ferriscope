#!/usr/bin/env bash
# Build and install the ferriscope Vulkan WSI layer, 64-bit and 32-bit.
#
#   tools/install-wsi.sh [--prefix PREFIX] [--profile release|debug]
#
# Installs, under PREFIX (default /usr/local; DESTDIR is honoured):
#   lib/ferriscope/libferriscope_wsi.so                         (x86_64)
#   lib32/ferriscope/libferriscope_wsi.so                       (i686)
#   share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.x86_64.json
#   share/vulkan/implicit_layer.d/VkLayer_FERRISCOPE_wsi.i686.json
#
# Each manifest carries its final library path (PREFIX, without DESTDIR); the
# Vulkan loader picks the one whose library matches the game's architecture.
# The layer is enabled by ENABLE_FERRISCOPE_WSI=1 and disabled by
# DISABLE_FERRISCOPE_WSI=1.
#
# The 32-bit layer (32-bit Vulkan games, 32-bit DXVK/vkd3d-proton) needs the
# i686-unknown-linux-gnu Rust target and a 32-bit C toolchain (cc -m32 with
# lib32 glibc; on Arch: lib32-rust-libs and lib32-glibc). Without them the
# script fails before installing anything.
#
# The build goes to CARGO_TARGET_DIR when it is set, and to the workspace's
# target/ otherwise.
#
# Run it as your own user: the layer is built first, and only the install step
# uses sudo, and only when the destination is not writable. Running the whole
# script under sudo is refused, because it would build as root (a root-owned
# target/ and cargo cache) with root's toolchain.
#
# To try the layer without installing into /usr, install into a scratch prefix
# and point the loader at it:
#   VK_ADD_IMPLICIT_LAYER_PATH=PREFIX/share/vulkan/implicit_layer.d
set -euo pipefail

prefix=/usr/local
profile=release
while [ $# -gt 0 ]; do
	case $1 in
	--prefix) prefix=$2; shift 2 ;;
	--profile) profile=$2; shift 2 ;;
	-h | --help) sed -n '2,/^set -euo/p' "$0" | sed 's/^# \{0,1\}//; /^set -euo/d'; exit 0 ;;
	*) echo "unknown argument: $1" >&2; exit 2 ;;
	esac
done

if [ "$(id -u)" = 0 ] && [ -n "${SUDO_USER:-}" ]; then
	echo "run $0 without sudo; it asks for sudo itself, only to install" >&2
	exit 1
fi

root=$(cd "$(dirname "$0")/.." && pwd)
destdir=${DESTDIR:-}
template=$root/tools/wsi/VkLayer_FERRISCOPE_wsi.json.in
target_dir=${CARGO_TARGET_DIR:-$root/target}
cargo_profile=(--release)
[ "$profile" = release ] || cargo_profile=()

# Run "$@" directly if the install destination is writable, otherwise via sudo.
# The nearest existing ancestor of the destination decides.
dest=$destdir$prefix
probe=$dest
while [ ! -e "$probe" ]; do probe=$(dirname "$probe"); done
if [ -w "$probe" ]; then
	as_installer() { "$@"; }
else
	echo "$dest is not writable; using sudo to install" >&2
	as_installer() { sudo "$@"; }
fi

# The 32-bit layer is required: check its toolchain before building anything.
i686=i686-unknown-linux-gnu
if [ ! -d "$(rustc --print sysroot)/lib/rustlib/$i686/lib" ]; then
	echo "the Rust target $i686 is missing (Arch: lib32-rust-libs; rustup: rustup target add $i686)" >&2
	exit 1
fi
if ! printf 'int main(void) { return 0; }\n' | cc -m32 -x c - -o /dev/null 2>/dev/null; then
	echo "cc -m32 cannot link a 32-bit program; install a 32-bit glibc (Arch: lib32-glibc, Debian: gcc-multilib)" >&2
	exit 1
fi

# Build both first, as the invoking user, so a build failure installs nothing
# and sudo is only asked for once the layers exist.
echo "building ferriscope-wsi (x86_64, i686)" >&2
(cd "$root" && CARGO_TARGET_DIR=$target_dir cargo build -p ferriscope-wsi "${cargo_profile[@]}")
(cd "$root" && CARGO_TARGET_DIR=$target_dir cargo build -p ferriscope-wsi --target "$i686" "${cargo_profile[@]}")

manifest_dir=$destdir$prefix/share/vulkan/implicit_layer.d
# install_layer ARCH BUILT LIBDIR
install_layer() {
	local library=$prefix/$3/ferriscope/libferriscope_wsi.so
	as_installer install -Dm755 "$2" "$destdir$library"
	sed -e "s|@ARCH@|$1|g" -e "s|@LIBRARY_PATH@|$library|g" "$template" |
		as_installer install -Dm644 /dev/stdin "$manifest_dir/VkLayer_FERRISCOPE_wsi.$1.json"
	echo "installed $destdir$library" >&2
}
install_layer x86_64 "$target_dir/$profile/libferriscope_wsi.so" lib
install_layer i686 "$target_dir/$i686/$profile/libferriscope_wsi.so" lib32
