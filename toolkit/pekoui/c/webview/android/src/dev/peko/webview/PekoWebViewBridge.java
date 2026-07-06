package dev.peko.webview;

import android.app.Activity;
import android.graphics.Color;
import android.graphics.PixelFormat;
import android.view.Gravity;
import android.view.WindowManager;
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

    /**
     * Creates the WebView and shows it in its own window over the activity.
     *
     * A Peko app is a NativeActivity, which takes the activity window surface
     * for native rendering. The Java view hierarchy attached to that window is
     * never drawn, so a WebView added to the content view lays out but stays
     * black. The WebView is added to the window manager as a panel window
     * anchored to the activity, which gives it a compositor surface of its own.
     */
    public static void create(final Activity activity) {
        sActivity = activity;
        activity.runOnUiThread(new Runnable() {
            @Override
            public void run() {
                final WebView view = new WebView(activity);
                WebSettings settings = view.getSettings();
                settings.setJavaScriptEnabled(true);
                settings.setDomStorageEnabled(true);
                view.setWebViewClient(new PekoWebViewClient());
                view.addJavascriptInterface(new PekoWebViewBridge(), "__peko_native__");
                // Where the page paints no color, the window shows through to
                // black rather than the default white.
                view.setBackgroundColor(Color.BLACK);

                // Loading content works before the view is on screen, so the
                // native side can drive it right away.
                sWebView = view;
                nativeOnReady();

                // Adding the panel window needs the host window token, which is
                // set once the decor view is attached. Attaching this early in
                // the activity lifecycle, the token can still be null, so the
                // add is deferred until it is available.
                android.view.View decor = activity.getWindow().getDecorView();
                decor.post(new WindowAttacher(activity, view, decor));
            }
        });
    }

    /**
     * Adds the WebView to the window manager as an activity panel window once
     * the host window token is available, re-posting itself while it is not.
     */
    private static final class WindowAttacher implements Runnable {
        private final Activity activity;
        private final WebView view;
        private final android.view.View decor;

        WindowAttacher(Activity activity, WebView view, android.view.View decor) {
            this.activity = activity;
            this.view = view;
            this.decor = decor;
        }

        @Override
        public void run() {
            if (decor.getWindowToken() == null) {
                decor.post(this);
                return;
            }
            // FLAG_LAYOUT_IN_SCREEN and FLAG_LAYOUT_NO_LIMITS extend the window
            // past the status bar and navigation bar so the WebView fills the
            // whole display and the system bars overlay it.
            WindowManager.LayoutParams params = new WindowManager.LayoutParams(
                    WindowManager.LayoutParams.MATCH_PARENT,
                    WindowManager.LayoutParams.MATCH_PARENT,
                    WindowManager.LayoutParams.TYPE_APPLICATION_PANEL,
                    WindowManager.LayoutParams.FLAG_NOT_TOUCH_MODAL
                            | WindowManager.LayoutParams.FLAG_LAYOUT_IN_SCREEN
                            | WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                    PixelFormat.OPAQUE);
            params.token = decor.getWindowToken();
            params.gravity = Gravity.TOP | Gravity.START;
            // Reach into the display cutout region on notched screens.
            if (android.os.Build.VERSION.SDK_INT >= 28) {
                params.layoutInDisplayCutoutMode =
                        WindowManager.LayoutParams.LAYOUT_IN_DISPLAY_CUTOUT_MODE_SHORT_EDGES;
            }
            activity.getWindowManager().addView(view, params);
        }
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
