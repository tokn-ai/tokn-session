package dev.tokn.session.viewer

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts

class MainActivity : TauriActivity() {
  private var viewerWebView: WebView? = null
  private val localNetworkPermission = registerForActivityResult(
    ActivityResultContracts.RequestPermission(),
  ) { granted ->
    // A LAN development page may have failed before the system prompt finished.
    // Release assets are bundled and keep their current session when permission changes.
    if (granted && BuildConfig.DEBUG) viewerWebView?.reload()
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    viewerWebView = webView
    if (
      Build.VERSION.SDK_INT >= 37 &&
      checkSelfPermission(Manifest.permission.ACCESS_LOCAL_NETWORK) != PackageManager.PERMISSION_GRANTED
    ) {
      // Direct peer connections need LAN access; public Hub relay still works if denied.
      localNetworkPermission.launch(Manifest.permission.ACCESS_LOCAL_NETWORK)
    }
  }
}
