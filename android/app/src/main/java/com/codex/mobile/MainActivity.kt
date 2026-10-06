package com.codex.mobile

import android.annotation.SuppressLint
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.ComponentActivity
import androidx.webkit.WebViewClientCompat

/**
 * Main entry: WebView hosting the Codex agent UI (from assets/ui/),
 * bridged to the native codex app-server via [CodexBridge].
 */
class MainActivity : ComponentActivity() {

    private lateinit var webView: WebView
    private lateinit var bridge: CodexBridge

    @SuppressLint("SetJavaScriptEnabled", "AddJavascriptInterface")
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        webView = WebView(this).apply {
            settings.javaScriptEnabled = true
            settings.domStorageEnabled = true
            // Keep navigation inside the WebView.
            webViewClient = WebViewClientCompat()
        }

        bridge = CodexBridge(this, webView)
        webView.addJavascriptInterface(bridge, "CodexNative")

        setContentView(webView)
        // UI bundle lives in src/main/assets/ui/ (exported from the web prototype).
        webView.loadUrl("file:///android_asset/ui/index.html")

        // Ensure the native codex binary is extracted and ready.
        bridge.ensureServerReady()
    }

    override fun onDestroy() {
        bridge.shutdown()
        webView.destroy()
        super.onDestroy()
    }
}
