#!/bin/sh
# Builds the SDK harness tools. Needs Xcode command line tools and DDPM
# installed at /Applications/DDPM (the tools dlopen its SDK at runtime).
set -e
cd "$(dirname "$0")"
clang -dynamiclib -framework IOKit -framework CoreFoundation hidlog.c -o hidlog.dylib
clang sdkharness.c -o sdkharness
clang session.c -o session
echo "built: sdkharness, session, hidlog.dylib"
echo "log HID frames:  DYLD_INSERT_LIBRARIES=./hidlog.dylib ./sdkharness"
