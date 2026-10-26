#!/usr/bin/env bash
set -euo pipefail

KCOV_VERSION=43
KCOV_SHA256=4cbba86af11f72de0c7514e09d59c7927ed25df7cebdad087f6d3623213b95bf

wget -O kcov.tar.gz "https://github.com/SimonKagstrom/kcov/archive/refs/tags/v${KCOV_VERSION}.tar.gz"
echo "${KCOV_SHA256}  kcov.tar.gz" | sha256sum -c -
tar xzf kcov.tar.gz
cd "kcov-${KCOV_VERSION}"
mkdir build
cd build
cmake ..
make
make install DESTDIR=../../kcov-build
cd ../..
rm -rf "kcov-${KCOV_VERSION}" kcov.tar.gz
for file in target/debug/deps/sav1n-*; do
    [ -x "${file}" ] || continue
    "${file}" --list 2>/dev/null | grep ': test$' >/dev/null || continue
    mkdir -p "target/cov/$(basename "$file")"
    ./kcov-build/usr/local/bin/kcov --exclude-pattern=/.cargo,/usr/lib "target/cov/$(basename "$file")" "$file"
done
