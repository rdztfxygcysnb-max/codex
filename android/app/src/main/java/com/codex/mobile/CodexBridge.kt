package com.codex.mobile

import android.content.Context
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.JavascriptInterface
import android.webkit.WebView
import java.io.File
import java.util.concurrent.Executors

/**
 * Bridge between the WebView UI and the native codex app-server.
 *
 * The codex CLI is shipped as jniLibs/arm64-v8a/libcodex.so (built by CI).
 * On first run it is copied to the app's private files dir, marked executable,
 * and spawned as a subprocess. The UI sends JSON commands over stdin and
 * receives JSON events over stdout.
 *
 * Exposed to JavaScript as `CodexNative`.
 */
class CodexBridge(
    private val context: Context,
    private val webView: WebView,
) {
    private val ioExecutor = Executors.newSingleThreadExecutor()
    private val mainHandler = Handler(Looper.getMainLooper())
    private var process: Process? = null

    companion object {
        private const val TAG = "CodexBridge"
        private const val BINARY_NAME = "libcodex.so"
    }

    /** Copy the bundled binary to private storage and make it executable. */
    fun ensureServerReady() {
        ioExecutor.execute {
            try {
                val binFile = File(context.filesDir, "codex")
                if (!binFile.exists()) {
                    context.assets.open("jniLibs/arm64-v8a/$BINARY_NAME").use { input ->
                        binFile.outputStream().use { output -> input.copyTo(output) }
                    }
                    // Note: assets are compressed; jniLibs entries must be copied
                    // via the APK's lib path instead on some builds. Fallback below.
                }
                if (!binFile.exists()) {
                    // Fallback: load from the native library dir.
                    val nativeLib = File(context.applicationInfo.nativeLibraryDir, BINARY_NAME)
                    if (nativeLib.exists()) {
                        nativeLib.copyTo(binFile, overwrite = true)
                    }
                }
                if (binFile.exists()) {
                    binFile.setExecutable(true)
                    Log.i(TAG, "codex binary ready at ${binFile.absolutePath}")
                } else {
                    Log.e(TAG, "codex binary not found in APK")
                }
            } catch (e: Exception) {
                Log.e(TAG, "failed to prepare codex binary", e)
            }
        }
    }

    /** Send a JSON command to the app-server's stdin. Called from JS. */
    @JavascriptInterface
    fun postCommand(json: String) {
        ioExecutor.execute {
            try {
                val proc = process ?: startServer()
                proc.outputStream.bufferedWriter().apply {
                    write(json)
                    newLine()
                    flush()
                }
            } catch (e: Exception) {
                Log.e(TAG, "postCommand failed", e)
                emitToUi("{\"type\":\"error\",\"message\":\"${e.message}\"}")
            }
        }
    }

    /** Current server status for the UI. Called from JS. */
    @JavascriptInterface
    fun getStatus(): String {
        val running = process?.isAlive == true
        return "{\"running\":$running}"
    }

    private fun startServer(): Process {
        val binFile = File(context.filesDir, "codex")
        require(binFile.exists()) { "codex binary not prepared" }
        val proc = ProcessBuilder(binFile.absolutePath, "app-server")
            .redirectErrorStream(false)
            .start()
        process = proc
        // Pump stdout -> WebView as JSON events.
        Thread {
            try {
                proc.inputStream.bufferedReader().forEachLine { line ->
                    emitToUi(line)
                }
            } catch (e: Exception) {
                Log.e(TAG, "stdout pump ended", e)
            }
        }.apply { isDaemon = true; start() }
        // Pump stderr -> logcat.
        Thread {
            try {
                proc.errorStream.bufferedReader().forEachLine { line ->
                    Log.w(TAG, "codex stderr: $line")
                }
            } catch (_: Exception) { }
        }.apply { isDaemon = true; start() }
        return proc
    }

    private fun emitToUi(jsonLine: String) {
        // Escape for JS string literal.
        val escaped = jsonLine
            .replace("\\", "\\\\")
            .replace("'", "\\'")
            .replace("\n", "\\n")
        mainHandler.post {
            webView.evaluateJavascript(
                "window.__codexOnEvent && window.__codexOnEvent('$escaped')",
                null,
            )
        }
    }

    fun shutdown() {
        try {
            process?.destroy()
        } catch (_: Exception) { }
        ioExecutor.shutdownNow()
    }
}
