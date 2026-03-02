# Perry Publish — Technical Specification

**Version:** 0.1.0-draft
**Date:** February 25, 2026
**Author:** Skelpo GmbH

---

## 1. Overview

Perry Publish extends the Perry compiler with a complete build-sign-package-distribute pipeline, enabling developers to go from TypeScript source to published applications on all major platforms with a single command.

### 1.1 Design Principles

- **Server-first by default**, local build as opt-in for power users
- **Zero cost for free-tier users** — no servers idle, no storage persists
- **Credentials never stored** — everything in-memory, wiped after build
- **Open source server** — self-hostable, full transparency
- **Progressive disclosure** — free for one platform, paid for multi-platform

### 1.2 Target Platforms

| Platform | Output Format | Signing Method | Distribution |
|----------|--------------|----------------|--------------|
| macOS | .dmg / .app | Developer ID + Notarization | Direct / Homebrew |
| iOS | .ipa | App Store provisioning profile | App Store Connect |
| Android | .aab / .apk | Keystore signing | Google Play Console |
| Windows | .exe / .msix | Code signing certificate | Direct / Microsoft Store |
| Linux | .deb / .AppImage | GPG signing (optional) | Direct / apt repo |

---

## 2. User Experience

### 2.1 First-Time Flow

```
$ perry publish --macos

  Perry Publish v0.1.0

  First time? Let's get you set up.
  Authenticate with GitHub to get your free license.

  → Open https://github.com/login/device
  → Enter code: ABCD-1234

  ✓ Licensed to @username (free tier: 1 platform)
  License saved to ~/.perry/config.toml

  Now let's set up macOS signing.
  Run: perry auth apple

```

### 2.2 Authentication Flow

```
$ perry auth apple

  Perry needs an App Store Connect API key.

  1. Go to https://appstoreconnect.apple.com/access/api
  2. Click "Generate API Key" (requires Admin role)
  3. Download the .p8 file

  Enter Key ID: XXXXXXXXXX
  Enter Issuer ID: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
  Select .p8 file: ~/Downloads/AuthKey_XXXXXXXXXX.p8

  ✓ Apple credentials saved to ~/.perry/credentials/apple.toml
  ✓ Credentials are encrypted at rest

  Test connection? [Y/n] Y
  ✓ Connected to App Store Connect as "Ralph - Skelpo GmbH"
```

```
$ perry auth google

  Perry needs a Google Play service account.

  1. Go to https://play.google.com/console/developers
  2. Settings → API access → Create service account
  3. Download the JSON key file

  Select JSON key file: ~/Downloads/service-account.json

  ✓ Google Play credentials saved to ~/.perry/credentials/google.toml
  ✓ Credentials are encrypted at rest

  Test connection? [Y/n] Y
  ✓ Connected to Google Play Console as "Skelpo GmbH"
```

### 2.3 Publishing Flow

```
$ perry publish --ios

  Reading perry.toml...
  Packaging project (2.3 MB)...

  ⠋ Uploading to Perry Build Service...
  ✓ Uploaded (1.2s)

  ⠋ Compiling for iOS (aarch64-apple-ios)...
    src/main.ts
    src/ui.ts
    src/api.ts
    src/components/header.ts
  ✓ Compiled (4.2s)

  ⠋ Processing assets...
    Generating App Icons (20 sizes from icon.png)
    Generating Launch Screen
    Compiling Asset Catalog
  ✓ Assets ready

  ⠋ Bundling MyApp.app...
    Generating Info.plist
    Embedding provisioning profile
    Creating app bundle structure
  ✓ Bundled

  ⠋ Code signing...
    Signing with "Apple Distribution: Skelpo GmbH (ABC123)"
    Signing frameworks and embedded content
  ✓ Signed

  ⠋ Creating .ipa...
  ✓ Packaged

  ⠋ Uploading to App Store Connect...
    Uploading MyApp-1.0.0.ipa (18.3 MB)
    ████████████████████████████████ 100%
    Validating with App Store...
  ✓ Uploaded

  🎉 MyApp 1.0.0 submitted to App Store Connect

  Build:    #1234
  Status:   Waiting for Review
  Track at: https://appstoreconnect.apple.com/apps/1234567890

```

### 2.4 Multi-Platform Publishing

```
$ perry publish --ios --android --macos

  ⚠ Multi-platform publishing requires Perry Pro (€29/mo)
  → Upgrade at https://perry.dev/pro/@username

  Already have a license key? Enter it: XXXX-XXXX-XXXX-XXXX
  ✓ Perry Pro activated

  Starting parallel builds for 3 targets...

  iOS (aarch64-apple-ios)
  ✓ Compiled (4.2s)
  ✓ Bundled & Signed
  ⠋ Uploading to App Store Connect...

  Android (aarch64-linux-android)
  ✓ Compiled (3.8s)
  ✓ Bundled & Signed
  ⠋ Uploading to Google Play Console...

  macOS (aarch64-apple-darwin)
  ✓ Compiled (3.1s)
  ✓ Bundled & Signed
  ⠋ Notarizing with Apple...

  ✓ All 3 targets published successfully

  iOS:     Waiting for Review
  Android: In Review
  macOS:   MyApp-1.0.0.dmg ready

```

### 2.5 Local Build Override

```
$ perry publish --macos --local

  Building locally...
  ⚠ Local builds require toolchain dependencies.
    Checking: codesign ✓  notarytool ✓  hdiutil ✓

  ⠋ Compiling for macOS (aarch64-apple-darwin)...
  ✓ Compiled (3.1s)
  ⠋ Bundling...
  ✓ Bundled
  ⠋ Signing...
  ✓ Signed
  ⠋ Notarizing...
  ✓ Notarized (43s)
  ⠋ Creating DMG...
  ✓ MyApp-1.0.0.dmg (24.1 MB)

  Output: ./dist/MyApp-1.0.0.dmg
```

---

## 3. Configuration

### 3.1 perry.toml — Full Schema

```toml
[app]
name = "My App"                      # Display name
version = "1.0.0"                    # Semver
description = "A cool app"           # Short description
author = "Developer Name"
url = "https://myapp.dev"
entry = "src/main.ts"                # Application entry point

[app.icons]
source = "assets/icon.png"           # Minimum 1024x1024, Perry generates all sizes
# Optional per-platform overrides:
# macos = "assets/icon-macos.png"
# ios = "assets/icon-ios.png"
# android = "assets/icon-android.png"

[app.splash]
source = "assets/splash.png"         # iOS/Android launch screen
background = "#FFFFFF"               # Background color


# ─── Platform Targets ───────────────────────────────────────

[macos]
bundle_id = "com.developer.myapp"
category = "public.app-category.developer-tools"  # LSApplicationCategoryType
minimum_os = "13.0"
sandbox = true
entitlements = [                     # macOS sandbox entitlements
  "com.apple.security.network.client",
  "com.apple.security.files.user-selected.read-write"
]
# Distribution options
distribute = "direct"                # "direct" (DMG) | "appstore" | "homebrew"


[ios]
bundle_id = "com.developer.myapp"
deployment_target = "16.0"
device_family = ["iphone", "ipad"]   # or just ["iphone"]
orientations = ["portrait"]          # portrait, landscape-left, landscape-right, upside-down
capabilities = [                     # iOS capabilities / entitlements
  "push-notifications",
  "camera",
  "photo-library"
]
app_category = "utilities"           # App Store category
distribute = "appstore"              # "appstore" | "testflight" | "adhoc"


[android]
package = "com.developer.myapp"
min_sdk = 26                         # Android 8.0
target_sdk = 34                      # Android 14
permissions = [
  "INTERNET",
  "CAMERA",
  "READ_EXTERNAL_STORAGE"
]
features = [
  { name = "android.hardware.camera", required = false }
]
distribute = "playstore"             # "playstore" | "direct" (APK)
track = "production"                 # "internal" | "alpha" | "beta" | "production"


[windows]
package_name = "com.developer.myapp"
minimum_os = "10.0.17763.0"          # Windows 10 1809
capabilities = [
  "internetClient",
  "webcam"
]
distribute = "direct"                # "direct" (EXE) | "msstore"


[linux]
package_name = "myapp"
categories = ["Development", "Utility"]
depends = []                         # apt dependencies
distribute = "direct"                # "direct" (AppImage) | "deb" | "rpm"


# ─── Build Service Options ──────────────────────────────────

[publish]
server = "https://build.perry.dev"   # Default Perry Build Service
# server = "https://build.mycompany.com"  # Self-hosted
# server = "local"                   # Force local builds
timeout = 600                        # Build timeout in seconds
```

### 3.2 Minimal perry.toml (Vibe Coder Edition)

The absolute minimum to publish an iOS app:

```toml
[app]
name = "My App"
version = "1.0.0"
entry = "src/main.ts"

[app.icons]
source = "icon.png"

[ios]
bundle_id = "com.me.myapp"
```

Everything else has sensible defaults. Perry infers what it can and fills in the rest.

### 3.3 Local Credentials Storage

```
~/.perry/
├── config.toml              # License key, GitHub identity, preferences
├── credentials/
│   ├── apple.toml           # App Store Connect API key (encrypted)
│   ├── google.toml          # Play Store service account (encrypted)
│   ├── windows.toml         # Windows code signing cert (encrypted)
│   └── keystore.jks         # Android signing keystore (encrypted)
└── cache/
    └── ... (build cache, optional)
```

Credential files are encrypted at rest using a key derived from the machine's hardware identity. They can only be decrypted on the same machine. This means credentials are usable but not extractable.

---

## 4. Build Server Architecture

### 4.1 Overview

The Perry Build Server is a standalone open source Rust application that receives build requests over WebSocket, compiles and packages applications, and returns artifacts or publishes directly to app stores.

### 4.2 Infrastructure (Launch)

```
                    ┌─────────────────────────┐
                    │   Cloudflare Tunnel      │
                    │   build.perry.dev        │
                    └────────┬────────────────┘
                             │
                    ┌────────▼────────────────┐
                    │   Mac Mini M4            │
                    │                          │
                    │   perry-build-server     │
                    │   ├── Rust/Axum          │
                    │   ├── Job Queue          │
                    │   └── Build Workers      │
                    │                          │
                    │   Toolchains:            │
                    │   ├── Perry compiler     │
                    │   ├── Apple SDK          │
                    │   ├── Android SDK        │
                    │   ├── Windows xtools     │
                    │   └── Linux targets      │
                    │                          │
                    │   NO persistent storage  │
                    │   NO database            │
                    │   NO user data at rest   │
                    └─────────────────────────┘
```

### 4.3 Server Project Structure

```
perry-build-server/
├── Cargo.toml
├── src/
│   ├── main.rs                    # Entry point, Axum server setup
│   ├── config.rs                  # Server configuration
│   ├── api/
│   │   ├── mod.rs
│   │   ├── routes.rs              # HTTP endpoints
│   │   └── ws.rs                  # WebSocket handler
│   ├── auth/
│   │   ├── mod.rs
│   │   ├── license.rs             # License key validation
│   │   └── github.rs              # GitHub device flow
│   ├── queue/
│   │   ├── mod.rs
│   │   ├── job.rs                 # Job definition
│   │   └── worker.rs              # Worker pool
│   ├── build/
│   │   ├── mod.rs
│   │   ├── pipeline.rs            # Build orchestration
│   │   ├── compiler.rs            # Runs perry build
│   │   ├── assets.rs              # Icon/splash generation
│   │   └── cleanup.rs             # Secure tmpdir wipe
│   ├── package/
│   │   ├── mod.rs
│   │   ├── macos.rs               # .app bundle + .dmg creation
│   │   ├── ios.rs                 # .app bundle + .ipa creation
│   │   ├── android.rs             # .apk / .aab creation
│   │   ├── windows.rs             # .exe / .msix creation
│   │   └── linux.rs               # .deb / .AppImage creation
│   ├── signing/
│   │   ├── mod.rs
│   │   ├── apple.rs               # codesign + notarytool
│   │   ├── android.rs             # apksigner / jarsigner
│   │   ├── windows.rs             # osslsigncode
│   │   └── gpg.rs                 # Linux package signing
│   ├── publish/
│   │   ├── mod.rs
│   │   ├── appstore.rs            # App Store Connect API
│   │   ├── playstore.rs           # Google Play Developer API
│   │   ├── msstore.rs             # Microsoft Store submission API
│   │   └── homebrew.rs            # Homebrew formula generation
│   └── ws/
│       ├── mod.rs
│       └── messages.rs            # WebSocket message types
├── tests/
│   ├── integration/
│   │   ├── build_macos.rs
│   │   ├── build_ios.rs
│   │   ├── build_android.rs
│   │   └── signing.rs
│   └── fixtures/
│       └── sample_project/
└── README.md
```

### 4.4 Data Flow

```
Phase 1: UPLOAD
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Client:
  1. Reads perry.toml
  2. Creates tarball of project directory
     - Respects .perryignore (like .gitignore)
     - Excludes: node_modules, .git, dist, build
  3. Reads credentials from ~/.perry/credentials/
  4. Opens WebSocket to build.perry.dev
  5. Sends initial handshake:

  → HTTP POST /api/v1/build (upgrades to WebSocket)

  Request body (multipart):
  ┌──────────────────────────────────────────┐
  │ project.tar.gz          (source code)    │
  │ manifest.json           (see §4.5)       │
  │ credentials.json        (see §4.6)       │
  └──────────────────────────────────────────┘


Phase 2: BUILD
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Server:
  1. Validates license key
  2. Creates isolated tmpdir: /tmp/perry-build-{uuid}/
  3. Extracts tarball
  4. Streams progress via WebSocket

  ┌─ For each target platform: ─────────────────────────┐
  │                                                      │
  │  a. perry build --target {target_triple}             │
  │     → Compiles TypeScript to native binary           │
  │     → Streams compiler output to client              │
  │                                                      │
  │  b. Asset processing                                 │
  │     → Resize icon.png to all required sizes          │
  │     → Generate asset catalogs (iOS: Assets.car)      │
  │     → Generate adaptive icons (Android)              │
  │     → Create launch screens                          │
  │                                                      │
  │  c. Bundle creation (see §5 per platform)            │
  │     → Create platform-specific app structure         │
  │     → Generate Info.plist / AndroidManifest.xml      │
  │     → Embed resources and assets                     │
  │                                                      │
  │  d. Code signing (credentials held in memory)        │
  │     → Sign binary and bundle                         │
  │     → Platform-specific signing (see §6)             │
  │                                                      │
  │  e. Package final artifact                           │
  │     → .ipa / .dmg / .aab / .exe / .AppImage         │
  │                                                      │
  └──────────────────────────────────────────────────────┘


Phase 3: DISTRIBUTE
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Server (based on distribute setting in perry.toml):

  IF distribute = "appstore" / "playstore" / "msstore":
    → Upload artifact to store API using user's credentials
    → Stream upload progress
    → Return store confirmation / review status URL

  IF distribute = "direct" / "testflight" / "adhoc":
    → Write artifact to temp location
    → Return short-lived download URL (10 min TTL)
    → Client downloads artifact
    → Server deletes artifact


Phase 4: CLEANUP
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Server (IMMEDIATELY after build completes or fails):
  1. Wipe tmpdir: rm -rf /tmp/perry-build-{uuid}/
  2. Zero credentials from memory
  3. Delete any temporary artifacts after download or timeout
  4. No logs retained containing source code or credentials
```

### 4.5 Build Manifest (manifest.json)

Sent by the CLI with each build request:

```json
{
  "version": "1",
  "license_key": "XXXX-XXXX-XXXX-XXXX",
  "github_username": "ralph",
  "targets": ["aarch64-apple-ios"],
  "perry_version": "0.5.0",
  "app": {
    "name": "My App",
    "version": "1.0.0",
    "bundle_id": "com.developer.myapp",
    "entry": "src/main.ts"
  },
  "platform_config": {
    "ios": {
      "deployment_target": "16.0",
      "device_family": ["iphone", "ipad"],
      "orientations": ["portrait"],
      "capabilities": ["push-notifications"],
      "distribute": "appstore"
    }
  },
  "options": {
    "upload": true,
    "timeout": 600
  }
}
```

### 4.6 Credentials Payload (credentials.json)

Sent encrypted in transit (TLS), held only in-memory on server:

```json
{
  "apple": {
    "key_id": "XXXXXXXXXX",
    "issuer_id": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
    "p8_key": "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----",
    "team_id": "ABC123DEF",
    "signing_identity": "Apple Distribution: Skelpo GmbH (ABC123DEF)"
  },
  "android": {
    "keystore": "<base64-encoded keystore>",
    "keystore_password": "...",
    "key_alias": "upload",
    "key_password": "...",
    "service_account": { /* Google Play service account JSON */ }
  },
  "windows": {
    "certificate": "<base64-encoded PFX>",
    "password": "..."
  }
}
```

---

## 5. Platform Bundle Specifications

### 5.1 macOS (.app → .dmg)

```
MyApp.app/
├── Contents/
│   ├── Info.plist               # Generated from perry.toml
│   ├── MacOS/
│   │   └── MyApp                # Native binary (from perry build)
│   ├── Resources/
│   │   ├── AppIcon.icns         # Generated from icon.png
│   │   ├── Assets.car           # Compiled asset catalog (if needed)
│   │   └── en.lproj/
│   │       └── InfoPlist.strings
│   ├── Frameworks/              # Embedded frameworks (if any)
│   └── _CodeSignature/
│       └── CodeResources         # Generated by codesign
```

**Info.plist generation** — Perry generates this entirely from perry.toml:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "...">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>       <string>MyApp</string>
  <key>CFBundleIdentifier</key>       <string>com.developer.myapp</string>
  <key>CFBundleName</key>             <string>My App</string>
  <key>CFBundleVersion</key>          <string>1</string>
  <key>CFBundleShortVersionString</key> <string>1.0.0</string>
  <key>CFBundleIconFile</key>         <string>AppIcon</string>
  <key>LSMinimumSystemVersion</key>   <string>13.0</string>
  <key>LSApplicationCategoryType</key> <string>public.app-category.developer-tools</string>
  <key>CFBundlePackageType</key>      <string>APPL</string>
  <key>NSHighResolutionCapable</key>  <true/>
  <!-- Sandbox entitlements reference -->
  <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
</dict>
</plist>
```

**DMG creation** (no Xcode required):

```bash
# Create temporary DMG directory
mkdir -p dmg_contents
cp -R MyApp.app dmg_contents/
ln -s /Applications dmg_contents/Applications

# Create DMG
hdiutil create -volname "My App" \
  -srcfolder dmg_contents \
  -ov -format UDZO \
  MyApp-1.0.0.dmg
```

### 5.2 iOS (.app → .ipa)

```
Payload/
└── MyApp.app/
    ├── Info.plist               # Generated, includes UIDevice requirements
    ├── MyApp                    # Native binary (ARM64)
    ├── Assets.car               # Compiled asset catalog (icons, images)
    ├── LaunchScreen.storyboardc # Compiled launch screen (or generated)
    ├── embedded.mobileprovision # Provisioning profile
    ├── Frameworks/              # Embedded frameworks
    └── _CodeSignature/
        └── CodeResources
```

**IPA creation** (no Xcode required):

```bash
# The .ipa is literally a zip of the Payload directory
mkdir -p Payload
cp -R MyApp.app Payload/
zip -r MyApp.ipa Payload/
```

**Key challenge: Provisioning profiles.** The user needs to have created a provisioning profile in their Apple Developer account that matches the bundle ID. Perry can guide them through this or automate it via the App Store Connect API.

**Asset catalog compilation** (requires `actool` from Xcode CLI tools, NOT full Xcode):

```bash
# Only Xcode Command Line Tools needed, not full Xcode
xcrun actool Assets.xcassets \
  --compile output/ \
  --platform iphoneos \
  --minimum-deployment-target 16.0
```

> **Note:** Investigate alternatives to `actool` for fully Xcode-free builds.
> The asset catalog format is documented and could be generated in pure Rust.
> This is a stretch goal for v1.0.

### 5.3 Android (.apk / .aab)

```
APK structure:
├── AndroidManifest.xml          # Generated from perry.toml (binary XML)
├── classes.dex                  # Empty or minimal (native app)
├── lib/
│   └── arm64-v8a/
│       └── libmyapp.so         # Native binary (from perry build)
├── res/
│   ├── mipmap-xxxhdpi/
│   │   └── ic_launcher.png     # Generated from icon.png
│   ├── mipmap-xxhdpi/
│   │   └── ic_launcher.png
│   ├── mipmap-xhdpi/
│   │   └── ic_launcher.png
│   ├── mipmap-hdpi/
│   │   └── ic_launcher.png
│   └── mipmap-mdpi/
│       └── ic_launcher.png
├── resources.arsc               # Compiled resources
└── META-INF/
    └── CERT.SF / CERT.RSA       # Signing info
```

**AAB (Android App Bundle)** — preferred for Play Store, similar structure but uses Google's bundletool format.

**No Android Studio required.** Build process:
1. Perry compiles to `libmyapp.so` (aarch64-linux-android target)
2. Generate `AndroidManifest.xml` from perry.toml
3. Resize icons to all density buckets
4. Use `aapt2` to compile resources (standalone binary, no IDE)
5. Package into APK/AAB
6. Sign with `apksigner` (standalone, from Android SDK build-tools)

Android SDK build-tools can be installed standalone (~200MB) without Android Studio (~2GB+).

### 5.4 Windows (.exe / .msix)

```
MyApp/
├── MyApp.exe                    # Native binary (from perry build)
├── app.manifest                 # Windows application manifest
├── resources.rc                 # Resource file (icon, version info)
└── MyApp.ico                    # Generated from icon.png
```

**MSIX package** (for Microsoft Store):

```
MyApp.msix
├── AppxManifest.xml             # Generated from perry.toml
├── Assets/
│   ├── Square150x150Logo.png
│   ├── Square44x44Logo.png
│   ├── StoreLogo.png
│   └── Wide310x150Logo.png
├── MyApp.exe
└── AppxBlockMap.xml
```

**No Visual Studio required.** Code signing on Linux/macOS:

```bash
# osslsigncode works on all platforms
osslsigncode sign \
  -pkcs12 cert.pfx \
  -pass "password" \
  -n "My App" \
  -t http://timestamp.digicert.com \
  -in MyApp.exe \
  -out MyApp-signed.exe
```

### 5.5 Linux (.AppImage / .deb)

```
AppImage structure:
├── AppRun                       # Entry point script
├── myapp.desktop                # Desktop entry file
├── myapp.png                    # App icon
└── usr/
    ├── bin/
    │   └── myapp                # Native binary
    └── share/
        └── applications/
            └── myapp.desktop

DEB structure:
├── DEBIAN/
│   ├── control                  # Package metadata
│   ├── postinst                 # Post-install script (optional)
│   └── prerm                    # Pre-remove script (optional)
└── usr/
    ├── bin/
    │   └── myapp
    └── share/
        ├── applications/
        │   └── myapp.desktop
        └── icons/
            └── hicolor/
                └── 256x256/
                    └── apps/
                        └── myapp.png
```

---

## 6. Signing Details

### 6.1 macOS Signing Pipeline

```
Step 1: Code sign the binary
  codesign --force --options runtime \
    --sign "Developer ID Application: Name (TEAM_ID)" \
    --entitlements entitlements.plist \
    MyApp.app

Step 2: Notarize
  xcrun notarytool submit MyApp.dmg \
    --key /path/to/key.p8 \
    --key-id KEY_ID \
    --issuer ISSUER_ID \
    --wait

Step 3: Staple (after notarization approved)
  xcrun stapler staple MyApp.dmg
```

**Without Xcode:** `codesign` ships with macOS. `notarytool` and `stapler` ship with Xcode Command Line Tools (small install, not full Xcode). Alternatively, notarization can be done entirely via the App Store Connect REST API — no local tools needed on the build server beyond `codesign`.

### 6.2 iOS Signing Pipeline

```
Step 1: Sign the .app
  codesign --force \
    --sign "Apple Distribution: Name (TEAM_ID)" \
    --entitlements entitlements.plist \
    Payload/MyApp.app

Step 2: Package .ipa
  zip -r MyApp.ipa Payload/

Step 3: Upload via App Store Connect API
  # REST API - no local tools needed
  POST https://api.appstoreconnect.apple.com/v1/builds
```

### 6.3 Android Signing Pipeline

```
Step 1: Build unsigned APK/AAB
  (perry handles this)

Step 2: Zipalign
  zipalign -v 4 unsigned.apk aligned.apk

Step 3: Sign
  apksigner sign \
    --ks keystore.jks \
    --ks-pass pass:PASSWORD \
    --ks-key-alias upload \
    aligned.apk

Step 4: Upload via Play Developer API
  # REST API
  POST https://androidpublisher.googleapis.com/...
```

### 6.4 Windows Signing Pipeline

```
Step 1: Sign EXE
  osslsigncode sign \
    -pkcs12 certificate.pfx \
    -pass PASSWORD \
    -n "My App" \
    -t http://timestamp.digicert.com \
    -in MyApp.exe \
    -out MyApp-signed.exe

Step 2: (Optional) Create MSIX and sign
  # makemsix or MSIX Packaging Tool
```

---

## 7. WebSocket Protocol

### 7.1 Connection Lifecycle

```
Client                              Server
  │                                   │
  ├── POST /api/v1/build ────────────→│ Upload tarball + manifest + credentials
  │                                   │ Validate license
  │                                   │ Create job
  │←── 101 Switching Protocols ───────│ Upgrade to WebSocket
  │                                   │
  │←── { type: "job_created" }  ──────│
  │←── { type: "stage" }       ──────│
  │←── { type: "log" }         ──────│ ... repeated
  │←── { type: "progress" }    ──────│ ... repeated
  │←── { type: "stage" }       ──────│
  │←── { type: "complete" }    ──────│
  │                                   │
  │── close ──────────────────────────│
```

### 7.2 Message Types

**Server → Client messages:**

```typescript
// Build job accepted
{
  type: "job_created",
  job_id: "uuid",
  position: 0,              // Queue position (0 = building now)
  estimated_seconds: 30
}

// Queue update (if waiting)
{
  type: "queue_update",
  position: 2,
  estimated_seconds: 60
}

// Build stage transition
{
  type: "stage",
  name: "compiling" | "assets" | "bundling" | "signing" |
        "notarizing" | "packaging" | "uploading" | "complete" | "failed",
  target: "aarch64-apple-ios"    // Which platform (for parallel builds)
}

// Log line
{
  type: "log",
  message: "compiling src/main.ts",
  target: "aarch64-apple-ios",
  timestamp: "2026-02-25T14:30:00Z"
}

// Progress update
{
  type: "progress",
  percent: 45,
  target: "aarch64-apple-ios"
}

// Build complete — artifact available for download
{
  type: "artifact_ready",
  target: "aarch64-apple-ios",
  artifact_url: "https://build.perry.dev/dl/tmp-abc123",
  artifact_name: "MyApp-1.0.0.ipa",
  artifact_size: 18_300_000,
  expires_at: "2026-02-25T14:40:00Z",   // 10 minute TTL
  checksums: {
    sha256: "abc123..."
  }
}

// Build complete — published to store
{
  type: "published",
  target: "aarch64-apple-ios",
  store: "appstore",
  status: "waiting_for_review",
  url: "https://appstoreconnect.apple.com/apps/1234567890"
}

// Error
{
  type: "error",
  code: "SIGNING_FAILED",
  message: "Your App Store Connect API key has expired.",
  hint: "Generate a new one at:\nhttps://appstoreconnect.apple.com/access/api\n\nThen run: perry auth apple",
  target: "aarch64-apple-ios",
  fatal: true                    // true = build stopped, false = warning
}

// Final summary
{
  type: "complete",
  success: true,
  duration_seconds: 47,
  targets: {
    "aarch64-apple-ios": {
      status: "published",
      store_url: "..."
    },
    "aarch64-apple-darwin": {
      status: "artifact_ready",
      download_url: "..."
    }
  }
}
```

**Client → Server messages:**

```typescript
// Cancel build
{
  type: "cancel",
  job_id: "uuid"
}

// Keepalive (every 30s)
{
  type: "ping"
}
```

### 7.3 Error Codes

| Code | Meaning | User Hint |
|------|---------|-----------|
| `LICENSE_INVALID` | License key not recognized | Check key at perry.dev/account |
| `LICENSE_TIER` | Feature requires Pro | Upgrade at perry.dev/pro |
| `UPLOAD_TOO_LARGE` | Project tarball exceeds limit | Check .perryignore, max 100MB |
| `COMPILE_FAILED` | Perry compilation error | Fix source code errors shown in log |
| `SIGNING_FAILED` | Code signing failed | Check credentials with `perry auth check` |
| `SIGNING_EXPIRED` | Certificate/key expired | Renew at provider, run `perry auth {platform}` |
| `PROFILE_MISMATCH` | Bundle ID vs provisioning profile | Ensure IDs match in Apple Developer portal |
| `NOTARIZE_FAILED` | Apple rejected notarization | Check Apple's rejection reason in logs |
| `NOTARIZE_TIMEOUT` | Apple took too long | Retry with `perry publish --ios` |
| `UPLOAD_FAILED` | Store upload failed | Check network, retry |
| `STORE_REJECTED` | Store rejected binary | Check store-specific error in logs |
| `QUEUE_FULL` | Build queue is full | Retry in a few minutes |
| `TIMEOUT` | Build exceeded timeout | Increase timeout in perry.toml or simplify build |
| `SERVER_ERROR` | Internal server error | Retry, if persistent report at github.com/perrydev |
| `CREDENTIAL_MISSING` | No credentials for target | Run `perry auth {platform}` |

---

## 8. API Endpoints

### 8.1 Build API

```
POST /api/v1/build
  Content-Type: multipart/form-data
  Authorization: Bearer {license_key}

  Parts:
    project    - application/gzip (tarball)
    manifest   - application/json
    credentials - application/json (encrypted in transit via TLS)

  Response: 101 Upgrade to WebSocket

  Error responses:
    401 - Invalid license key
    402 - Tier upgrade required
    413 - Project too large
    429 - Rate limited
    503 - Queue full
```

### 8.2 Artifact Download

```
GET /api/v1/dl/{token}
  Response: application/octet-stream

  The token is single-use and expires after 10 minutes.

  Error responses:
    404 - Token expired or invalid
    410 - Artifact already downloaded
```

### 8.3 License API

```
POST /api/v1/license/register
  Body: { "github_username": "ralph", "github_token": "..." }
  Response: { "license_key": "FREE-XXXX-XXXX-XXXX", "tier": "free" }

POST /api/v1/license/verify
  Body: { "license_key": "XXXX-XXXX-XXXX-XXXX" }
  Response: { "valid": true, "tier": "free", "platforms": 1 }
```

### 8.4 Health / Status

```
GET /api/v1/status
  Response: {
    "status": "ok",
    "queue_length": 2,
    "perry_version": "0.5.0",
    "supported_targets": [
      "aarch64-apple-darwin",
      "aarch64-apple-ios",
      "aarch64-linux-android",
      "x86_64-unknown-linux-gnu",
      "x86_64-pc-windows-msvc"
    ]
  }
```

---

## 9. Security Model

### 9.1 Principles

1. **Zero trust in persistence.** No source code, credentials, or artifacts are written to permanent storage. Everything lives in tmpdir and memory.
2. **In-memory credentials.** User credentials are deserialized into memory, used during the build, then explicitly zeroed (using `zeroize` crate in Rust).
3. **Process isolation.** Each build runs in its own tmpdir with restricted filesystem access.
4. **TLS everywhere.** All communication between CLI and build server is over TLS.
5. **Open source verification.** The entire server is open source. Users can audit exactly what happens with their credentials and code.

### 9.2 Credential Lifecycle on Server

```
1. Request received over TLS
2. Credentials deserialized into memory (never written to disk)
3. Used for signing/uploading during build
4. After build completes (success or failure):
   a. Credential memory explicitly zeroed (zeroize crate)
   b. Credential variables dropped
5. No logs contain credential material
6. No crash dumps include credential memory (configured at OS level)
```

### 9.3 Source Code Lifecycle on Server

```
1. Tarball extracted to /tmp/perry-build-{uuid}/
2. Compilation happens in this directory
3. After build completes (success or failure):
   a. rm -rf /tmp/perry-build-{uuid}/
   b. Verified deletion
4. No source code in logs (only filenames)
5. Artifacts deleted after download or 10-minute TTL
```

### 9.4 Rate Limiting

| Tier | Concurrent builds | Builds per hour | Max project size |
|------|-------------------|-----------------|------------------|
| Free | 1 | 5 | 50 MB |
| Pro | 3 | 30 | 200 MB |
| Self-hosted | Unlimited | Unlimited | Unlimited |

---

## 10. License & Pricing

### 10.1 Tiers

| Feature | Free | Pro (€29/mo) |
|---------|------|--------------|
| Perry compiler | ✓ | ✓ |
| `perry build` (local) | ✓ | ✓ |
| `perry publish` (1 platform) | ✓ | ✓ |
| `perry publish` (multi-platform) | ✗ | ✓ |
| Build server usage | ✓ (rate limited) | ✓ (higher limits) |
| Priority queue | ✗ | ✓ |
| Email support | Community | Direct |
| Self-hosted server | ✓ | ✓ |

### 10.2 License Key Format

```
FREE-XXXX-XXXX-XXXX     (free tier, auto-generated)
PRO-XXXX-XXXX-XXXX      (paid tier, via Stripe/Polar.sh)
```

### 10.3 Payment Infrastructure

- **Polar.sh or Lemon Squeezy** for payment processing
- Handles EU VAT (important for Skelpo GmbH)
- Simple checkout page at perry.dev/pro
- License key delivered instantly via email + shown in CLI
- No recurring billing infrastructure needed on our side

---

## 11. Error Handling Philosophy

Every error message follows this structure:

```
✗ {What went wrong}

  {Why it went wrong — one sentence}
  {Exact next step — specific command or URL}
```

### 11.1 Examples

```
✗ Signing failed

  Your App Store Connect API key has expired.
  Generate a new one at:
  https://appstoreconnect.apple.com/access/api

  Then run: perry auth apple
```

```
✗ Compilation failed

  Type error in src/api.ts:42
  Property 'name' does not exist on type 'User'.

  Fix the error and run: perry publish --ios
```

```
✗ Bundle ID mismatch

  perry.toml says "com.dev.myapp" but your provisioning
  profile is for "com.developer.myapp".

  Either:
  • Update bundle_id in perry.toml to "com.developer.myapp"
  • Create a new provisioning profile at:
    https://developer.apple.com/account/resources/profiles

```

```
✗ Upload to Play Store failed

  Your app's version code (1) is not higher than the
  current live version (1).

  Bump version in perry.toml:
  version = "1.0.1"
```

```
✗ Notarization rejected by Apple

  Apple's response: "The binary uses a private API (IOKit)."

  This usually means a dependency is using restricted APIs.
  Check: https://developer.apple.com/documentation/security/notarizing_macos_software_before_distribution
```

### 11.2 Vibe Coder Friendly Hints

For common issues that assume zero knowledge:

```
✗ No Apple Developer account found

  To publish iOS apps, you need an Apple Developer account ($99/year).

  1. Sign up at https://developer.apple.com/programs/
  2. Wait for approval (usually 24-48 hours)
  3. Then run: perry auth apple

  💡 Tip: You can test locally with: perry build --ios --simulator
```

```
✗ No signing keystore found

  Android apps must be signed with a keystore before publishing.
  Perry can create one for you.

  Run: perry auth android --create-keystore

  ⚠ IMPORTANT: Back up the keystore file! If you lose it,
  you cannot update your app on the Play Store.
```

---

## 12. CLI Command Reference

```
perry publish [OPTIONS]

TARGETS (at least one required):
  --macos          Build and publish for macOS
  --ios            Build and publish for iOS
  --android        Build and publish for Android
  --windows        Build and publish for Windows
  --linux          Build and publish for Linux
  --all            Build and publish for all configured targets

OPTIONS:
  --local          Build locally instead of using build service
  --no-upload      Build and sign but don't upload to store
  --dry-run        Validate config and credentials without building
  --verbose        Show detailed build output
  --config <path>  Path to perry.toml (default: ./perry.toml)

AUTHENTICATION:
  perry auth apple       Set up App Store Connect credentials
  perry auth google      Set up Google Play credentials
  perry auth windows     Set up Windows signing certificate
  perry auth android     Set up Android keystore
    --create-keystore    Create a new signing keystore
  perry auth github      Authenticate with GitHub (for license)
  perry auth check       Verify all configured credentials

ACCOUNT:
  perry account          Show license status and usage
  perry account upgrade  Open upgrade page

VERSION MANAGEMENT:
  perry version bump <major|minor|patch>
                         Bump version in perry.toml
  perry version set <version>
                         Set specific version in perry.toml
```

---

## 13. Implementation Phases

### Phase 0: Foundation (weeks 1-2)
- [ ] `perry publish` subcommand scaffolding in existing CLI
- [ ] `perry.toml` parsing for publish configuration
- [ ] `~/.perry/` credentials storage with encryption
- [ ] `perry auth github` device flow for license registration
- [ ] License key generation API (simple Rust HTTP server)

### Phase 1: macOS (weeks 3-5)
- [ ] .app bundle generation from compiled binary
- [ ] Info.plist generation from perry.toml
- [ ] Icon generation (all sizes from single PNG, using `image` crate)
- [ ] `codesign` integration
- [ ] `notarytool` integration (or App Store Connect API for notarization)
- [ ] DMG creation via `hdiutil`
- [ ] Build server: accept tarball, compile, sign, return artifact
- [ ] WebSocket progress streaming
- [ ] `perry publish --macos` end-to-end working

### Phase 2: iOS (weeks 6-9)
- [ ] iOS .app bundle structure generation
- [ ] Asset catalog generation (investigate pure Rust vs actool)
- [ ] Provisioning profile embedding
- [ ] iOS code signing
- [ ] .ipa packaging
- [ ] App Store Connect upload API integration
- [ ] Guided provisioning profile creation flow in CLI
- [ ] `perry publish --ios` end-to-end working

### Phase 3: Android (weeks 10-12)
- [ ] AndroidManifest.xml generation
- [ ] APK/AAB structure creation
- [ ] Android icon generation (all density buckets, adaptive icons)
- [ ] apksigner integration
- [ ] Google Play Developer API upload integration
- [ ] `perry auth android --create-keystore`
- [ ] `perry publish --android` end-to-end working

### Phase 4: Windows & Linux (weeks 13-15)
- [ ] Windows EXE resource embedding (icon, version info)
- [ ] osslsigncode integration for Windows signing
- [ ] MSIX packaging (stretch goal)
- [ ] AppImage creation for Linux
- [ ] .deb packaging for Linux
- [ ] `perry publish --windows` and `--linux` working

### Phase 5: Polish & Launch (weeks 16-18)
- [ ] Multi-platform parallel builds
- [ ] `perry publish --all`
- [ ] Payment integration (Polar.sh / Lemon Squeezy)
- [ ] perry.dev website with docs and pricing
- [ ] Error message review and vibe-coder testing
- [ ] Security audit of credential handling
- [ ] Open source perry-build-server
- [ ] Launch announcement

---

## 14. Open Questions

1. **Asset catalog compilation without Xcode:** Can we generate `Assets.car` in pure Rust? If not, Xcode Command Line Tools (~2GB) are needed on the build server. This is fine for our server but blocks fully Xcode-free local builds.

2. **iOS Simulator builds:** Should `perry build --ios --simulator` work locally without any Apple credentials for testing? This would be great for the development loop.

3. **Provisioning profile automation:** Can we fully automate provisioning profile creation via the App Store Connect API, or do users need to manually create them? Investigate API capabilities.

4. **Windows cross-compilation:** How mature is CraneLift's PE/COFF output? May need LLVM backend for Windows targets initially.

5. **App Store metadata:** Should perry.toml include store listing metadata (description, screenshots, keywords) or is that out of scope for v1? Leaning toward out of scope — stores have their own dashboards for this.

6. **CI/CD integration:** Should we provide GitHub Actions / GitLab CI templates for `perry publish`? Probably yes, as a follow-up after launch.

7. **Version management:** Auto-increment build numbers for stores that require unique build numbers per upload (iOS especially)?

8. **Perry compiler version pinning:** Should the build server support building with specific Perry versions, or always use latest? Probably pin to project's `perry_version` in manifest.

---

## 15. Competitive Landscape

| Tool | What it does | Perry Publish advantage |
|------|-------------|----------------------|
| Fastlane | Ruby-based mobile deployment automation | No Ruby dependency, integrated with compiler, simpler config |
| Xcode | Full Apple IDE and build system | Not needed at all, massive footprint reduction |
| Android Studio | Full Android IDE and build system | Not needed at all |
| Expo EAS | React Native cloud builds | Perry produces true native binaries, not JS bundles |
| Capacitor | Web-to-native wrapper | Perry compiles to native, no WebView |
| Flutter | Cross-platform framework | TypeScript instead of Dart, single toolchain |
| Codemagic | Mobile CI/CD service | Perry owns the whole pipeline from source to store |

Perry Publish's unique position: **only tool that owns the entire pipeline from TypeScript source code to app store listing, with no IDE dependencies.**

---

*This is a living document. Last updated: February 25, 2026.*
