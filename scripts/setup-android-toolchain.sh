#!/bin/bash
# AudioRelayPlus - Android build environment setup (JDK 17 + Gradle + Android SDK)
# Installs into your home directory, no sudo needed. Safe to run again.
set -euo pipefail

JDK_DIR="$HOME/.jdks/temurin-17"
GRADLE_VER="8.9"
GRADLE_DIR="$HOME/.local/share/gradle-$GRADLE_VER"
SDK_DIR="$HOME/Android/Sdk"
CMDTOOLS_URL="https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip"

log() { echo "[setup $(date +%H:%M:%S)] $*"; }

mkdir -p "$HOME/.jdks" "$HOME/.local/share" "$SDK_DIR" /tmp/arp-setup
cd /tmp/arp-setup

if [ ! -x "$JDK_DIR/bin/java" ]; then
  log "Downloading JDK 17 (Temurin)..."
  curl -fsSL -o jdk17.tar.gz \
    'https://api.adoptium.net/v3/binary/latest/17/ga/linux/x64/jdk/hotspot/normal/eclipse?project=jdk'
  mkdir -p "$JDK_DIR"
  tar xzf jdk17.tar.gz -C "$JDK_DIR" --strip-components=1
  log "JDK 17 ready: $JDK_DIR"
else
  log "JDK 17 already installed."
fi

if [ ! -x "$GRADLE_DIR/bin/gradle" ]; then
  log "Downloading Gradle $GRADLE_VER..."
  curl -fsSL -o gradle.zip "https://services.gradle.org/distributions/gradle-$GRADLE_VER-bin.zip"
  unzip -qo gradle.zip -d "$HOME/.local/share/"
  log "Gradle ready: $GRADLE_DIR"
else
  log "Gradle already installed."
fi

if [ ! -x "$SDK_DIR/cmdline-tools/latest/bin/sdkmanager" ]; then
  log "Downloading Android cmdline-tools..."
  curl -fsSL -o cmdtools.zip "$CMDTOOLS_URL"
  rm -rf "$SDK_DIR/cmdline-tools/latest" "$SDK_DIR/cmdline-tools/cmdline-tools"
  mkdir -p "$SDK_DIR/cmdline-tools"
  unzip -qo cmdtools.zip -d "$SDK_DIR/cmdline-tools"
  mv "$SDK_DIR/cmdline-tools/cmdline-tools" "$SDK_DIR/cmdline-tools/latest"
  log "cmdline-tools ready."
else
  log "cmdline-tools already installed."
fi

export JAVA_HOME="$JDK_DIR"
SDKMANAGER="$SDK_DIR/cmdline-tools/latest/bin/sdkmanager"

log "Accepting the SDK licenses..."
yes | "$SDKMANAGER" --sdk_root="$SDK_DIR" --licenses >/dev/null 2>&1 || true

log "Downloading SDK packages (platform-34, build-tools 34.0.0, platform-tools)..."
"$SDKMANAGER" --sdk_root="$SDK_DIR" \
  "platforms;android-34" "build-tools;34.0.0" "platform-tools" >/dev/null

log "TOOLCHAIN_READY"
echo "JAVA_HOME=$JDK_DIR"
echo "GRADLE=$GRADLE_DIR/bin/gradle"
echo "ANDROID_HOME=$SDK_DIR"
