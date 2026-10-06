# Codex Mobile — Android Shell

Native Android wrapper for the Codex coding agent.

## Architecture

```
┌─────────────────────────────────────────┐
│  WebView UI (assets/ui/)                │  ← Agent activity interface
│  - Task list, timeline, terminal, diffs │     (light warm theme)
│  - Calls CodexNative.* via JS bridge   │
├─────────────────────────────────────────┤
│  CodexBridge (Kotlin)                   │  ← JavascriptInterface
│  - Manages codex subprocess lifecycle   │
│  - stdin/stdout JSON message pump       │
├─────────────────────────────────────────┤
│  libcodex.so (jniLibs/arm64-v8a/)       │  ← Rust codex CLI binary
│  - Built by .github/workflows/         │     (CI: build-android.yml)
│    build-android.yml                    │
└─────────────────────────────────────────┘
```

## Building

Requires Android SDK + JDK 17. The `libcodex.so` is produced by CI
(`build-android.yml` → `dist/jniLibs/arm64-v8a/libcodex.so`).

```bash
# Copy the CI-built libcodex.so:
cp /path/to/dist/jniLibs/arm64-v8a/libcodex.so \
   app/src/main/jniLibs/arm64-v8a/

# Build debug APK:
./gradlew assembleDebug
```

## UI

The WebView loads `assets/ui/index.html`. The UI prototype lives as a web
artifact (`codex-mobile-ui`); export it and place the files under
`app/src/main/assets/ui/` before building.

JS ↔ Native protocol:
- UI → Native: `CodexNative.postCommand(json)` — JSON task commands
- Native → UI: `window.__codexOnEvent(jsonLine)` — JSON events from app-server
- UI → Native: `CodexNative.getStatus()` — `{"running": bool}`
