package dev.peko.webview;

import android.webkit.WebView;
import android.webkit.WebViewClient;

/**
 * Forwards WebView navigation lifecycle events to the native library. Page
 * start re-injects the native init scripts; page finish drives on_load and the
 * navigation callbacks.
 */
public final class PekoWebViewClient extends WebViewClient {
    @Override
    public void onPageStarted(WebView view, String url, android.graphics.Bitmap favicon) {
        PekoWebViewBridge.nativeOnPageStarted(url);
    }

    @Override
    public void onPageFinished(WebView view, String url) {
        PekoWebViewBridge.nativeOnPageFinished(url);
    }
}
