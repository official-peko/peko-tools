package dev.peko.webview;

import android.app.Activity;
import android.view.ViewGroup;
import android.webkit.JavascriptInterface;
import android.webkit.WebView;
import android.webkit.WebSettings;

/**
 * The prebuilt Java side of std::webview on Android.
 *
 * A Peko app is a NativeActivity with no application DEX of its own, so this
 * helper ships precompiled with the standard library. The native library
 * (libPekoApp.so) drives it through the static entry points, and it calls back
 * into the native library through the native methods, which resolve to the
 * JNI-exported functions in webview_android.c.
 *
 * Every WebView operation runs on the UI thread. The native side runs on the
 * NativeActivity thread, so the entry points hop to the UI thread through
 * Activity.runOnUiThread.
 */
public final class PekoWebViewBridge {
    private static Activity sActivity;
    private static WebView sWebView;

    private PekoWebViewBridge() {
    }

    /** Creates the WebView and adds it to the activity's content view. */
    public static void create(final Activity activity) {
        sActivity = activity;
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                WebView view = new WebView(activity);
                WebSettings settings = view.getSettings();
                settings.setJavaScriptEnabled(true);
                settings.setDomStorageEnabled(true);
                view.setWebViewClient(new PekoWebViewClient());
                view.addJavascriptInterface(new PekoWebViewBridge(), "__peko_native__");
                activity.addContentView(view, new ViewGroup.LayoutParams(
                        ViewGroup.LayoutParams.MATCH_PARENT,
                        ViewGroup.LayoutParams.MATCH_PARENT));
                sWebView = view;
                nativeOnReady();
            }
        });
    }

    /** Loads a URL. */
    public static void navigate(final String url) {
        runOnUi(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    sWebView.loadUrl(url);
                }
            }
        });
    }

    /** Loads an HTML document directly. */
    public static void loadHtml(final String html) {
        runOnUi(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    sWebView.loadDataWithBaseURL(null, html, "text/html", "utf-8", null);
                }
            }
        });
    }

    /** Evaluates JavaScript in the current page. */
    public static void eval(final String js) {
        runOnUi(new Runnable() {
            @Override
            public void run() {
                if (sWebView != null) {
                    sWebView.evaluateJavascript(js, null);
                }
            }
        });
    }

    private static void runOnUi(Runnable action) {
        if (sActivity != null) {
            sActivity.runOnUiThread(action);
        }
    }

    /**
     * Called by bound JavaScript through window.__peko_native__.postMessage.
     * Forwards the RPC request to the native dispatcher.
     */
    @JavascriptInterface
    public void postMessage(String message) {
        nativeOnMessage(message);
    }

    // Native entry points, exported from webview_android.c.
    static native void nativeOnReady();
    static native void nativeOnMessage(String message);
    static native void nativeOnPageStarted(String url);
    static native void nativeOnPageFinished(String url);
}
