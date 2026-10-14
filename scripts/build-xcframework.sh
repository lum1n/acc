#!/usr/bin/env bash
# Builds acc-ffi for iOS (device + simulator) and packages it as a Swift
# package in bindings/swift: AccFFI.xcframework plus the generated bindings
# as the `Acc` library. Apps that already ship another Rust static library
# should instead link acc-ffi into their own one (see README).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
OUT="$ROOT/bindings/swift"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios)
rustup target add "${TARGETS[@]}" 2>/dev/null || true
for t in "${TARGETS[@]}"; do
  IPHONEOS_DEPLOYMENT_TARGET=16.0 cargo build -p acc-ffi --release --target "$t"
done
cargo build -p acc-ffi --release
cargo run -p acc-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/release/libacc_ffi.dylib --language swift --out-dir "$STAGE/gen"

mkdir -p "$STAGE/headers" "$STAGE/sim"
cp "$STAGE/gen/acc_ffiFFI.h" "$STAGE/headers/"
cp "$STAGE/gen/acc_ffiFFI.modulemap" "$STAGE/headers/module.modulemap"
lipo -create target/aarch64-apple-ios-sim/release/libacc_ffi.a target/x86_64-apple-ios/release/libacc_ffi.a \
  -output "$STAGE/sim/libacc_ffi.a"

rm -rf "$OUT"
mkdir -p "$OUT/Sources/Acc"
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libacc_ffi.a -headers "$STAGE/headers" \
  -library "$STAGE/sim/libacc_ffi.a" -headers "$STAGE/headers" \
  -output "$OUT/AccFFI.xcframework"
cp "$STAGE/gen/acc_ffi.swift" "$OUT/Sources/Acc/"
cat >"$OUT/Package.swift" <<'PKG'
// swift-tools-version:5.9
import PackageDescription

let package = Package(
  name: "Acc",
  platforms: [.iOS(.v16), .macOS(.v13)],
  products: [.library(name: "Acc", targets: ["Acc"])],
  targets: [
    .binaryTarget(name: "AccFFI", path: "AccFFI.xcframework"),
    .target(name: "Acc", dependencies: ["AccFFI"], path: "Sources/Acc"),
  ]
)
PKG
echo "Swift package ready in bindings/swift"
