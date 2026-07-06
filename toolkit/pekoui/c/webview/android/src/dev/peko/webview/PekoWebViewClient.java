package dev.peko.webview;

import android.content.res.AssetManager;
import android.net.Uri;
import android.webkit.WebResourceRequest;
import android.webkit.WebResourceResponse;
import android.webkit.WebView;
import android.webkit.WebViewClient;

import org.json.JSONObject;

import java.io.IOException;
import java.io.InputStream;

/**
 * Forwards WebView navigation lifecycle events to the native library and serves
 * the app's bundled assets straight from the APK.
 *
 * The app is served from a loopback URL whose path begins with /_assets/. On
 * Android the WebView's network stack refuses loopback loads whenever the device
 * reports no connectivity (ERR_INTERNET_DISCONNECTED), so instead of relying on
 * the loopback HTTP server the requests are intercepted and answered from the
 * AssetManager. This bypasses the network entirely and works fully offline.
 */
public final class PekoWebViewClient extends WebViewClient {
    private static final String ASSET_PREFIX = "/_assets/";

    @Override
    public void onPageStarted(WebView view, String url, android.graphics.Bitmap favicon) {
        PekoWebViewBridge.nativeOnPageStarted(url);
    }

    @Override
    public boolean shouldOverrideUrlLoading(WebView view, WebResourceRequest request) {
        return handleUrl(request.getUrl().toString());
    }

    @Override
    @SuppressWarnings("deprecation")
    public boolean shouldOverrideUrlLoading(WebView view, String url) {
        return handleUrl(url);
    }

    /**
     * Intercepts a link the WebView is about to navigate to. A custom-scheme
     * link (the app's own deep-link scheme) is an in-app navigation: its route
     * is delivered to the router and the WebView does not navigate to it, which
     * it cannot load and which would blank the page. Loopback http(s) links
     * (the app's own pages and assets) load normally.
     */
    private static boolean handleUrl(String url) {
        if (url == null) {
            return false;
        }
        if (url.startsWith("http://") || url.startsWith("https://")) {
            return false;
        }

        int marker = url.indexOf("://");
        String route = (marker >= 0) ? url.substring(marker + 3) : url;
        if (!route.startsWith("/")) {
            route = "/" + route;
        }
        PekoWebViewBridge.eval(
                "window.__peko_deeplink && window.__peko_deeplink(" + JSONObject.quote(route) + ")");
        return true;
    }

    @Override
    public void onPageFinished(WebView view, String url) {
        PekoWebViewBridge.nativeOnPageFinished(url);
    }

    @Override
    public WebResourceResponse shouldInterceptRequest(WebView view, WebResourceRequest request) {
        Uri uri = request.getUrl();
        String path = uri.getPath();
        if (path == null || !path.startsWith(ASSET_PREFIX)) {
            return null;
        }

        String name = path.substring(ASSET_PREFIX.length());
        if (name.isEmpty()) {
            name = "index.html";
        }

        AssetManager assets = view.getContext().getAssets();
        InputStream stream = openAsset(assets, name);
        if (stream == null && !name.contains(".")) {
            // History-mode route (no file extension): serve the SPA shell.
            name = "index.html";
            stream = openAsset(assets, name);
        }
        if (stream == null) {
            return null;
        }
        return new WebResourceResponse(guessMime(name), null, stream);
    }

    private static InputStream openAsset(AssetManager assets, String name) {
        try {
            return assets.open(name);
        } catch (IOException e) {
            return null;
        }
    }

    private static String guessMime(String name) {
        String lower = name.toLowerCase();
        if (lower.endsWith(".html") || lower.endsWith(".htm")) return "text/html";
        if (lower.endsWith(".js") || lower.endsWith(".mjs")) return "text/javascript";
        if (lower.endsWith(".css")) return "text/css";
        if (lower.endsWith(".json")) return "application/json";
        if (lower.endsWith(".svg")) return "image/svg+xml";
        if (lower.endsWith(".png")) return "image/png";
        if (lower.endsWith(".jpg") || lower.endsWith(".jpeg")) return "image/jpeg";
        if (lower.endsWith(".gif")) return "image/gif";
        if (lower.endsWith(".webp")) return "image/webp";
        if (lower.endsWith(".ico")) return "image/x-icon";
        if (lower.endsWith(".woff2")) return "font/woff2";
        if (lower.endsWith(".woff")) return "font/woff";
        if (lower.endsWith(".ttf")) return "font/ttf";
        if (lower.endsWith(".wasm")) return "application/wasm";
        if (lower.endsWith(".txt")) return "text/plain";
        return "application/octet-stream";
    }
}
