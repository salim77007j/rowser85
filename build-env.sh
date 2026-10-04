# Source this before any cargo command in the rowser85 repo.
# Local deb-extracted native libs (alsa dev) — no root on this box.
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_INCREMENTAL=0
export PKG_CONFIG_PATH=/home/z/debs/extracted/usr/lib/x86_64-linux-gnu/pkgconfig
export LIBRARY_PATH=/home/z/debs/extracted/usr/lib/x86_64-linux-gnu
export LD_LIBRARY_PATH=/home/z/debs/extracted/usr/lib/x86_64-linux-gnu
