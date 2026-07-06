package dev.peko.webview;

import android.app.NativeActivity;
import android.content.Intent;
import android.net.Uri;
import org.json.JSONObject;

/**
 * NativeActivity subclass that delivers a deep-link URL opened while the app is
 * already running.
 *
 * The activity is declared singleTop, so a new registered-scheme URL arrives
 * here through onNewIntent rather than starting a second activity. The route
 * that follows the scheme is pushed straight into the loaded page, which the
 * client SDK applies to the router. A cold launch is handled natively from the
 * launch intent instead, so this only covers the running case.
 */
public final class PekoNativeActivity extends NativeActivity {
    @Override
    public void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        if (intent == null) {
            return;
        }
        Uri data = intent.getData();
        if (data == null) {
            return;
        }

        String url = data.toString();
        int marker = url.indexOf("://");
        String route = (marker >= 0) ? url.substring(marker + 3) : url;
        if (!route.startsWith("/")) {
            route = "/" + route;
        }

        PekoWebViewBridge.eval(
                "window.__peko_deeplink && window.__peko_deeplink(" + JSONObject.quote(route) + ")");
    }
}
