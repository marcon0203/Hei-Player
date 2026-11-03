#!/usr/bin/env bash
# 打包 Hei Player macOS 客户端:.app(ad-hoc 签名)+ .dmg(带 /Applications 快捷方式)
# 用法:scripts/package-mac.sh   (先 cargo build --release -p aura-app)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="$ROOT/target/release/HeiPlayer"
APP_DIR="$ROOT/dist/HeiPlayer.app"
STAGE="$ROOT/dist/dmg-stage"
DMG="$ROOT/dist/HeiPlayer-0.1.0-arm64.dmg"
ARCH="$(uname -m)"

[ -x "$BIN" ] || { echo "❌ 未找到 $BIN,先执行:cargo build --release -p aura-app"; exit 1; }

echo "==> 组装 .app"
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS" "$APP_DIR/Contents/Resources"
cp "$BIN" "$APP_DIR/Contents/MacOS/HeiPlayer"
cp "$ROOT/crates/app/icons/icon.icns" "$APP_DIR/Contents/Resources/icon.icns"

cat > "$APP_DIR/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>HeiPlayer</string>
    <key>CFBundleDisplayName</key>       <string>HeiPlayer</string>
    <key>CFBundleExecutable</key>        <string>HeiPlayer</string>
    <key>CFBundleIdentifier</key>        <string>com.heiplayer.app</string>
    <key>CFBundleVersion</key>           <string>0.1.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
    <key>CFBundleIconFile</key>          <string>icon.icns</string>
    <key>LSMinimumSystemVersion</key>    <string>11.0</string>
    <key>LSApplicationCategoryType</key> <string>public.app-category.music</string>
    <key>NSHighResolutionCapable</key>   <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key> <true/>
</dict>
</plist>
PLIST

echo "==> ad-hoc 签名(本机可直跑;对外分发需 Developer ID + 公证)"
codesign --force --sign - "$APP_DIR"

echo "==> 生成 DMG"
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R "$APP_DIR" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "HeiPlayer" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

echo "✅ 完成($ARCH):"
ls -la "$ROOT/dist/" | grep -E "HeiPlayer"
echo "   双击运行: open $APP_DIR"
echo "   DMG 分发: $DMG(未 Developer ID 签名,他人首次打开需右键→打开)"
