/*
 * MIT License
 *
 * Copyright (c) 2017 Serge Zaitsev
 * Copyright (c) 2022 Steffen André Langnes
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the "Software"), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in
 * all copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

/* Derived from webview/webview (https://github.com/webview/webview), version
 * 0.10.0, and substantially modified by Peko UI Technologies LLC.
 * Modifications copyright (c) 2026 Peko UI Technologies LLC, released under the
 * same MIT terms above.
 *
 * The modifications are extensive and this file no longer tracks upstream. They
 * include: per-OS source gating so iOS and Android fall through to the backends
 * below; frameless-window support (NCCALCSIZE/NCHITTEST on Windows, titlebar
 * height control on macOS); composition-based transparent hosting with manual
 * input forwarding on Windows; native window-control handling; and the Peko
 * bridge integration points.
 *
 * iOS and Android implement the webview in webview_ios.m and webview_android.c,
 * so this file supplies only the declarations there. Defining WEBVIEW_HEADER
 * skips the whole implementation block below, which would otherwise select the
 * AppKit backend on iOS or the GTK backend on Android, neither of which exists
 * on those platforms. */
#if defined(__ANDROID__)
#define WEBVIEW_HEADER
#elif defined(__APPLE__)
#include <TargetConditionals.h>
#if TARGET_OS_IPHONE
#define WEBVIEW_HEADER
#endif
#endif

#ifndef WEBVIEW_API
#define WEBVIEW_API extern
#endif

#ifndef WEBVIEW_VERSION_MAJOR
// The current library major version.
#define WEBVIEW_VERSION_MAJOR 0
#endif

#ifndef WEBVIEW_VERSION_MINOR
// The current library minor version.
#define WEBVIEW_VERSION_MINOR 10
#endif

#ifndef WEBVIEW_VERSION_PATCH
// The current library patch version.
#define WEBVIEW_VERSION_PATCH 0
#endif

#ifndef WEBVIEW_VERSION_PRE_RELEASE
// SemVer 2.0.0 pre-release labels prefixed with "-".
#define WEBVIEW_VERSION_PRE_RELEASE ""
#endif

#ifndef WEBVIEW_VERSION_BUILD_METADATA
// SemVer 2.0.0 build metadata prefixed with "+".
#define WEBVIEW_VERSION_BUILD_METADATA ""
#endif

// Utility macro for stringifying a macro argument.
#define WEBVIEW_STRINGIFY(x) #x

// Utility macro for stringifying the result of a macro argument expansion.
#define WEBVIEW_EXPAND_AND_STRINGIFY(x) WEBVIEW_STRINGIFY(x)

// SemVer 2.0.0 version number in MAJOR.MINOR.PATCH format.
#define WEBVIEW_VERSION_NUMBER                                                 \
  WEBVIEW_EXPAND_AND_STRINGIFY(WEBVIEW_VERSION_MAJOR)                          \
  "." WEBVIEW_EXPAND_AND_STRINGIFY(                                            \
      WEBVIEW_VERSION_MINOR) "." WEBVIEW_EXPAND_AND_STRINGIFY(WEBVIEW_VERSION_PATCH)

// Holds the elements of a MAJOR.MINOR.PATCH version number.
typedef struct {
  // Major version.
  unsigned int major;
  // Minor version.
  unsigned int minor;
  // Patch version.
  unsigned int patch;
} webview_version_t;

// Holds the library's version information.
typedef struct {
  // The elements of the version number.
  webview_version_t version;
  // SemVer 2.0.0 version number in MAJOR.MINOR.PATCH format.
  char version_number[32];
  // SemVer 2.0.0 pre-release labels prefixed with "-" if specified, otherwise
  // an empty string.
  char pre_release[48];
  // SemVer 2.0.0 build metadata prefixed with "+", otherwise an empty string.
  char build_metadata[48];
} webview_version_info_t;

#ifdef __cplusplus
extern "C" {
#endif

typedef void *webview_t;

// Creates a new webview instance. If debug is non-zero - developer tools will
// be enabled (if the platform supports them). Window parameter can be a
// pointer to the native window handle. If it's non-null - then child WebView
// is embedded into the given parent window. Otherwise a new window is created.
// Depending on the platform, a GtkWindow, NSWindow or HWND pointer can be
// passed here. Returns null on failure. Creation can fail for various reasons
// such as when required runtime dependencies are missing or when window creation
// fails.
WEBVIEW_API webview_t webview_create(int debug, void *window);

// Destroys a webview and closes the native window.
WEBVIEW_API void webview_destroy(webview_t w);

// Runs the main loop until it's terminated. After this function exits - you
// must destroy the webview.
WEBVIEW_API void webview_run(webview_t w);

// Stops the main loop. It is safe to call this function from another other
// background thread.
WEBVIEW_API void webview_terminate(webview_t w);

// Posts a function to be executed on the main thread. You normally do not need
// to call this function, unless you want to tweak the native window.
WEBVIEW_API void
webview_dispatch(webview_t w, void (*fn)(webview_t w, void *arg), void *arg);

// Returns a native window handle pointer. When using GTK backend the pointer
// is GtkWindow pointer, when using Cocoa backend the pointer is NSWindow
// pointer, when using Win32 backend the pointer is HWND pointer.
WEBVIEW_API void *webview_get_window(webview_t w);

// Returns the native browser controller pointer. On the Win32 backend this is
// an ICoreWebView2Controller pointer, from which the ICoreWebView2 is reached
// for capture. Returns null on the GTK and Cocoa backends.
WEBVIEW_API void *webview_get_controller(webview_t w);

// Updates the title of the native window. Must be called from the UI thread.
WEBVIEW_API void webview_set_title(webview_t w, const char *title);

// Window size hints
#define WEBVIEW_HINT_NONE 0  // Width and height are default size
#define WEBVIEW_HINT_MIN 1   // Width and height are minimum bounds
#define WEBVIEW_HINT_MAX 2   // Width and height are maximum bounds
#define WEBVIEW_HINT_FIXED 3 // Window size can not be changed by a user
// Updates native window size. See WEBVIEW_HINT constants.
WEBVIEW_API void webview_set_size(webview_t w, int width, int height,
                                  int hints);

// Navigates webview to the given URL. URL may be a properly encoded data URI.
// Examples:
// webview_navigate(w, "https://github.com/webview/webview");
// webview_navigate(w, "data:text/html,%3Ch1%3EHello%3C%2Fh1%3E");
// webview_navigate(w, "data:text/html;base64,PGgxPkhlbGxvPC9oMT4=");
WEBVIEW_API void webview_navigate(webview_t w, const char *url);

// Set webview HTML directly.
// Example: webview_set_html(w, "<h1>Hello</h1>");
WEBVIEW_API void webview_set_html(webview_t w, const char *html);

// Injects JavaScript code at the initialization of the new page. Every time
// the webview will open a the new page - this initialization code will be
// executed. It is guaranteed that code is executed before window.onload.
WEBVIEW_API void webview_init(webview_t w, const char *js);

// Evaluates arbitrary JavaScript code. Evaluation happens asynchronously, also
// the result of the expression is ignored. Use RPC bindings if you want to
// receive notifications about the results of the evaluation.
WEBVIEW_API void webview_eval(webview_t w, const char *js);

// Binds a native C callback so that it will appear under the given name as a
// global JavaScript function. Internally it uses webview_init(). Callback
// receives a request string and a user-provided argument pointer. Request
// string is a JSON array of all the arguments passed to the JavaScript
// function.
WEBVIEW_API void webview_bind(webview_t w, const char *name,
                              void (*fn)(const char *seq, const char *req,
                                         void *arg),
                              void *arg);

// Removes a native C callback that was previously set by webview_bind.
WEBVIEW_API void webview_unbind(webview_t w, const char *name);

// Allows to return a value from the native binding. Original request pointer
// must be provided to help internal RPC engine match requests with responses.
// If status is zero - result is expected to be a valid JSON result value.
// If status is not zero - result is an error JSON object.
WEBVIEW_API void webview_return(webview_t w, const char *seq, int status,
                                const char *result);

// Get the library's version information.
// @since 0.10
WEBVIEW_API const webview_version_info_t *webview_version();

#ifdef __cplusplus
}

#ifndef WEBVIEW_HEADER

#if !defined(WEBVIEW_GTK) && !defined(WEBVIEW_COCOA) && !defined(WEBVIEW_EDGE)
#if defined(__APPLE__)
#define WEBVIEW_COCOA
#elif defined(__unix__)
#define WEBVIEW_GTK
#elif defined(_WIN32)
#define WEBVIEW_EDGE
#else
#error "please, specify webview backend"
#endif
#endif

#ifndef WEBVIEW_DEPRECATED
#if __cplusplus >= 201402L
#define WEBVIEW_DEPRECATED(reason) [[deprecated(reason)]]
#elif defined(_MSC_VER)
#define WEBVIEW_DEPRECATED(reason) __declspec(deprecated(reason))
#else
#define WEBVIEW_DEPRECATED(reason) __attribute__((deprecated(reason)))
#endif
#endif

#ifndef WEBVIEW_DEPRECATED_PRIVATE
#define WEBVIEW_DEPRECATED_PRIVATE                                             \
  WEBVIEW_DEPRECATED("Private API should not be used")
#endif

#include <array>
#include <atomic>
#include <functional>
#include <future>
#include <map>
#include <string>
#include <utility>
#include <vector>

#include <cstring>

namespace webview {

using dispatch_fn_t = std::function<void()>;

namespace detail {

// The library's version information.
constexpr const webview_version_info_t library_version_info{
    {WEBVIEW_VERSION_MAJOR, WEBVIEW_VERSION_MINOR, WEBVIEW_VERSION_PATCH},
    WEBVIEW_VERSION_NUMBER,
    WEBVIEW_VERSION_PRE_RELEASE,
    WEBVIEW_VERSION_BUILD_METADATA};

inline int json_parse_c(const char *s, size_t sz, const char *key, size_t keysz,
                        const char **value, size_t *valuesz) {
  enum {
    JSON_STATE_VALUE,
    JSON_STATE_LITERAL,
    JSON_STATE_STRING,
    JSON_STATE_ESCAPE,
    JSON_STATE_UTF8
  } state = JSON_STATE_VALUE;
  const char *k = nullptr;
  int index = 1;
  int depth = 0;
  int utf8_bytes = 0;

  *value = nullptr;
  *valuesz = 0;

  if (key == nullptr) {
    index = static_cast<decltype(index)>(keysz);
    if (index < 0) {
      return -1;
    }
    keysz = 0;
  }

  for (; sz > 0; s++, sz--) {
    enum {
      JSON_ACTION_NONE,
      JSON_ACTION_START,
      JSON_ACTION_END,
      JSON_ACTION_START_STRUCT,
      JSON_ACTION_END_STRUCT
    } action = JSON_ACTION_NONE;
    auto c = static_cast<unsigned char>(*s);
    switch (state) {
    case JSON_STATE_VALUE:
      if (c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == ',' ||
          c == ':') {
        continue;
      } else if (c == '"') {
        action = JSON_ACTION_START;
        state = JSON_STATE_STRING;
      } else if (c == '{' || c == '[') {
        action = JSON_ACTION_START_STRUCT;
      } else if (c == '}' || c == ']') {
        action = JSON_ACTION_END_STRUCT;
      } else if (c == 't' || c == 'f' || c == 'n' || c == '-' ||
                 (c >= '0' && c <= '9')) {
        action = JSON_ACTION_START;
        state = JSON_STATE_LITERAL;
      } else {
        return -1;
      }
      break;
    case JSON_STATE_LITERAL:
      if (c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == ',' ||
          c == ']' || c == '}' || c == ':') {
        state = JSON_STATE_VALUE;
        s--;
        sz++;
        action = JSON_ACTION_END;
      } else if (c < 32 || c > 126) {
        return -1;
      } // fallthrough
    case JSON_STATE_STRING:
      if (c < 32 || (c > 126 && c < 192)) {
        return -1;
      } else if (c == '"') {
        action = JSON_ACTION_END;
        state = JSON_STATE_VALUE;
      } else if (c == '\\') {
        state = JSON_STATE_ESCAPE;
      } else if (c >= 192 && c < 224) {
        utf8_bytes = 1;
        state = JSON_STATE_UTF8;
      } else if (c >= 224 && c < 240) {
        utf8_bytes = 2;
        state = JSON_STATE_UTF8;
      } else if (c >= 240 && c < 247) {
        utf8_bytes = 3;
        state = JSON_STATE_UTF8;
      } else if (c >= 128 && c < 192) {
        return -1;
      }
      break;
    case JSON_STATE_ESCAPE:
      if (c == '"' || c == '\\' || c == '/' || c == 'b' || c == 'f' ||
          c == 'n' || c == 'r' || c == 't' || c == 'u') {
        state = JSON_STATE_STRING;
      } else {
        return -1;
      }
      break;
    case JSON_STATE_UTF8:
      if (c < 128 || c > 191) {
        return -1;
      }
      utf8_bytes--;
      if (utf8_bytes == 0) {
        state = JSON_STATE_STRING;
      }
      break;
    default:
      return -1;
    }

    if (action == JSON_ACTION_END_STRUCT) {
      depth--;
    }

    if (depth == 1) {
      if (action == JSON_ACTION_START || action == JSON_ACTION_START_STRUCT) {
        if (index == 0) {
          *value = s;
        } else if (keysz > 0 && index == 1) {
          k = s;
        } else {
          index--;
        }
      } else if (action == JSON_ACTION_END ||
                 action == JSON_ACTION_END_STRUCT) {
        if (*value != nullptr && index == 0) {
          *valuesz = (size_t)(s + 1 - *value);
          return 0;
        } else if (keysz > 0 && k != nullptr) {
          if (keysz == (size_t)(s - k - 1) && memcmp(key, k + 1, keysz) == 0) {
            index = 0;
          } else {
            index = 2;
          }
          k = nullptr;
        }
      }
    }

    if (action == JSON_ACTION_START_STRUCT) {
      depth++;
    }
  }
  return -1;
}

inline std::string json_escape(const std::string &s) {
  // TODO: implement
  return '"' + s + '"';
}

inline int json_unescape(const char *s, size_t n, char *out) {
  int r = 0;
  if (*s++ != '"') {
    return -1;
  }
  while (n > 2) {
    char c = *s;
    if (c == '\\') {
      s++;
      n--;
      switch (*s) {
      case 'b':
        c = '\b';
        break;
      case 'f':
        c = '\f';
        break;
      case 'n':
        c = '\n';
        break;
      case 'r':
        c = '\r';
        break;
      case 't':
        c = '\t';
        break;
      case '\\':
        c = '\\';
        break;
      case '/':
        c = '/';
        break;
      case '\"':
        c = '\"';
        break;
      default: // TODO: support unicode decoding
        return -1;
      }
    }
    if (out != nullptr) {
      *out++ = c;
    }
    s++;
    n--;
    r++;
  }
  if (*s != '"') {
    return -1;
  }
  if (out != nullptr) {
    *out = '\0';
  }
  return r;
}

inline std::string json_parse(const std::string &s, const std::string &key,
                              const int index) {
  const char *value;
  size_t value_sz;
  if (key.empty()) {
    json_parse_c(s.c_str(), s.length(), nullptr, index, &value, &value_sz);
  } else {
    json_parse_c(s.c_str(), s.length(), key.c_str(), key.length(), &value,
                 &value_sz);
  }
  if (value != nullptr) {
    if (value[0] != '"') {
      return {value, value_sz};
    }
    int n = json_unescape(value, value_sz, nullptr);
    if (n > 0) {
      char *decoded = new char[n + 1];
      json_unescape(value, value_sz, decoded);
      std::string result(decoded, n);
      delete[] decoded;
      return result;
    }
  }
  return "";
}

} // namespace detail

WEBVIEW_DEPRECATED_PRIVATE
inline int json_parse_c(const char *s, size_t sz, const char *key, size_t keysz,
                        const char **value, size_t *valuesz) {
  return detail::json_parse_c(s, sz, key, keysz, value, valuesz);
}

WEBVIEW_DEPRECATED_PRIVATE
inline std::string json_escape(const std::string &s) {
  return detail::json_escape(s);
}

WEBVIEW_DEPRECATED_PRIVATE
inline int json_unescape(const char *s, size_t n, char *out) {
  return detail::json_unescape(s, n, out);
}

WEBVIEW_DEPRECATED_PRIVATE
inline std::string json_parse(const std::string &s, const std::string &key,
                              const int index) {
  return detail::json_parse(s, key, index);
}

} // namespace webview

#if defined(WEBVIEW_GTK)
//
// ====================================================================
//
// This implementation uses webkit2gtk backend. It requires gtk+3.0 and
// webkit2gtk-4.0 libraries. Proper compiler flags can be retrieved via:
//
//   pkg-config --cflags --libs gtk+-3.0 webkit2gtk-4.0
//
// ====================================================================
//
#include <JavaScriptCore/JavaScript.h>
#include <gtk/gtk.h>
#include <webkit2/webkit2.h>

namespace webview {
namespace detail {

class gtk_webkit_engine {
public:
  gtk_webkit_engine(bool debug, void *window)
      : m_window(static_cast<GtkWidget *>(window)) {
    if (gtk_init_check(nullptr, nullptr) == FALSE) {
      return;
    }
    m_window = static_cast<GtkWidget *>(window);
    if (m_window == nullptr) {
      m_window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    }
    g_signal_connect(G_OBJECT(m_window), "destroy",
                     G_CALLBACK(+[](GtkWidget *, gpointer arg) {
                       static_cast<gtk_webkit_engine *>(arg)->terminate();
                     }),
                     this);
    // Initialize webview widget
    m_webview = webkit_web_view_new();
    WebKitUserContentManager *manager =
        webkit_web_view_get_user_content_manager(WEBKIT_WEB_VIEW(m_webview));
    g_signal_connect(manager, "script-message-received::external",
                     G_CALLBACK(+[](WebKitUserContentManager *,
                                    WebKitJavascriptResult *r, gpointer arg) {
                       auto *w = static_cast<gtk_webkit_engine *>(arg);
                       char *s = get_string_from_js_result(r);
                       w->on_message(s);
                       g_free(s);
                     }),
                     this);
    webkit_user_content_manager_register_script_message_handler(manager,
                                                                "external");
    init("window.external={invoke:function(s){window.webkit.messageHandlers."
         "external.postMessage(s);}}");

    // A clicked link with a custom (non-http) scheme is an in-app deep link:
    // deliver its route to the page and refuse the navigation, which WebKit
    // otherwise renders as a "URL can't be shown" error page.
    g_signal_connect(
        m_webview, "decide-policy",
        G_CALLBACK(+[](WebKitWebView *web_view, WebKitPolicyDecision *decision,
                       WebKitPolicyDecisionType type, gpointer) -> gboolean {
          if (type != WEBKIT_POLICY_DECISION_TYPE_NAVIGATION_ACTION) {
            return FALSE;
          }
          auto *nav = WEBKIT_NAVIGATION_POLICY_DECISION(decision);
          WebKitNavigationAction *action =
              webkit_navigation_policy_decision_get_navigation_action(nav);
          WebKitURIRequest *request =
              webkit_navigation_action_get_request(action);
          const char *uri = webkit_uri_request_get_uri(request);
          if (uri == nullptr) {
            return FALSE;
          }
          bool is_http = strncmp(uri, "http://", 7) == 0 ||
                         strncmp(uri, "https://", 8) == 0;
          if (is_http) {
            // A clicked link to an external site opens in the browser; a
            // same-origin navigation (SPA route, dev server, reload) stays in
            // the app.
            auto host_of = [](const char *u) -> std::string {
              if (!u) {
                return "";
              }
              std::string s(u);
              size_t m = s.find("://");
              if (m == std::string::npos) {
                return "";
              }
              size_t start = m + 3;
              size_t end = s.find('/', start);
              return s.substr(start, end == std::string::npos
                                         ? std::string::npos
                                         : end - start);
            };
            WebKitNavigationType ntype =
                webkit_navigation_action_get_navigation_type(action);
            const char *current = webkit_web_view_get_uri(web_view);
            if (ntype == WEBKIT_NAVIGATION_TYPE_LINK_CLICKED &&
                !host_of(uri).empty() && host_of(uri) != host_of(current)) {
              GError *open_error = nullptr;
              g_app_info_launch_default_for_uri(uri, nullptr, &open_error);
              if (open_error) {
                g_error_free(open_error);
              }
              webkit_policy_decision_ignore(decision);
              return TRUE;
            }
            return FALSE;
          }

          std::string full(uri);
          size_t marker = full.find("://");
          std::string route =
              (marker != std::string::npos) ? full.substr(marker + 3) : full;
          if (route.empty() || route[0] != '/') {
            route = "/" + route;
          }
          std::string escaped;
          for (char c : route) {
            if (c == '\\' || c == '"') {
              escaped += '\\';
            }
            escaped += c;
          }
          std::string js =
              "window.__peko_deeplink && window.__peko_deeplink(\"" + escaped +
              "\")";
          webkit_web_view_run_javascript(web_view, js.c_str(), nullptr, nullptr,
                                         nullptr);
          webkit_policy_decision_ignore(decision);
          return TRUE;
        }),
        this);

    gtk_container_add(GTK_CONTAINER(m_window), GTK_WIDGET(m_webview));
    gtk_widget_grab_focus(GTK_WIDGET(m_webview));

    WebKitSettings *settings =
        webkit_web_view_get_settings(WEBKIT_WEB_VIEW(m_webview));
    webkit_settings_set_javascript_can_access_clipboard(settings, true);
    if (debug) {
      webkit_settings_set_enable_write_console_messages_to_stdout(settings,
                                                                  true);
      webkit_settings_set_enable_developer_extras(settings, true);
    }

    gtk_widget_show_all(m_window);
  }
  virtual ~gtk_webkit_engine() = default;
  void *window() { return (void *)m_window; }
  void run() { gtk_main(); }
  void terminate() { gtk_main_quit(); }
  void dispatch(std::function<void()> f) {
    g_idle_add_full(G_PRIORITY_HIGH_IDLE, (GSourceFunc)([](void *f) -> int {
                      (*static_cast<dispatch_fn_t *>(f))();
                      return G_SOURCE_REMOVE;
                    }),
                    new std::function<void()>(f),
                    [](void *f) { delete static_cast<dispatch_fn_t *>(f); });
  }

  void set_title(const std::string &title) {
    gtk_window_set_title(GTK_WINDOW(m_window), title.c_str());
  }

  void set_size(int width, int height, int hints) {
    gtk_window_set_resizable(GTK_WINDOW(m_window), hints != WEBVIEW_HINT_FIXED);
    if (hints == WEBVIEW_HINT_NONE) {
      gtk_window_resize(GTK_WINDOW(m_window), width, height);
    } else if (hints == WEBVIEW_HINT_FIXED) {
      gtk_widget_set_size_request(m_window, width, height);
    } else {
      GdkGeometry g;
      g.min_width = g.max_width = width;
      g.min_height = g.max_height = height;
      GdkWindowHints h =
          (hints == WEBVIEW_HINT_MIN ? GDK_HINT_MIN_SIZE : GDK_HINT_MAX_SIZE);
      // This defines either MIN_SIZE, or MAX_SIZE, but not both:
      gtk_window_set_geometry_hints(GTK_WINDOW(m_window), nullptr, &g, h);
    }
  }

  void navigate(const std::string &url) {
    webkit_web_view_load_uri(WEBKIT_WEB_VIEW(m_webview), url.c_str());
  }

  void set_html(const std::string &html) {
    webkit_web_view_load_html(WEBKIT_WEB_VIEW(m_webview), html.c_str(),
                              nullptr);
  }

  void init(const std::string &js) {
    WebKitUserContentManager *manager =
        webkit_web_view_get_user_content_manager(WEBKIT_WEB_VIEW(m_webview));
    webkit_user_content_manager_add_script(
        manager,
        webkit_user_script_new(js.c_str(), WEBKIT_USER_CONTENT_INJECT_TOP_FRAME,
                               WEBKIT_USER_SCRIPT_INJECT_AT_DOCUMENT_START,
                               nullptr, nullptr));
  }

  void eval(const std::string &js) {
    webkit_web_view_run_javascript(WEBKIT_WEB_VIEW(m_webview), js.c_str(),
                                   nullptr, nullptr, nullptr);
  }

private:
  virtual void on_message(const std::string &msg) = 0;

  static char *get_string_from_js_result(WebKitJavascriptResult *r) {
    char *s;
#if WEBKIT_MAJOR_VERSION >= 2 && WEBKIT_MINOR_VERSION >= 22
    JSCValue *value = webkit_javascript_result_get_js_value(r);
    s = jsc_value_to_string(value);
#else
    JSGlobalContextRef ctx = webkit_javascript_result_get_global_context(r);
    JSValueRef value = webkit_javascript_result_get_value(r);
    JSStringRef js = JSValueToStringCopy(ctx, value, nullptr);
    size_t n = JSStringGetMaximumUTF8CStringSize(js);
    s = g_new(char, n);
    JSStringGetUTF8CString(js, s, n);
    JSStringRelease(js);
#endif
    return s;
  }

  GtkWidget *m_window;
  GtkWidget *m_webview;
};

} // namespace detail

using browser_engine = detail::gtk_webkit_engine;

} // namespace webview

#elif defined(WEBVIEW_COCOA)

//
// ====================================================================
//
// This implementation uses Cocoa WKWebView backend on macOS. It is
// written using ObjC runtime and uses WKWebView class as a browser runtime.
// You should pass "-framework Webkit" flag to the compiler.
//
// ====================================================================
//

#include <CoreGraphics/CoreGraphics.h>
// #include <ApplicationServices/ApplicationServices.h>
#include <objc/NSObjCRuntime.h>
#include <objc/runtime.h>
#include <objc/message.h>

namespace webview {
namespace detail {
namespace objc {

// A convenient template function for unconditionally casting the specified
// C-like function into a function that can be called with the given return
// type and arguments. Caller takes full responsibility for ensuring that
// the function call is valid. It is assumed that the function will not
// throw exceptions.
template <typename Result, typename Callable, typename... Args>
Result invoke(Callable callable, Args... args) noexcept {
  return reinterpret_cast<Result (*)(Args...)>(callable)(args...);
}

// Calls objc_msgSend.
template <typename Result, typename... Args>
Result msg_send(Args... args) noexcept {
  return invoke<Result>(objc_msgSend, args...);
}

} // namespace objc

enum NSBackingStoreType : NSUInteger { NSBackingStoreBuffered = 2 };

enum NSWindowStyleMask : NSUInteger {
  NSWindowStyleMaskTitled = 1,
  NSWindowStyleMaskClosable = 2,
  NSWindowStyleMaskMiniaturizable = 4,
  NSWindowStyleMaskResizable = 8
};

enum NSApplicationActivationPolicy : NSInteger {
  NSApplicationActivationPolicyRegular = 0
};

enum WKUserScriptInjectionTime : NSInteger {
  WKUserScriptInjectionTimeAtDocumentStart = 0
};

enum NSModalResponse : NSInteger { NSModalResponseOK = 1 };

// Convenient conversion of string literals.
inline id operator"" _cls(const char *s, std::size_t) {
  return (id)objc_getClass(s);
}
inline SEL operator"" _sel(const char *s, std::size_t) {
  return sel_registerName(s);
}
inline id operator"" _str(const char *s, std::size_t) {
  return objc::msg_send<id>("NSString"_cls, "stringWithUTF8String:"_sel, s);
}

class cocoa_wkwebview_engine {
public:
  cocoa_wkwebview_engine(bool debug, void *window)
      : m_debug{debug}, m_parent_window{window} {
    auto app = get_shared_application();
    auto delegate = create_app_delegate();
    objc_setAssociatedObject(delegate, "webview", (id)this,
                             OBJC_ASSOCIATION_ASSIGN);
    objc::msg_send<void>(app, "setDelegate:"_sel, delegate);

    // See comments related to application lifecycle in create_app_delegate().
    if (window) {
      on_application_did_finish_launching(delegate, app);
    } else {
      // Start the main run loop so that the app delegate gets the
      // NSApplicationDidFinishLaunchingNotification notification after the run
      // loop has started in order to perform further initialization.
      // We need to return from this constructor so this run loop is only
      // temporary.
      objc::msg_send<void>(app, "run"_sel);
    }
  }
  virtual ~cocoa_wkwebview_engine() = default;
  void *window() { return (void *)m_window; }
  void terminate() {
    auto app = get_shared_application();
    objc::msg_send<void>(app, "terminate:"_sel, nullptr);
  }
  void run() {
    auto app = get_shared_application();
    objc::msg_send<void>(app, "run"_sel);
  }
  void dispatch(std::function<void()> f) {
    dispatch_async_f(dispatch_get_main_queue(), new dispatch_fn_t(f),
                     (dispatch_function_t)([](void *arg) {
                       auto f = static_cast<dispatch_fn_t *>(arg);
                       (*f)();
                       delete f;
                     }));
  }
  void set_title(const std::string &title) {
    objc::msg_send<void>(m_window, "setTitle:"_sel,
                         objc::msg_send<id>("NSString"_cls,
                                            "stringWithUTF8String:"_sel,
                                            title.c_str()));
  }
  void set_size(int width, int height, int hints) {
    auto style = static_cast<NSWindowStyleMask>(
        NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
        NSWindowStyleMaskMiniaturizable);
    if (hints != WEBVIEW_HINT_FIXED) {
      style =
          static_cast<NSWindowStyleMask>(style | NSWindowStyleMaskResizable);
    }
    objc::msg_send<void>(m_window, "setStyleMask:"_sel, style);

    if (hints == WEBVIEW_HINT_MIN) {
      objc::msg_send<void>(m_window, "setContentMinSize:"_sel,
                           CGSizeMake(width, height));
    } else if (hints == WEBVIEW_HINT_MAX) {
      objc::msg_send<void>(m_window, "setContentMaxSize:"_sel,
                           CGSizeMake(width, height));
    } else {
      objc::msg_send<void>(m_window, "setFrame:display:animate:"_sel,
                           CGRectMake(0, 0, width, height), YES, NO);
    }
    objc::msg_send<void>(m_window, "center"_sel);
  }
  void navigate(const std::string &url) {
    auto nsurl = objc::msg_send<id>(
        "NSURL"_cls, "URLWithString:"_sel,
        objc::msg_send<id>("NSString"_cls, "stringWithUTF8String:"_sel,
                           url.c_str()));

    objc::msg_send<void>(
        m_webview, "loadRequest:"_sel,
        objc::msg_send<id>("NSURLRequest"_cls, "requestWithURL:"_sel, nsurl));
  }
  void set_html(const std::string &html) {
    objc::msg_send<void>(m_webview, "loadHTMLString:baseURL:"_sel,
                         objc::msg_send<id>("NSString"_cls,
                                            "stringWithUTF8String:"_sel,
                                            html.c_str()),
                         nullptr);
  }
  void init(const std::string &js) {
    // Equivalent Obj-C:
    // [m_manager addUserScript:[[WKUserScript alloc] initWithSource:[NSString stringWithUTF8String:js.c_str()] injectionTime:WKUserScriptInjectionTimeAtDocumentStart forMainFrameOnly:YES]]
    objc::msg_send<void>(
        m_manager, "addUserScript:"_sel,
        objc::msg_send<id>(objc::msg_send<id>("WKUserScript"_cls, "alloc"_sel),
                           "initWithSource:injectionTime:forMainFrameOnly:"_sel,
                           objc::msg_send<id>("NSString"_cls,
                                              "stringWithUTF8String:"_sel,
                                              js.c_str()),
                           WKUserScriptInjectionTimeAtDocumentStart, YES));
  }
  void eval(const std::string &js) {
    objc::msg_send<void>(m_webview, "evaluateJavaScript:completionHandler:"_sel,
                         objc::msg_send<id>("NSString"_cls,
                                            "stringWithUTF8String:"_sel,
                                            js.c_str()),
                         nullptr);
  }

private:
  virtual void on_message(const std::string &msg) = 0;
  id create_app_delegate() {
    // Note: Avoid registering the class name "AppDelegate" as it is the
    // default name in projects created with Xcode, and using the same name
    // causes objc_registerClassPair to crash.
    auto cls = objc_allocateClassPair((Class) "NSResponder"_cls,
                                      "WebviewAppDelegate", 0);
    class_addProtocol(cls, objc_getProtocol("NSTouchBarProvider"));
    class_addMethod(cls, "applicationShouldTerminateAfterLastWindowClosed:"_sel,
                    (IMP)(+[](id, SEL, id) -> BOOL { return 1; }), "c@:@");
    // If the library was not initialized with an existing window then the user
    // is likely managing the application lifecycle and we would not get the
    // "applicationDidFinishLaunching:" message and therefore do not need to
    // add this method.
    if (!m_parent_window) {
      class_addMethod(cls, "applicationDidFinishLaunching:"_sel,
                      (IMP)(+[](id self, SEL, id notification) {
                        auto app =
                            objc::msg_send<id>(notification, "object"_sel);
                        auto w = get_associated_webview(self);
                        w->on_application_did_finish_launching(self, app);
                      }),
                      "v@:@");
    }
    objc_registerClassPair(cls);
    return objc::msg_send<id>((id)cls, "new"_sel);
  }
  id create_script_message_handler() {
    auto cls = objc_allocateClassPair((Class) "NSResponder"_cls,
                                      "WebkitScriptMessageHandler", 0);
    class_addProtocol(cls, objc_getProtocol("WKScriptMessageHandler"));
    class_addMethod(
        cls, "userContentController:didReceiveScriptMessage:"_sel,
        (IMP)(+[](id self, SEL, id, id msg) {
          auto w = get_associated_webview(self);
          w->on_message(objc::msg_send<const char *>(
              objc::msg_send<id>(msg, "body"_sel), "UTF8String"_sel));
        }),
        "v@:@@");
    objc_registerClassPair(cls);
    auto instance = objc::msg_send<id>((id)cls, "new"_sel);
    objc_setAssociatedObject(instance, "webview", (id)this,
                             OBJC_ASSOCIATION_ASSIGN);
    return instance;
  }
  static id create_webkit_ui_delegate() {
    auto cls =
        objc_allocateClassPair((Class) "NSObject"_cls, "WebkitUIDelegate", 0);
    class_addProtocol(cls, objc_getProtocol("WKUIDelegate"));
    class_addMethod(
        cls,
        "webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:"_sel,
        (IMP)(+[](id, SEL, id, id parameters, id, id completion_handler) {
          auto allows_multiple_selection =
              objc::msg_send<BOOL>(parameters, "allowsMultipleSelection"_sel);
          auto allows_directories =
              objc::msg_send<BOOL>(parameters, "allowsDirectories"_sel);

          // Show a panel for selecting files.
          auto panel = objc::msg_send<id>("NSOpenPanel"_cls, "openPanel"_sel);
          objc::msg_send<void>(panel, "setCanChooseFiles:"_sel, YES);
          objc::msg_send<void>(panel, "setCanChooseDirectories:"_sel,
                               allows_directories);
          objc::msg_send<void>(panel, "setAllowsMultipleSelection:"_sel,
                               allows_multiple_selection);
          auto modal_response =
              objc::msg_send<NSModalResponse>(panel, "runModal"_sel);

          // Get the URLs for the selected files. If the modal was canceled
          // then we pass null to the completion handler to signify
          // cancellation.
          id urls = modal_response == NSModalResponseOK
                        ? objc::msg_send<id>(panel, "URLs"_sel)
                        : nullptr;

          // Invoke the completion handler block.
          auto sig = objc::msg_send<id>("NSMethodSignature"_cls,
                                        "signatureWithObjCTypes:"_sel, "v@?@");
          auto invocation = objc::msg_send<id>(
              "NSInvocation"_cls, "invocationWithMethodSignature:"_sel, sig);
          objc::msg_send<void>(invocation, "setTarget:"_sel,
                               completion_handler);
          objc::msg_send<void>(invocation, "setArgument:atIndex:"_sel, &urls,
                               1);
          objc::msg_send<void>(invocation, "invoke"_sel);
        }),
        "v@:@@@@");
    objc_registerClassPair(cls);
    return objc::msg_send<id>((id)cls, "new"_sel);
  }
  static id create_navigation_delegate() {
    // A clicked link to an external site (http/https on a different host than
    // the loaded page) opens in the user's browser instead of replacing the
    // app's own view. Same-origin navigations (SPA routes, the dev server, a
    // reload) proceed normally.
    auto cls = objc_allocateClassPair((Class) "NSObject"_cls,
                                      "WebkitNavigationDelegate", 0);
    class_addProtocol(cls, objc_getProtocol("WKNavigationDelegate"));
    class_addMethod(
        cls,
        "webView:decidePolicyForNavigationAction:decisionHandler:"_sel,
        (IMP)(+[](id, SEL, id web_view, id action, id decision_handler) {
          // WKNavigationTypeLinkActivated == 0.
          auto nav_type = objc::msg_send<long>(action, "navigationType"_sel);
          auto request = objc::msg_send<id>(action, "request"_sel);
          auto url = objc::msg_send<id>(request, "URL"_sel);
          auto scheme = url ? objc::msg_send<id>(url, "scheme"_sel) : nullptr;
          bool is_http =
              scheme && objc::msg_send<BOOL>(scheme, "hasPrefix:"_sel, "http"_str);
          auto target_host = url ? objc::msg_send<id>(url, "host"_sel) : nullptr;
          auto current_url = objc::msg_send<id>(web_view, "URL"_sel);
          auto current_host =
              current_url ? objc::msg_send<id>(current_url, "host"_sel) : nullptr;
          bool same_host =
              target_host && current_host &&
              objc::msg_send<BOOL>(target_host, "isEqualToString:"_sel,
                                   current_host);
          bool external = is_http && nav_type == 0 && target_host && !same_host;

          // WKNavigationActionPolicyCancel == 0, Allow == 1.
          long policy = external ? 0 : 1;
          if (external) {
            auto workspace = objc::msg_send<id>("NSWorkspace"_cls,
                                                "sharedWorkspace"_sel);
            objc::msg_send<void>(workspace, "openURL:"_sel, url);
          }

          // Invoke decisionHandler(policy).
          auto sig = objc::msg_send<id>("NSMethodSignature"_cls,
                                        "signatureWithObjCTypes:"_sel, "v@?q");
          auto invocation = objc::msg_send<id>(
              "NSInvocation"_cls, "invocationWithMethodSignature:"_sel, sig);
          objc::msg_send<void>(invocation, "setTarget:"_sel, decision_handler);
          objc::msg_send<void>(invocation, "setArgument:atIndex:"_sel, &policy, 1);
          objc::msg_send<void>(invocation, "invoke"_sel);
        }),
        "v@:@@@");
    objc_registerClassPair(cls);
    return objc::msg_send<id>((id)cls, "new"_sel);
  }
  static id get_shared_application() {
    return objc::msg_send<id>("NSApplication"_cls, "sharedApplication"_sel);
  }
  static cocoa_wkwebview_engine *get_associated_webview(id object) {
    auto w =
        (cocoa_wkwebview_engine *)objc_getAssociatedObject(object, "webview");
    assert(w);
    return w;
  }
  static id get_main_bundle() noexcept {
    return objc::msg_send<id>("NSBundle"_cls, "mainBundle"_sel);
  }
  static bool is_app_bundled() noexcept {
    auto bundle = get_main_bundle();
    if (!bundle) {
      return false;
    }
    auto bundle_path = objc::msg_send<id>(bundle, "bundlePath"_sel);
    auto bundled =
        objc::msg_send<BOOL>(bundle_path, "hasSuffix:"_sel, ".app"_str);
    return !!bundled;
  }
  void on_application_did_finish_launching(id /*delegate*/, id app) {
    // See comments related to application lifecycle in create_app_delegate().
    if (!m_parent_window) {
      // Stop the main run loop so that we can return
      // from the constructor.
      objc::msg_send<void>(app, "stop:"_sel, nullptr);
    }

    // Activate the app if it is not bundled.
    // Bundled apps launched from Finder are activated automatically but
    // otherwise not. Activating the app even when it has been launched from
    // Finder does not seem to be harmful but calling this function is rarely
    // needed as proper activation is normally taken care of for us.
    // Bundled apps have a default activation policy of
    // NSApplicationActivationPolicyRegular while non-bundled apps have a
    // default activation policy of NSApplicationActivationPolicyProhibited.
    if (!is_app_bundled()) {
      // "setActivationPolicy:" must be invoked before
      // "activateIgnoringOtherApps:" for activation to work.
      objc::msg_send<void>(app, "setActivationPolicy:"_sel,
                           NSApplicationActivationPolicyRegular);
      // Activate the app regardless of other active apps.
      // This can be obtrusive so we only do it when necessary.
      objc::msg_send<void>(app, "activateIgnoringOtherApps:"_sel, YES);
    }

    // Main window
    if (!m_parent_window) {
      m_window = objc::msg_send<id>("NSWindow"_cls, "alloc"_sel);
      auto style = NSWindowStyleMaskTitled;
      m_window = objc::msg_send<id>(
          m_window, "initWithContentRect:styleMask:backing:defer:"_sel,
          CGRectMake(0, 0, 0, 0), style, NSBackingStoreBuffered, NO);
    } else {
      m_window = (id)m_parent_window;
    }

    // Webview
    // objc::msg_send<id>(config, "release"_sel);
    auto config = objc::msg_send<id>("WKWebViewConfiguration"_cls, "new"_sel);
    m_manager = objc::msg_send<id>(config, "userContentController"_sel);
    m_webview = objc::msg_send<id>("WKWebView"_cls, "alloc"_sel);

    if (m_debug) {
      // Equivalent Obj-C:
      // [[config preferences] setValue:@YES forKey:@"developerExtrasEnabled"];
      objc::msg_send<id>(
          objc::msg_send<id>(config, "preferences"_sel), "setValue:forKey:"_sel,
          objc::msg_send<id>("NSNumber"_cls, "numberWithBool:"_sel, YES),
          "developerExtrasEnabled"_str);
    }

    // Equivalent Obj-C:
    // [[config preferences] setValue:@YES forKey:@"fullScreenEnabled"];
    objc::msg_send<id>(
        objc::msg_send<id>(config, "preferences"_sel), "setValue:forKey:"_sel,
        objc::msg_send<id>("NSNumber"_cls, "numberWithBool:"_sel, YES),
        "fullScreenEnabled"_str);

    // Equivalent Obj-C:
    // [[config preferences] setValue:@YES forKey:@"javaScriptCanAccessClipboard"];
    objc::msg_send<id>(
        objc::msg_send<id>(config, "preferences"_sel), "setValue:forKey:"_sel,
        objc::msg_send<id>("NSNumber"_cls, "numberWithBool:"_sel, YES),
        "javaScriptCanAccessClipboard"_str);

    // Equivalent Obj-C:
    // [[config preferences] setValue:@YES forKey:@"DOMPasteAllowed"];
    objc::msg_send<id>(
        objc::msg_send<id>(config, "preferences"_sel), "setValue:forKey:"_sel,
        objc::msg_send<id>("NSNumber"_cls, "numberWithBool:"_sel, YES),
        "DOMPasteAllowed"_str);

    auto ui_delegate = create_webkit_ui_delegate();
    objc::msg_send<void>(m_webview, "initWithFrame:configuration:"_sel,
                         CGRectMake(0, 0, 0, 0), config);
    objc::msg_send<void>(m_webview, "setUIDelegate:"_sel, ui_delegate);
    auto nav_delegate = create_navigation_delegate();
    objc::msg_send<void>(m_webview, "setNavigationDelegate:"_sel, nav_delegate);
    auto script_message_handler = create_script_message_handler();
    objc::msg_send<void>(m_manager, "addScriptMessageHandler:name:"_sel,
                         script_message_handler, "external"_str);

    init(R""(
      window.external = {
        invoke: function(s) {
          window.webkit.messageHandlers.external.postMessage(s);
        },
      };
      )"");
    objc::msg_send<void>(m_window, "setContentView:"_sel, m_webview);
    objc::msg_send<void>(m_window, "makeKeyAndOrderFront:"_sel, nullptr);
  }
  bool m_debug;
  void *m_parent_window;
  id m_window;
  id m_webview;
  id m_manager;
};

} // namespace detail

using browser_engine = detail::cocoa_wkwebview_engine;

} // namespace webview

#elif defined(WEBVIEW_EDGE)

//
// ====================================================================
//
// This implementation uses Win32 API to create a native window. It
// uses Edge/Chromium webview2 backend as a browser engine.
//
// ====================================================================
//

#define WIN32_LEAN_AND_MEAN
#include <shlobj.h>
#include <shlwapi.h>

// The native menu (peko_menu_windows.c, linked into every pekoui build) builds
// a keyboard-accelerator table for its items. The message loop calls this per
// message so a shortcut dispatches its menu command; it returns nonzero when
// the message was consumed, and 0 when no menu (and so no table) is installed.
extern "C" int peko_menu_translate_accel(void *msg);
// Key presses in the web content run in the WebView2 process, not the host
// message loop, so the controller's AcceleratorKeyPressed event forwards the
// pressed virtual key here to run a matching menu accelerator. Returns nonzero
// when an accelerator ran, so the event is marked handled.
extern "C" int peko_menu_dispatch_accel_key(unsigned int vkey);
#include <stdlib.h>
#include <windows.h>

// DirectComposition hosting. The web view renders into a composition visual so
// the window can present a per-pixel transparent surface over the desktop.
#include <d3d11.h>
#include <dcomp.h>
#include <dxgi.h>
#include <windowsx.h>

#include "WebView2.h"

#ifdef _MSC_VER
#pragma comment(lib, "advapi32.lib")
#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "shell32.lib")
#pragma comment(lib, "shlwapi.lib")
#pragma comment(lib, "user32.lib")
#pragma comment(lib, "version.lib")
#endif

namespace webview {
namespace detail {

using msg_cb_t = std::function<void(const std::string)>;

// Converts a narrow (UTF-8-encoded) string into a wide (UTF-16-encoded) string.
inline std::wstring widen_string(const std::string &input) {
  if (input.empty()) {
    return std::wstring();
  }
  UINT cp = CP_UTF8;
  DWORD flags = MB_ERR_INVALID_CHARS;
  auto input_c = input.c_str();
  auto input_length = static_cast<int>(input.size());
  auto required_length =
      MultiByteToWideChar(cp, flags, input_c, input_length, nullptr, 0);
  if (required_length > 0) {
    std::wstring output(static_cast<std::size_t>(required_length), L'\0');
    if (MultiByteToWideChar(cp, flags, input_c, input_length, &output[0],
                            required_length) > 0) {
      return output;
    }
  }
  // Failed to convert string from UTF-8 to UTF-16
  return std::wstring();
}

// Converts a wide (UTF-16-encoded) string into a narrow (UTF-8-encoded) string.
inline std::string narrow_string(const std::wstring &input) {
  if (input.empty()) {
    return std::string();
  }
  UINT cp = CP_UTF8;
  DWORD flags = WC_ERR_INVALID_CHARS;
  auto input_c = input.c_str();
  auto input_length = static_cast<int>(input.size());
  auto required_length = WideCharToMultiByte(cp, flags, input_c, input_length,
                                             nullptr, 0, nullptr, nullptr);
  if (required_length > 0) {
    std::string output(static_cast<std::size_t>(required_length), '\0');
    if (WideCharToMultiByte(cp, flags, input_c, input_length, &output[0],
                            required_length, nullptr, nullptr) > 0) {
      return output;
    }
  }
  // Failed to convert string from UTF-16 to UTF-8
  return std::string();
}

// Parses a version string with 1-4 integral components, e.g. "1.2.3.4".
// Missing or invalid components default to 0, and excess components are ignored.
template <typename T>
std::array<unsigned int, 4>
parse_version(const std::basic_string<T> &version) noexcept {
  auto parse_component = [](auto sb, auto se) -> unsigned int {
    try {
      auto n = std::stol(std::basic_string<T>(sb, se));
      return n < 0 ? 0 : n;
    } catch (std::exception &) {
      return 0;
    }
  };
  auto end = version.end();
  auto sb = version.begin(); // subrange begin
  auto se = sb;              // subrange end
  unsigned int ci = 0;       // component index
  std::array<unsigned int, 4> components{};
  while (sb != end && se != end && ci < components.size()) {
    if (*se == static_cast<T>('.')) {
      components[ci++] = parse_component(sb, se);
      sb = ++se;
      continue;
    }
    ++se;
  }
  if (sb < se && ci < components.size()) {
    components[ci] = parse_component(sb, se);
  }
  return components;
}

template <typename T, std::size_t Length>
auto parse_version(const T (&version)[Length]) noexcept {
  return parse_version(std::basic_string<T>(version, Length));
}

std::wstring get_file_version_string(const std::wstring &file_path) noexcept {
  DWORD dummy_handle; // Unused
  DWORD info_buffer_length =
      GetFileVersionInfoSizeW(file_path.c_str(), &dummy_handle);
  if (info_buffer_length == 0) {
    return std::wstring();
  }
  std::vector<char> info_buffer;
  info_buffer.reserve(info_buffer_length);
  if (!GetFileVersionInfoW(file_path.c_str(), 0, info_buffer_length,
                           info_buffer.data())) {
    return std::wstring();
  }
  auto sub_block = L"\\StringFileInfo\\040904B0\\ProductVersion";
  LPWSTR version = nullptr;
  unsigned int version_length = 0;
  if (!VerQueryValueW(info_buffer.data(), sub_block,
                      reinterpret_cast<LPVOID *>(&version), &version_length)) {
    return std::wstring();
  }
  if (!version || version_length == 0) {
    return std::wstring();
  }
  return std::wstring(version, version_length);
}

// A wrapper around COM library initialization. Calls CoInitializeEx in the
// constructor and CoUninitialize in the destructor.
class com_init_wrapper {
public:
  com_init_wrapper(DWORD dwCoInit) {
    // We can safely continue as long as COM was either successfully
    // initialized or already initialized.
    // RPC_E_CHANGED_MODE means that CoInitializeEx was already called with
    // a different concurrency model.
    switch (CoInitializeEx(nullptr, dwCoInit)) {
    case S_OK:
    case S_FALSE:
      m_initialized = true;
      break;
    }
  }

  ~com_init_wrapper() {
    if (m_initialized) {
      CoUninitialize();
      m_initialized = false;
    }
  }

  com_init_wrapper(const com_init_wrapper &other) = delete;
  com_init_wrapper &operator=(const com_init_wrapper &other) = delete;
  com_init_wrapper(com_init_wrapper &&other) = delete;
  com_init_wrapper &operator=(com_init_wrapper &&other) = delete;

  bool is_initialized() const { return m_initialized; }

private:
  bool m_initialized = false;
};

// Holds a symbol name and associated type for code clarity.
template <typename T> class library_symbol {
public:
  using type = T;

  constexpr explicit library_symbol(const char *name) : m_name(name) {}
  constexpr const char *get_name() const { return m_name; }

private:
  const char *m_name;
};

// Loads a native shared library and allows one to get addresses for those
// symbols.
class native_library {
public:
  explicit native_library(const wchar_t *name) : m_handle(LoadLibraryW(name)) {}

  ~native_library() {
    if (m_handle) {
      FreeLibrary(m_handle);
      m_handle = nullptr;
    }
  }

  native_library(const native_library &other) = delete;
  native_library &operator=(const native_library &other) = delete;
  native_library(native_library &&other) = default;
  native_library &operator=(native_library &&other) = default;

  // Returns true if the library is currently loaded; otherwise false.
  operator bool() const { return is_loaded(); }

  // Get the address for the specified symbol or nullptr if not found.
  template <typename Symbol>
  typename Symbol::type get(const Symbol &symbol) const {
    if (is_loaded()) {
      return reinterpret_cast<typename Symbol::type>(
          GetProcAddress(m_handle, symbol.get_name()));
    }
    return nullptr;
  }

  // Returns true if the library is currently loaded; otherwise false.
  bool is_loaded() const { return !!m_handle; }

  void detach() { m_handle = nullptr; }

private:
  HMODULE m_handle = nullptr;
};

struct user32_symbols {
  using DPI_AWARENESS_CONTEXT = HANDLE;
  using SetProcessDpiAwarenessContext_t = BOOL(WINAPI *)(DPI_AWARENESS_CONTEXT);
  using SetProcessDPIAware_t = BOOL(WINAPI *)();

  static constexpr auto SetProcessDpiAwarenessContext =
      library_symbol<SetProcessDpiAwarenessContext_t>(
          "SetProcessDpiAwarenessContext");
  static constexpr auto SetProcessDPIAware =
      library_symbol<SetProcessDPIAware_t>("SetProcessDPIAware");
};

struct shcore_symbols {
  typedef enum { PROCESS_PER_MONITOR_DPI_AWARE = 2 } PROCESS_DPI_AWARENESS;
  using SetProcessDpiAwareness_t = HRESULT(WINAPI *)(PROCESS_DPI_AWARENESS);

  static constexpr auto SetProcessDpiAwareness =
      library_symbol<SetProcessDpiAwareness_t>("SetProcessDpiAwareness");
};

class reg_key {
public:
  explicit reg_key(HKEY root_key, const wchar_t *sub_key, DWORD options,
                   REGSAM sam_desired) {
    HKEY handle;
    auto status =
        RegOpenKeyExW(root_key, sub_key, options, sam_desired, &handle);
    if (status == ERROR_SUCCESS) {
      m_handle = handle;
    }
  }

  explicit reg_key(HKEY root_key, const std::wstring &sub_key, DWORD options,
                   REGSAM sam_desired)
      : reg_key(root_key, sub_key.c_str(), options, sam_desired) {}

  virtual ~reg_key() {
    if (m_handle) {
      RegCloseKey(m_handle);
      m_handle = nullptr;
    }
  }

  reg_key(const reg_key &other) = delete;
  reg_key &operator=(const reg_key &other) = delete;
  reg_key(reg_key &&other) = delete;
  reg_key &operator=(reg_key &&other) = delete;

  bool is_open() const { return !!m_handle; }
  bool get_handle() const { return m_handle; }

  std::wstring query_string(const wchar_t *name) const {
    DWORD buf_length = 0;
    // Get the size of the data in bytes.
    auto status = RegQueryValueExW(m_handle, name, nullptr, nullptr, nullptr,
                                   &buf_length);
    if (status != ERROR_SUCCESS && status != ERROR_MORE_DATA) {
      return std::wstring();
    }
    // Read the data.
    std::wstring result(buf_length / sizeof(wchar_t), 0);
    auto buf = reinterpret_cast<LPBYTE>(&result[0]);
    status =
        RegQueryValueExW(m_handle, name, nullptr, nullptr, buf, &buf_length);
    if (status != ERROR_SUCCESS) {
      return std::wstring();
    }
    // Remove trailing null-characters.
    for (std::size_t length = result.size(); length > 0; --length) {
      if (result[length - 1] != 0) {
        result.resize(length);
        break;
      }
    }
    return result;
  }

private:
  HKEY m_handle = nullptr;
};

inline bool enable_dpi_awareness() {
  auto user32 = native_library(L"user32.dll");
  if (auto fn = user32.get(user32_symbols::SetProcessDpiAwarenessContext)) {
    // Per-Monitor V2 (-4), then V1 (-3). The context values are passed
    // explicitly rather than through a header macro, which is missing or wrong
    // in some cross-compile SDKs and would leave the process DPI unaware. An
    // unaware process gets a virtualized (scaled-down) client rect, so the web
    // view is sized to a fraction of the window on a high-DPI display.
    for (intptr_t context : {-4, -3}) {
      if (fn(reinterpret_cast<user32_symbols::DPI_AWARENESS_CONTEXT>(context))) {
        return true;
      }
      if (GetLastError() == ERROR_ACCESS_DENIED) {
        return true; // Already set, e.g. by an embedded manifest.
      }
    }
  }
  if (auto shcore = native_library(L"shcore.dll")) {
    if (auto fn = shcore.get(shcore_symbols::SetProcessDpiAwareness)) {
      auto result = fn(shcore_symbols::PROCESS_PER_MONITOR_DPI_AWARE);
      return result == S_OK || result == E_ACCESSDENIED;
    }
  }
  if (auto fn = user32.get(user32_symbols::SetProcessDPIAware)) {
    return !!fn();
  }
  return true;
}

// The window's client rect in PHYSICAL pixels. The web view composes through a
// DirectComposition visual, which uses physical pixels and ignores DPI
// virtualization, but GetClientRect returns virtualized (logical) pixels when
// the process is not per-monitor DPI aware, so the view would fill only 1/scale
// of the window on a high-DPI monitor.
//
// The thread's DPI awareness context is temporarily set to per-monitor-aware V2
// (-4) around the GetClientRect call. Window/GDI queries return values relative
// to the calling thread's awareness, so within that context GetClientRect
// reports the true physical client rect regardless of the process's own
// awareness. The previous context is restored immediately. Querying the monitor
// DPI directly is not reliable here: for an unaware process the effective DPI is
// itself virtualized to 96.
inline RECT peko_client_bounds_physical(HWND wnd) {
  RECT bounds;
  typedef HANDLE(WINAPI * SetThreadDpiAwarenessContext_t)(HANDLE);
  static auto set_thread_dpi = reinterpret_cast<SetThreadDpiAwarenessContext_t>(
      GetProcAddress(GetModuleHandleW(L"user32.dll"),
                     "SetThreadDpiAwarenessContext"));
  if (set_thread_dpi) {
    HANDLE previous =
        set_thread_dpi(reinterpret_cast<HANDLE>(static_cast<intptr_t>(-4)));
    GetClientRect(wnd, &bounds);
    if (previous) {
      set_thread_dpi(previous);
    }
  } else {
    GetClientRect(wnd, &bounds);
  }
  return bounds;
}

// Enable built-in WebView2Loader implementation by default.
#ifndef WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL
#define WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL 1
#endif

// Link WebView2Loader.dll explicitly by default only if the built-in
// implementation is enabled.
#ifndef WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK
#define WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL
#endif

// Explicit linking of WebView2Loader.dll should be used along with
// the built-in implementation.
#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1 &&                                    \
    WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK != 1
#undef WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK
#error Please set WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK=1.
#endif

#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1
// Gets the last component of a Windows native file path.
// For example, if the path is "C:\a\b" then the result is "b".
template <typename T>
std::basic_string<T>
get_last_native_path_component(const std::basic_string<T> &path) {
  if (auto pos = path.find_last_of(static_cast<T>('\\'));
      pos != std::basic_string<T>::npos) {
    return path.substr(pos + 1);
  }
  return std::basic_string<T>();
}
#endif /* WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL */

template <typename T> struct cast_info_t {
  using type = T;
  IID iid;
};

namespace mswebview2 {
static constexpr IID
    IID_ICoreWebView2CreateCoreWebView2ControllerCompletedHandler{
        0x6C4819F3, 0xC9B7, 0x4260, 0x81, 0x27, 0xC9,
        0xF5,       0xBD,   0xE7,   0xF6, 0x8C};
static constexpr IID
    IID_ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler{
        0x4E8A3389, 0xC9D8, 0x4BD2, 0xB6, 0xB5, 0x12,
        0x4F,       0xEE,   0x6C,   0xC1, 0x4D};
static constexpr IID IID_ICoreWebView2PermissionRequestedEventHandler{
    0x15E1C6A3, 0xC72A, 0x4DF3, 0x91, 0xD7, 0xD0, 0x97, 0xFB, 0xEC, 0x6B, 0xFD};
static constexpr IID IID_ICoreWebView2WebMessageReceivedEventHandler{
    0x57213F19, 0x00E6, 0x49FA, 0x8E, 0x07, 0x89, 0x8E, 0xA0, 0x1E, 0xCB, 0xD2};

#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1
enum class webview2_runtime_type { installed = 0, embedded = 1 };

namespace webview2_symbols {
using CreateWebViewEnvironmentWithOptionsInternal_t =
    HRESULT(STDMETHODCALLTYPE *)(
        bool, webview2_runtime_type, PCWSTR, IUnknown *,
        ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler *);
using DllCanUnloadNow_t = HRESULT(STDMETHODCALLTYPE *)();

static constexpr auto CreateWebViewEnvironmentWithOptionsInternal =
    library_symbol<CreateWebViewEnvironmentWithOptionsInternal_t>(
        "CreateWebViewEnvironmentWithOptionsInternal");
static constexpr auto DllCanUnloadNow =
    library_symbol<DllCanUnloadNow_t>("DllCanUnloadNow");
} // namespace webview2_symbols
#endif /* WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL */

#if WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK == 1
namespace webview2_symbols {
using CreateCoreWebView2EnvironmentWithOptions_t = HRESULT(STDMETHODCALLTYPE *)(
    PCWSTR, PCWSTR, ICoreWebView2EnvironmentOptions *,
    ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler *);
using GetAvailableCoreWebView2BrowserVersionString_t =
    HRESULT(STDMETHODCALLTYPE *)(PCWSTR, LPWSTR *);

static constexpr auto CreateCoreWebView2EnvironmentWithOptions =
    library_symbol<CreateCoreWebView2EnvironmentWithOptions_t>(
        "CreateCoreWebView2EnvironmentWithOptions");
static constexpr auto GetAvailableCoreWebView2BrowserVersionString =
    library_symbol<GetAvailableCoreWebView2BrowserVersionString_t>(
        "GetAvailableCoreWebView2BrowserVersionString");
} // namespace webview2_symbols
#endif /* WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK */

class loader {
public:
  HRESULT create_environment_with_options(
      PCWSTR browser_dir, PCWSTR user_data_dir,
      ICoreWebView2EnvironmentOptions *env_options,
      ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler
          *created_handler) const {
#if WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK == 1
    if (m_lib.is_loaded()) {
      if (auto fn = m_lib.get(
              webview2_symbols::CreateCoreWebView2EnvironmentWithOptions)) {
        return fn(browser_dir, user_data_dir, env_options, created_handler);
      }
    }
#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1
    return create_environment_with_options_impl(browser_dir, user_data_dir,
                                                env_options, created_handler);
#else
    return S_FALSE;
#endif
#else
    return ::CreateCoreWebView2EnvironmentWithOptions(
        browser_dir, user_data_dir, env_options, created_handler);
#endif /* WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK */
  }

  HRESULT
  get_available_browser_version_string(PCWSTR browser_dir,
                                       LPWSTR *version) const {
#if WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK == 1
    if (m_lib.is_loaded()) {
      if (auto fn = m_lib.get(
              webview2_symbols::GetAvailableCoreWebView2BrowserVersionString)) {
        return fn(browser_dir, version);
      }
    }
#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1
    return get_available_browser_version_string_impl(browser_dir, version);
#else
    return S_FALSE;
#endif
#else
    return ::GetAvailableCoreWebView2BrowserVersionString(browser_dir, version);
#endif /* WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK */
  }

private:
#if WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL == 1
  struct client_info_t {
    bool found = false;
    std::wstring dll_path;
    std::wstring version;
    webview2_runtime_type runtime_type;
  };

  HRESULT create_environment_with_options_impl(
      PCWSTR browser_dir, PCWSTR user_data_dir,
      ICoreWebView2EnvironmentOptions *env_options,
      ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler
          *created_handler) const {
    auto found_client = find_available_client(browser_dir);
    if (!found_client.found) {
      return -1;
    }
    auto client_dll = native_library(found_client.dll_path.c_str());
    if (auto fn = client_dll.get(
            webview2_symbols::CreateWebViewEnvironmentWithOptionsInternal)) {
      return fn(true, found_client.runtime_type, user_data_dir, env_options,
                created_handler);
    }
    if (auto fn = client_dll.get(webview2_symbols::DllCanUnloadNow)) {
      if (!fn()) {
        client_dll.detach();
      }
    }
    return ERROR_SUCCESS;
  }

  HRESULT
  get_available_browser_version_string_impl(PCWSTR browser_dir,
                                            LPWSTR *version) const {
    if (!version) {
      return -1;
    }
    auto found_client = find_available_client(browser_dir);
    if (!found_client.found) {
      return -1;
    }
    auto info_length_bytes =
        found_client.version.size() * sizeof(found_client.version[0]);
    auto info = static_cast<LPWSTR>(CoTaskMemAlloc(info_length_bytes));
    if (!info) {
      return -1;
    }
    CopyMemory(info, found_client.version.c_str(), info_length_bytes);
    *version = info;
    return 0;
  }

  client_info_t find_available_client(PCWSTR browser_dir) const {
    if (browser_dir) {
      return find_embedded_client(api_version, browser_dir);
    }
    auto found_client =
        find_installed_client(api_version, true, default_release_channel_guid);
    if (!found_client.found) {
      found_client = find_installed_client(api_version, false,
                                           default_release_channel_guid);
    }
    return found_client;
  }

  std::wstring make_client_dll_path(const std::wstring &dir) const {
    auto dll_path = dir;
    if (!dll_path.empty()) {
      auto last_char = dir[dir.size() - 1];
      if (last_char != L'\\' && last_char != L'/') {
        dll_path += L'\\';
      }
    }
    dll_path += L"EBWebView\\";
#if defined(_M_X64) || defined(__x86_64__)
    dll_path += L"x64";
#elif defined(_M_IX86) || defined(__i386__)
    dll_path += L"x86";
#elif defined(_M_ARM64) || defined(__aarch64__)
    dll_path += L"arm64";
#else
#error WebView2 integration for this platform is not yet supported.
#endif
    dll_path += L"\\EmbeddedBrowserWebView.dll";
    return dll_path;
  }

  client_info_t
  find_installed_client(unsigned int min_api_version, bool system,
                        const std::wstring &release_channel) const {
    std::wstring sub_key = client_state_reg_sub_key;
    sub_key += release_channel;
    auto root_key = system ? HKEY_LOCAL_MACHINE : HKEY_CURRENT_USER;
    reg_key key(root_key, sub_key, 0, KEY_READ | KEY_WOW64_32KEY);
    if (!key.is_open()) {
      return {};
    }
    auto ebwebview_value = key.query_string(L"EBWebView");

    auto client_version_string =
        get_last_native_path_component(ebwebview_value);
    auto client_version = parse_version(client_version_string);
    if (client_version[2] < min_api_version) {
      // Our API version is greater than the runtime API version.
      return {};
    }

    auto client_dll_path = make_client_dll_path(ebwebview_value);
    return {true, client_dll_path, client_version_string,
            webview2_runtime_type::installed};
  }

  client_info_t find_embedded_client(unsigned int min_api_version,
                                     const std::wstring &dir) const {
    auto client_dll_path = make_client_dll_path(dir);

    auto client_version_string = get_file_version_string(client_dll_path);
    auto client_version = parse_version(client_version_string);
    if (client_version[2] < min_api_version) {
      // Our API version is greater than the runtime API version.
      return {};
    }

    return {true, client_dll_path, client_version_string,
            webview2_runtime_type::embedded};
  }

  // The minimum WebView2 API version we need regardless of the SDK release
  // actually used. The number comes from the SDK release version,
  // e.g. 1.0.1150.38. To be safe the SDK should have a number that is greater
  // than or equal to this number. The Edge browser webview client must
  // have a number greater than or equal to this number.
  static constexpr unsigned int api_version = 1150;

  static constexpr auto client_state_reg_sub_key =
      L"SOFTWARE\\Microsoft\\EdgeUpdate\\ClientState\\";

  // GUID for the stable release channel.
  static constexpr auto stable_release_guid =
      L"{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

  static constexpr auto default_release_channel_guid = stable_release_guid;
#endif /* WEBVIEW_MSWEBVIEW2_BUILTIN_IMPL */

#if WEBVIEW_MSWEBVIEW2_EXPLICIT_LINK == 1
  native_library m_lib{L"WebView2Loader.dll"};
#endif
};

namespace cast_info {
static constexpr auto controller_completed =
    cast_info_t<ICoreWebView2CreateCoreWebView2ControllerCompletedHandler>{
        IID_ICoreWebView2CreateCoreWebView2ControllerCompletedHandler};

// Not constexpr: the composition completed-handler IID in WebView2.h is a plain
// const IID, which this toolchain will not read inside a constant expression.
// The value is only compared at runtime in QueryInterface, so const suffices.
static const auto composition_controller_completed = cast_info_t<
    ICoreWebView2CreateCoreWebView2CompositionControllerCompletedHandler>{
    IID_ICoreWebView2CreateCoreWebView2CompositionControllerCompletedHandler};

static constexpr auto environment_completed =
    cast_info_t<ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler>{
        IID_ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler};

static constexpr auto message_received =
    cast_info_t<ICoreWebView2WebMessageReceivedEventHandler>{
        IID_ICoreWebView2WebMessageReceivedEventHandler};

static constexpr auto permission_requested =
    cast_info_t<ICoreWebView2PermissionRequestedEventHandler>{
        IID_ICoreWebView2PermissionRequestedEventHandler};
} // namespace cast_info
} // namespace mswebview2

class webview2_com_handler
    : public ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler,
      public ICoreWebView2CreateCoreWebView2ControllerCompletedHandler,
      public ICoreWebView2CreateCoreWebView2CompositionControllerCompletedHandler,
      public ICoreWebView2WebMessageReceivedEventHandler,
      public ICoreWebView2PermissionRequestedEventHandler {
  using webview2_com_handler_cb_t =
      std::function<void(ICoreWebView2Controller *, ICoreWebView2 *webview,
                         ICoreWebView2CompositionController *composition)>;

public:
  webview2_com_handler(HWND hwnd, msg_cb_t msgCb, webview2_com_handler_cb_t cb,
                       bool composited)
      : m_window(hwnd), m_msgCb(msgCb), m_cb(cb), m_composited(composited) {}

  virtual ~webview2_com_handler() = default;
  webview2_com_handler(const webview2_com_handler &other) = delete;
  webview2_com_handler &operator=(const webview2_com_handler &other) = delete;
  webview2_com_handler(webview2_com_handler &&other) = delete;
  webview2_com_handler &operator=(webview2_com_handler &&other) = delete;

  ULONG STDMETHODCALLTYPE AddRef() { return ++m_ref_count; }
  ULONG STDMETHODCALLTYPE Release() {
    if (m_ref_count > 1) {
      return --m_ref_count;
    }
    delete this;
    return 0;
  }
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, LPVOID *ppv) {
    using namespace mswebview2::cast_info;

    if (!ppv) {
      return E_POINTER;
    }

    // All of the COM interfaces we implement should be added here regardless
    // of whether they are required.
    // This is just to be on the safe side in case the WebView2 Runtime ever
    // requests a pointer to an interface we implement.
    // The WebView2 Runtime must at the very least be able to get a pointer to
    // ICoreWebView2CreateCoreWebView2EnvironmentCompletedHandler when we use
    // our custom WebView2 loader implementation, and observations have shown
    // that it is the only interface requested in this case. None have been
    // observed to be requested when using the official WebView2 loader.

    if (cast_if_equal_iid(riid, controller_completed, ppv) ||
        cast_if_equal_iid(riid, composition_controller_completed, ppv) ||
        cast_if_equal_iid(riid, environment_completed, ppv) ||
        cast_if_equal_iid(riid, message_received, ppv) ||
        cast_if_equal_iid(riid, permission_requested, ppv)) {
      return S_OK;
    }

    return E_NOINTERFACE;
  }
  HRESULT STDMETHODCALLTYPE Invoke(HRESULT res, ICoreWebView2Environment *env) {
    if (SUCCEEDED(res)) {
      if (m_composited) {
        // Composition hosting renders into a DirectComposition visual, which
        // the environment exposes through ICoreWebView2Environment3. This is
        // what lets the window present a transparent surface.
        ICoreWebView2Environment3 *env3 = nullptr;
        if (SUCCEEDED(env->QueryInterface(IID_ICoreWebView2Environment3,
                                          reinterpret_cast<void **>(&env3))) &&
            env3) {
          res = env3->CreateCoreWebView2CompositionController(m_window, this);
          env3->Release();
          if (SUCCEEDED(res)) {
            return S_OK;
          }
        }
      } else {
        // Windowed hosting parents the web view in the window and receives
        // input directly, and the window keeps its redirection surface so a
        // native menu bar paints.
        res = env->CreateCoreWebView2Controller(m_window, this);
        if (SUCCEEDED(res)) {
          return S_OK;
        }
      }
    }
    try_create_environment();
    return S_OK;
  }
  // The windowed controller completion is unused under composition hosting but
  // is kept so the interface stays fully implemented.
  HRESULT STDMETHODCALLTYPE Invoke(HRESULT res,
                                   ICoreWebView2Controller *controller) {
    if (FAILED(res)) {
      switch (res) {
      case HRESULT_FROM_WIN32(ERROR_INVALID_STATE):
      case E_ABORT:
        return S_OK;
      }
      try_create_environment();
      return S_OK;
    }
    ICoreWebView2 *webview;
    ::EventRegistrationToken token;
    controller->get_CoreWebView2(&webview);
    webview->add_WebMessageReceived(this, &token);
    webview->add_PermissionRequested(this, &token);
    m_cb(controller, webview, nullptr);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Invoke(
      HRESULT res, ICoreWebView2CompositionController *composition) {
    if (FAILED(res)) {
      // See try_create_environment() regarding
      // HRESULT_FROM_WIN32(ERROR_INVALID_STATE). E_ABORT is reported if the
      // parent window has been destroyed already.
      switch (res) {
      case HRESULT_FROM_WIN32(ERROR_INVALID_STATE):
      case E_ABORT:
        return S_OK;
      }
      try_create_environment();
      return S_OK;
    }

    ICoreWebView2Controller *controller = nullptr;
    composition->QueryInterface(IID_ICoreWebView2Controller,
                                reinterpret_cast<void **>(&controller));
    ICoreWebView2 *webview = nullptr;
    ::EventRegistrationToken token;
    controller->get_CoreWebView2(&webview);
    webview->add_WebMessageReceived(this, &token);
    webview->add_PermissionRequested(this, &token);

    m_cb(controller, webview, composition);
    if (controller) {
      controller->Release();
    }
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Invoke(
      ICoreWebView2 *sender, ICoreWebView2WebMessageReceivedEventArgs *args) {
    LPWSTR message;
    args->TryGetWebMessageAsString(&message);
    m_msgCb(narrow_string(message));
    sender->PostWebMessageAsString(message);

    CoTaskMemFree(message);
    return S_OK;
  }
  HRESULT STDMETHODCALLTYPE Invoke(
      ICoreWebView2 *sender, ICoreWebView2PermissionRequestedEventArgs *args) {
    COREWEBVIEW2_PERMISSION_KIND kind;
    args->get_PermissionKind(&kind);
    if (kind == COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ) {
      args->put_State(COREWEBVIEW2_PERMISSION_STATE_ALLOW);
    }
    return S_OK;
  }

  // Checks whether the specified IID equals the IID of the specified type and
  // if so casts the "this" pointer to T and returns it. Returns nullptr on
  // mismatching IIDs.
  // If ppv is specified then the pointer will also be assigned to *ppv.
  template <typename T>
  T *cast_if_equal_iid(REFIID riid, const cast_info_t<T> &info,
                       LPVOID *ppv = nullptr) noexcept {
    T *ptr = nullptr;
    if (IsEqualIID(riid, info.iid)) {
      ptr = static_cast<T *>(this);
      ptr->AddRef();
    }
    if (ppv) {
      *ppv = ptr;
    }
    return ptr;
  }

  // Set the function that will perform the initiating logic for creating
  // the WebView2 environment.
  void set_attempt_handler(std::function<HRESULT()> attempt_handler) noexcept {
    m_attempt_handler = attempt_handler;
  }

  // Retry creating a WebView2 environment.
  // The initiating logic for creating the environment is defined by the
  // caller of set_attempt_handler().
  void try_create_environment() noexcept {
    // WebView creation fails with HRESULT_FROM_WIN32(ERROR_INVALID_STATE) if
    // a running instance using the same user data folder exists, and the
    // Environment objects have different EnvironmentOptions.
    // Source: https://docs.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/icorewebview2environment?view=webview2-1.0.1150.38
    if (m_attempts < m_max_attempts) {
      ++m_attempts;
      auto res = m_attempt_handler();
      if (SUCCEEDED(res)) {
        return;
      }
      // Not entirely sure if this error code only applies to
      // CreateCoreWebView2Controller so we check here as well.
      if (res == HRESULT_FROM_WIN32(ERROR_INVALID_STATE)) {
        return;
      }
      try_create_environment();
      return;
    }
    // Give up.
    m_cb(nullptr, nullptr, nullptr);
  }

private:
  HWND m_window;
  msg_cb_t m_msgCb;
  webview2_com_handler_cb_t m_cb;
  bool m_composited = false;
  std::atomic<ULONG> m_ref_count{1};
  std::function<HRESULT()> m_attempt_handler;
  unsigned int m_max_attempts = 5;
  unsigned int m_attempts = 0;
};

// Runs a menu keyboard accelerator pressed inside the WebView2. The web content
// runs in a separate process, so accelerator keys never reach the host message
// loop; the controller raises AcceleratorKeyPressed on the host instead. On a
// key-down this forwards the virtual key to the native menu, which runs any
// matching accelerator and reports whether it did, so the event is marked
// handled to suppress the browser default (for example Ctrl+S save-as).
class peko_accel_key_handler
    : public ICoreWebView2AcceleratorKeyPressedEventHandler {
public:
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
    if (!ppv) {
      return E_POINTER;
    }
    if (riid == IID_IUnknown ||
        riid == IID_ICoreWebView2AcceleratorKeyPressedEventHandler) {
      *ppv = static_cast<ICoreWebView2AcceleratorKeyPressedEventHandler *>(this);
      AddRef();
      return S_OK;
    }
    *ppv = nullptr;
    return E_NOINTERFACE;
  }
  ULONG STDMETHODCALLTYPE AddRef() override {
    return ++m_ref_count;
  }
  ULONG STDMETHODCALLTYPE Release() override {
    ULONG count = --m_ref_count;
    if (count == 0) {
      delete this;
    }
    return count;
  }
  HRESULT STDMETHODCALLTYPE
  Invoke(ICoreWebView2Controller *sender,
         ICoreWebView2AcceleratorKeyPressedEventArgs *args) override {
    (void)sender;
    if (!args) {
      return S_OK;
    }
    COREWEBVIEW2_KEY_EVENT_KIND kind;
    UINT vkey = 0;
    if (SUCCEEDED(args->get_KeyEventKind(&kind)) &&
        (kind == COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN ||
         kind == COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN) &&
        SUCCEEDED(args->get_VirtualKey(&vkey))) {
      if (peko_menu_dispatch_accel_key(vkey)) {
        args->put_Handled(TRUE);
      }
    }
    return S_OK;
  }

private:
  std::atomic<ULONG> m_ref_count{1};
};

// Completion handler for AddScriptToExecuteOnDocumentCreated. It records when
// the script registration finishes so init can pump the message loop until the
// script is guaranteed to run on the next navigation.
class peko_add_script_handler
    : public ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler {
public:
  HRESULT STDMETHODCALLTYPE QueryInterface(REFIID riid, void **ppv) override {
    if (!ppv) {
      return E_POINTER;
    }
    if (riid == IID_IUnknown ||
        riid ==
            IID_ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler) {
      *ppv = static_cast<
          ICoreWebView2AddScriptToExecuteOnDocumentCreatedCompletedHandler *>(
          this);
      AddRef();
      return S_OK;
    }
    *ppv = nullptr;
    return E_NOINTERFACE;
  }
  ULONG STDMETHODCALLTYPE AddRef() override {
    return ++m_ref_count;
  }
  ULONG STDMETHODCALLTYPE Release() override {
    ULONG count = --m_ref_count;
    if (count == 0) {
      delete this;
    }
    return count;
  }
  HRESULT STDMETHODCALLTYPE Invoke(HRESULT error_code, LPCWSTR id) override {
    (void)error_code;
    (void)id;
    m_done = true;
    return S_OK;
  }
  bool done() const {
    return m_done;
  }

private:
  std::atomic<ULONG> m_ref_count{1};
  std::atomic<bool> m_done{false};
};

class win32_edge_engine {
public:
  win32_edge_engine(bool debug, void *window) {
    if (!is_webview2_available()) {
      return;
    }
    if (!m_com_init.is_initialized()) {
      return;
    }
    enable_dpi_awareness();
    if (window == nullptr) {
      HINSTANCE hInstance = GetModuleHandle(nullptr);
      HICON icon = (HICON)LoadImage(
          hInstance, IDI_APPLICATION, IMAGE_ICON, GetSystemMetrics(SM_CXICON),
          GetSystemMetrics(SM_CYICON), LR_DEFAULTCOLOR);

      WNDCLASSEXW wc;
      ZeroMemory(&wc, sizeof(WNDCLASSEX));
      wc.cbSize = sizeof(WNDCLASSEX);
      wc.hInstance = hInstance;
      wc.lpszClassName = L"webview";
      wc.hIcon = icon;
      wc.lpfnWndProc =
          (WNDPROC)(+[](HWND hwnd, UINT msg, WPARAM wp, LPARAM lp) -> LRESULT {
            auto w = (win32_edge_engine *)GetWindowLongPtr(hwnd, GWLP_USERDATA);
            switch (msg) {
            case WM_SIZE:
              w->resize(hwnd);
              break;
            case WM_CLOSE:
              DestroyWindow(hwnd);
              break;
            case WM_DESTROY:
              w->terminate();
              break;
            // Composition hosting delivers no input on its own, so mouse
            // messages are forwarded to the web view here.
            case WM_MOUSEMOVE:
            case WM_MOUSELEAVE:
            case WM_LBUTTONDOWN:
            case WM_LBUTTONUP:
            case WM_LBUTTONDBLCLK:
            case WM_RBUTTONDOWN:
            case WM_RBUTTONUP:
            case WM_RBUTTONDBLCLK:
            case WM_MBUTTONDOWN:
            case WM_MBUTTONUP:
            case WM_MBUTTONDBLCLK:
            case WM_XBUTTONDOWN:
            case WM_XBUTTONUP:
            case WM_XBUTTONDBLCLK:
            case WM_MOUSEWHEEL:
            case WM_MOUSEHWHEEL:
              if (w != nullptr) {
                w->forward_mouse(hwnd, msg, wp, lp);
              }
              break;
            case WM_SETCURSOR:
              if (LOWORD(lp) == HTCLIENT && w != nullptr &&
                  w->set_webview_cursor()) {
                return TRUE;
              }
              return DefWindowProcW(hwnd, msg, wp, lp);
            case WM_SETFOCUS:
              if (w != nullptr && w->m_controller != nullptr) {
                w->m_controller->MoveFocus(
                    COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
              }
              break;
            // A frameless window removes its whole non-client area so the
            // content reaches every edge with no titlebar or border strip.
            case WM_NCCALCSIZE:
              if (wp == TRUE && w != nullptr && w->m_frameless) {
                // A maximized frameless window would otherwise overhang the
                // monitor by the frame thickness and cover the taskbar, so its
                // client rect is clamped to the monitor work area.
                if (IsZoomed(hwnd)) {
                  auto *params = reinterpret_cast<NCCALCSIZE_PARAMS *>(lp);
                  HMONITOR monitor =
                      MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
                  MONITORINFO info;
                  info.cbSize = sizeof(info);
                  if (GetMonitorInfoW(monitor, &info)) {
                    params->rgrc[0] = info.rcWork;
                  }
                }
                return 0;
              }
              return DefWindowProcW(hwnd, msg, wp, lp);
            // With no native frame the sizing borders are supplied here: the
            // outer eight pixels report a resize edge, the rest is client.
            case WM_NCHITTEST: {
              if (w == nullptr || !w->m_frameless) {
                return DefWindowProcW(hwnd, msg, wp, lp);
              }
              const int border = 8;
              POINT pt = {GET_X_LPARAM(lp), GET_Y_LPARAM(lp)};
              RECT rc;
              GetWindowRect(hwnd, &rc);
              int col = pt.x < rc.left + border    ? 0
                        : pt.x >= rc.right - border ? 2
                                                    : 1;
              int row = pt.y < rc.top + border      ? 0
                        : pt.y >= rc.bottom - border ? 2
                                                     : 1;
              static const LRESULT cells[3][3] = {
                  {HTTOPLEFT, HTTOP, HTTOPRIGHT},
                  {HTLEFT, HTCLIENT, HTRIGHT},
                  {HTBOTTOMLEFT, HTBOTTOM, HTBOTTOMRIGHT}};
              return cells[row][col];
            }
            case WM_GETMINMAXINFO: {
              auto lpmmi = (LPMINMAXINFO)lp;
              if (w == nullptr) {
                return 0;
              }
              if (w->m_maxsz.x > 0 && w->m_maxsz.y > 0) {
                lpmmi->ptMaxSize = w->m_maxsz;
                lpmmi->ptMaxTrackSize = w->m_maxsz;
              }
              if (w->m_minsz.x > 0 && w->m_minsz.y > 0) {
                lpmmi->ptMinTrackSize = w->m_minsz;
              }
            } break;
            default:
              return DefWindowProcW(hwnd, msg, wp, lp);
            }
            return 0;
          });
      RegisterClassExW(&wc);
      // WS_EX_NOREDIRECTIONBITMAP gives the window no opaque GDI backing, so
      // the DirectComposition content it presents can be per-pixel transparent
      // over the desktop. It also leaves no surface for a native menu bar to
      // paint into, so it is used only when compositing (a transparent window).
      DWORD ex_style = m_composited ? WS_EX_NOREDIRECTIONBITMAP : 0;
      m_window = CreateWindowExW(ex_style, L"webview", L"",
                                 WS_OVERLAPPEDWINDOW, CW_USEDEFAULT,
                                 CW_USEDEFAULT, 640, 480, nullptr, nullptr,
                                 hInstance, nullptr);
      if (m_window == nullptr) {
        return;
      }
      SetWindowLongPtr(m_window, GWLP_USERDATA, (LONG_PTR)this);
    } else {
      m_window = *(static_cast<HWND *>(window));
    }

    // The composition device and root visual must exist before the web view's
    // composition controller is created, since the completion handler binds the
    // controller to the visual. Windowed hosting needs none of this.
    if (m_composited) {
      setup_composition();
    }

    ShowWindow(m_window, SW_SHOW);
    UpdateWindow(m_window);
    SetFocus(m_window);

    auto cb =
        std::bind(&win32_edge_engine::on_message, this, std::placeholders::_1);

    embed(m_window, debug, cb);
    resize(m_window);
    m_controller->MoveFocus(COREWEBVIEW2_MOVE_FOCUS_REASON_PROGRAMMATIC);
  }

  virtual ~win32_edge_engine() {
    if (m_com_handler) {
      m_com_handler->Release();
      m_com_handler = nullptr;
    }
    if (m_webview) {
      m_webview->Release();
      m_webview = nullptr;
    }
    if (m_controller) {
      m_controller->Release();
      m_controller = nullptr;
    }
    if (m_composition_controller) {
      m_composition_controller->Release();
      m_composition_controller = nullptr;
    }
    if (m_root_visual) {
      m_root_visual->Release();
      m_root_visual = nullptr;
    }
    if (m_dcomp_target) {
      m_dcomp_target->Release();
      m_dcomp_target = nullptr;
    }
    if (m_dcomp_device) {
      m_dcomp_device->Release();
      m_dcomp_device = nullptr;
    }
    if (m_d3d_device) {
      m_d3d_device->Release();
      m_d3d_device = nullptr;
    }
  }

  win32_edge_engine(const win32_edge_engine &other) = delete;
  win32_edge_engine &operator=(const win32_edge_engine &other) = delete;
  win32_edge_engine(win32_edge_engine &&other) = delete;
  win32_edge_engine &operator=(win32_edge_engine &&other) = delete;

  void run() {
    MSG msg;
    BOOL res;
    while ((res = GetMessage(&msg, nullptr, 0, 0)) != -1) {
      if (msg.hwnd) {
        // A menu accelerator consumes the message and dispatches its command.
        if (peko_menu_translate_accel(&msg)) {
          continue;
        }
        TranslateMessage(&msg);
        DispatchMessage(&msg);
        continue;
      }
      if (msg.message == WM_APP) {
        auto f = (dispatch_fn_t *)(msg.lParam);
        (*f)();
        delete f;
      } else if (msg.message == WM_QUIT) {
        return;
      }
    }
  }
  void *window() { return (void *)m_window; }
  // The WebView2 controller, exposed so the Peko chrome helpers can set the
  // default background color for transparency.
  void *controller() { return (void *)m_controller; }
  // Toggle the native window frame off for a custom titlebar.
  void set_frameless(bool frameless) { m_frameless = frameless; }
  void terminate() { PostQuitMessage(0); }
  void dispatch(dispatch_fn_t f) {
    PostThreadMessage(m_main_thread, WM_APP, 0, (LPARAM) new dispatch_fn_t(f));
  }

  void set_title(const std::string &title) {
    SetWindowTextW(m_window, widen_string(title).c_str());
  }

  void set_size(int width, int height, int hints) {
    auto style = GetWindowLong(m_window, GWL_STYLE);
    if (hints == WEBVIEW_HINT_FIXED) {
      style &= ~(WS_THICKFRAME | WS_MAXIMIZEBOX);
    } else {
      style |= (WS_THICKFRAME | WS_MAXIMIZEBOX);
    }
    SetWindowLong(m_window, GWL_STYLE, style);

    if (hints == WEBVIEW_HINT_MAX) {
      m_maxsz.x = width;
      m_maxsz.y = height;
    } else if (hints == WEBVIEW_HINT_MIN) {
      m_minsz.x = width;
      m_minsz.y = height;
    } else {
      RECT r;
      r.left = r.top = 0;
      r.right = width;
      r.bottom = height;
      AdjustWindowRect(&r, WS_OVERLAPPEDWINDOW, 0);
      SetWindowPos(
          m_window, nullptr, r.left, r.top, r.right - r.left, r.bottom - r.top,
          SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE | SWP_FRAMECHANGED);
      resize(m_window);
    }
  }

  void navigate(const std::string &url) {
    auto wurl = widen_string(url);
    m_webview->Navigate(wurl.c_str());
  }

  void init(const std::string &js) {
    auto wjs = widen_string(js);
    // Register the boot script and pump the message loop until registration
    // finishes, so a Navigate that follows applies the script to the first
    // document. This matches the synchronous WKUserScript path on macOS.
    // Without the wait the async registration can lose the race, leaving
    // window.__PEKO__ undefined when the page's own scripts run, which disables
    // the devtools console and points the bridge socket at the wrong origin.
    auto *handler = new peko_add_script_handler();
    m_webview->AddScriptToExecuteOnDocumentCreated(wjs.c_str(), handler);
    MSG msg = {};
    while (!handler->done() && GetMessage(&msg, nullptr, 0, 0)) {
      TranslateMessage(&msg);
      DispatchMessage(&msg);
    }
    handler->Release();
  }

  void eval(const std::string &js) {
    auto wjs = widen_string(js);
    m_webview->ExecuteScript(wjs.c_str(), nullptr);
  }

  void set_html(const std::string &html) {
    m_webview->NavigateToString(widen_string(html).c_str());
  }

private:
  bool embed(HWND wnd, bool debug, msg_cb_t cb) {
    std::atomic_flag flag = ATOMIC_FLAG_INIT;
    flag.test_and_set();

    wchar_t currentExePath[MAX_PATH];
    GetModuleFileNameW(nullptr, currentExePath, MAX_PATH);
    wchar_t *currentExeName = PathFindFileNameW(currentExePath);

    wchar_t dataPath[MAX_PATH];
    if (!SUCCEEDED(
            SHGetFolderPathW(nullptr, CSIDL_APPDATA, nullptr, 0, dataPath))) {
      return false;
    }
    wchar_t userDataFolder[MAX_PATH];
    PathCombineW(userDataFolder, dataPath, currentExeName);

    m_com_handler = new webview2_com_handler(
        wnd, cb,
        [&](ICoreWebView2Controller *controller, ICoreWebView2 *webview,
            ICoreWebView2CompositionController *composition) {
          if (!controller || !webview) {
            flag.clear();
            return;
          }
          controller->AddRef();
          webview->AddRef();
          m_controller = controller;
          m_webview = webview;
          // Forward accelerator keys pressed in the web content to the native
          // menu, since they do not reach the host message loop.
          {
            auto *accel = new peko_accel_key_handler();
            ::EventRegistrationToken accel_token;
            controller->add_AcceleratorKeyPressed(accel, &accel_token);
            accel->Release();
          }
          if (composition) {
            composition->AddRef();
            m_composition_controller = composition;
            // Present the web view through the composition visual so the
            // window can show a transparent surface.
            composition->put_RootVisualTarget(m_root_visual);
            if (m_dcomp_device) {
              m_dcomp_device->Commit();
            }
          }
          flag.clear();
        },
        m_composited);

    m_com_handler->set_attempt_handler([&] {
      return m_webview2_loader.create_environment_with_options(
          nullptr, userDataFolder, nullptr, m_com_handler);
    });
    m_com_handler->try_create_environment();

    MSG msg = {};
    while (flag.test_and_set() && GetMessage(&msg, nullptr, 0, 0)) {
      TranslateMessage(&msg);
      DispatchMessage(&msg);
    }
    if (!m_controller || !m_webview) {
      return false;
    }
    ICoreWebView2Settings *settings = nullptr;
    auto res = m_webview->get_Settings(&settings);
    if (res != S_OK) {
      return false;
    }
    res = settings->put_AreDevToolsEnabled(debug ? TRUE : FALSE);
    if (res != S_OK) {
      return false;
    }
    init("window.external={invoke:s=>window.chrome.webview.postMessage(s)}");
    return true;
  }

  void resize(HWND wnd) {
    if (m_controller == nullptr) {
      return;
    }
    RECT bounds = peko_client_bounds_physical(wnd);
    m_controller->put_Bounds(bounds);
  }

  // Build the DirectComposition device, a target bound to the window, and a
  // root visual the web view controller presents into.
  void setup_composition() {
    D3D_FEATURE_LEVEL feature_level;
    HRESULT res = D3D11CreateDevice(
        nullptr, D3D_DRIVER_TYPE_HARDWARE, nullptr,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0, D3D11_SDK_VERSION,
        &m_d3d_device, &feature_level, nullptr);
    if (FAILED(res)) {
      // Fall back to the WARP software renderer when no hardware device is
      // available, so the window still composes.
      res = D3D11CreateDevice(nullptr, D3D_DRIVER_TYPE_WARP, nullptr,
                              D3D11_CREATE_DEVICE_BGRA_SUPPORT, nullptr, 0,
                              D3D11_SDK_VERSION, &m_d3d_device, &feature_level,
                              nullptr);
      if (FAILED(res)) {
        return;
      }
    }
    IDXGIDevice *dxgi_device = nullptr;
    if (FAILED(m_d3d_device->QueryInterface(
            __uuidof(IDXGIDevice), reinterpret_cast<void **>(&dxgi_device)))) {
      return;
    }
    res = DCompositionCreateDevice(dxgi_device, __uuidof(IDCompositionDevice),
                                   reinterpret_cast<void **>(&m_dcomp_device));
    dxgi_device->Release();
    if (FAILED(res) || m_dcomp_device == nullptr) {
      return;
    }
    m_dcomp_device->CreateTargetForHwnd(m_window, TRUE, &m_dcomp_target);
    m_dcomp_device->CreateVisual(&m_root_visual);
    if (m_dcomp_target && m_root_visual) {
      m_dcomp_target->SetRoot(m_root_visual);
    }
    m_dcomp_device->Commit();
  }

  // Forward a window mouse message to the web view. The mouse event kinds match
  // the WM_ message codes, so the message is passed through directly. Wheel
  // messages carry screen coordinates and the wheel delta in the high word.
  void forward_mouse(HWND wnd, UINT msg, WPARAM wp, LPARAM lp) {
    if (m_composition_controller == nullptr) {
      return;
    }
    if (msg == WM_MOUSELEAVE) {
      m_composition_controller->SendMouseInput(
          COREWEBVIEW2_MOUSE_EVENT_KIND_LEAVE,
          static_cast<COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS>(0), 0, POINT{0, 0});
      m_mouse_tracked = false;
      return;
    }
    if (msg == WM_MOUSEMOVE && !m_mouse_tracked) {
      TRACKMOUSEEVENT tme;
      tme.cbSize = sizeof(tme);
      tme.dwFlags = TME_LEAVE;
      tme.hwndTrack = wnd;
      tme.dwHoverTime = 0;
      TrackMouseEvent(&tme);
      m_mouse_tracked = true;
    }
    POINT point{GET_X_LPARAM(lp), GET_Y_LPARAM(lp)};
    UINT32 mouse_data = 0;
    if (msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL) {
      ScreenToClient(wnd, &point);
      mouse_data = static_cast<UINT32>(GET_WHEEL_DELTA_WPARAM(wp));
    } else if (msg == WM_XBUTTONDOWN || msg == WM_XBUTTONUP ||
               msg == WM_XBUTTONDBLCLK) {
      mouse_data = static_cast<UINT32>(GET_XBUTTON_WPARAM(wp));
    }
    auto keys = static_cast<COREWEBVIEW2_MOUSE_EVENT_VIRTUAL_KEYS>(
        GET_KEYSTATE_WPARAM(wp));
    m_composition_controller->SendMouseInput(
        static_cast<COREWEBVIEW2_MOUSE_EVENT_KIND>(msg), keys, mouse_data,
        point);
  }

  // Apply the cursor the web view requests for the area under the pointer.
  bool set_webview_cursor() {
    if (m_composition_controller == nullptr) {
      return false;
    }
    HCURSOR cursor = nullptr;
    if (SUCCEEDED(m_composition_controller->get_Cursor(&cursor)) && cursor) {
      SetCursor(cursor);
      return true;
    }
    return false;
  }

  bool is_webview2_available() const noexcept {
    LPWSTR version_info = nullptr;
    auto res = m_webview2_loader.get_available_browser_version_string(
        nullptr, &version_info);
    // The result will be equal to HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
    // if the WebView2 runtime is not installed.
    auto ok = SUCCEEDED(res) && version_info;
    if (version_info) {
      CoTaskMemFree(version_info);
    }
    return ok;
  }

  virtual void on_message(const std::string &msg) = 0;

  // The app is expected to call CoInitializeEx before
  // CreateCoreWebView2EnvironmentWithOptions.
  // Source: https://docs.microsoft.com/en-us/microsoft-edge/webview2/reference/win32/webview2-idl#createcorewebview2environmentwithoptions
  com_init_wrapper m_com_init{COINIT_APARTMENTTHREADED};
  HWND m_window = nullptr;
  POINT m_minsz = POINT{0, 0};
  POINT m_maxsz = POINT{0, 0};
  DWORD m_main_thread = GetCurrentThreadId();
  ICoreWebView2 *m_webview = nullptr;
  ICoreWebView2Controller *m_controller = nullptr;
  ICoreWebView2CompositionController *m_composition_controller = nullptr;
  webview2_com_handler *m_com_handler = nullptr;
  mswebview2::loader m_webview2_loader;
  // DirectComposition presents the web view visual so the window can show a
  // per-pixel transparent surface over the desktop.
  ID3D11Device *m_d3d_device = nullptr;
  IDCompositionDevice *m_dcomp_device = nullptr;
  IDCompositionTarget *m_dcomp_target = nullptr;
  IDCompositionVisual *m_root_visual = nullptr;
  bool m_mouse_tracked = false;
  // When set, the window draws no native frame; the content fills every edge
  // and the sizing borders are provided through hit-testing.
  bool m_frameless = false;
  // Composition hosting (DirectComposition + WS_EX_NOREDIRECTIONBITMAP) lets the
  // window present a per-pixel transparent surface. The pekoui desktop model is a
  // frameless, transparent window with an HTML (not native) menu on Windows, so
  // composition is the default. set_transparent then toggles only the default
  // background color. (A window created before this default is applied cannot opt
  // in later, which is why the transparent surface never appeared when this was
  // false.)
  bool m_composited = true;
};

} // namespace detail

using browser_engine = detail::win32_edge_engine;

} // namespace webview

#endif /* WEBVIEW_GTK, WEBVIEW_COCOA, WEBVIEW_EDGE */

namespace webview {

class webview : public browser_engine {
public:
  webview(bool debug = false, void *wnd = nullptr)
      : browser_engine(debug, wnd) {}

  void navigate(const std::string &url) {
    if (url.empty()) {
      browser_engine::navigate("about:blank");
      return;
    }
    browser_engine::navigate(url);
  }

  using binding_t = std::function<void(std::string, std::string, void *)>;
  class binding_ctx_t {
  public:
    binding_ctx_t(binding_t callback, void *arg)
        : callback(callback), arg(arg) {}
    // This function is called upon execution of the bound JS function
    binding_t callback;
    // This user-supplied argument is passed to the callback
    void *arg;
  };

  using sync_binding_t = std::function<std::string(std::string)>;

  // Synchronous bind
  void bind(const std::string &name, sync_binding_t fn) {
    auto wrapper = [this, fn](const std::string &seq, const std::string &req,
                              void * /*arg*/) { resolve(seq, 0, fn(req)); };
    bind(name, wrapper, nullptr);
  }

  // Asynchronous bind
  void bind(const std::string &name, binding_t fn, void *arg) {
    if (bindings.count(name) > 0) {
      return;
    }
    bindings.emplace(name, binding_ctx_t(fn, arg));
    auto js = "(function() { var name = '" + name + "';" + R""(
      var RPC = window._rpc = (window._rpc || {nextSeq: 1});
      window[name] = function() {
        var seq = RPC.nextSeq++;
        var promise = new Promise(function(resolve, reject) {
          RPC[seq] = {
            resolve: resolve,
            reject: reject,
          };
        });
        window.external.invoke(JSON.stringify({
          id: seq,
          method: name,
          params: Array.prototype.slice.call(arguments),
        }));
        return promise;
      }
    })())"";
    init(js);
    eval(js);
  }

  void unbind(const std::string &name) {
    auto found = bindings.find(name);
    if (found != bindings.end()) {
      auto js = "delete window['" + name + "'];";
      init(js);
      eval(js);
      bindings.erase(found);
    }
  }

  void resolve(const std::string &seq, int status, const std::string &result) {
    dispatch([seq, status, result, this]() {
      if (status == 0) {
        eval("window._rpc[" + seq + "].resolve(" + result +
             "); delete window._rpc[" + seq + "]");
      } else {
        eval("window._rpc[" + seq + "].reject(" + result +
             "); delete window._rpc[" + seq + "]");
      }
    });
  }

private:
  void on_message(const std::string &msg) {
    auto seq = detail::json_parse(msg, "id", 0);
    auto name = detail::json_parse(msg, "method", 0);
    auto args = detail::json_parse(msg, "params", 0);
    auto found = bindings.find(name);
    if (found == bindings.end()) {
      return;
    }
    const auto &context = found->second;
    context.callback(seq, args, context.arg);
  }

  std::map<std::string, binding_ctx_t> bindings;
};
} // namespace webview

WEBVIEW_API webview_t webview_create(int debug, void *wnd) {
  // The dev loop sets PEKO_DEVTOOLS. Enable the webview inspector then even when
  // the app requests debug off, so the running app's web contents can be
  // inspected during development.
  if (!debug) {
    const char *devtools = getenv("PEKO_DEVTOOLS");
    if (devtools != nullptr && devtools[0] != '\0') {
      debug = 1;
    }
  }
  auto w = new webview::webview(debug, wnd);
  if (!w->window()) {
    delete w;
    return nullptr;
  }
  return w;
}

WEBVIEW_API void webview_destroy(webview_t w) {
  delete static_cast<webview::webview *>(w);
}

WEBVIEW_API void webview_run(webview_t w) {
  static_cast<webview::webview *>(w)->run();
}

WEBVIEW_API void webview_terminate(webview_t w) {
  static_cast<webview::webview *>(w)->terminate();
}

WEBVIEW_API void webview_dispatch(webview_t w, void (*fn)(webview_t, void *),
                                  void *arg) {
  static_cast<webview::webview *>(w)->dispatch([=]() { fn(w, arg); });
}

WEBVIEW_API void *webview_get_window(webview_t w) {
  return static_cast<webview::webview *>(w)->window();
}

WEBVIEW_API void *webview_get_controller(webview_t w) {
#if defined(_WIN32)
  return static_cast<webview::webview *>(w)->controller();
#else
  (void)w;
  return nullptr;
#endif
}

WEBVIEW_API void webview_set_title(webview_t w, const char *title) {
  static_cast<webview::webview *>(w)->set_title(title);
}

WEBVIEW_API void webview_set_size(webview_t w, int width, int height,
                                  int hints) {
  static_cast<webview::webview *>(w)->set_size(width, height, hints);
}

WEBVIEW_API void webview_navigate(webview_t w, const char *url) {
  static_cast<webview::webview *>(w)->navigate(url);
}

WEBVIEW_API void webview_set_html(webview_t w, const char *html) {
  static_cast<webview::webview *>(w)->set_html(html);
}

WEBVIEW_API void webview_init(webview_t w, const char *js) {
  static_cast<webview::webview *>(w)->init(js);
}

WEBVIEW_API void webview_eval(webview_t w, const char *js) {
  static_cast<webview::webview *>(w)->eval(js);
}

WEBVIEW_API void webview_bind(webview_t w, const char *name,
                              void (*fn)(const char *seq, const char *req,
                                         void *arg),
                              void *arg) {
  static_cast<webview::webview *>(w)->bind(
      name,
      [=](const std::string &seq, const std::string &req, void *arg) {
        fn(seq.c_str(), req.c_str(), arg);
      },
      arg);
}

WEBVIEW_API void webview_unbind(webview_t w, const char *name) {
  static_cast<webview::webview *>(w)->unbind(name);
}

WEBVIEW_API void webview_return(webview_t w, const char *seq, int status,
                                const char *result) {
  static_cast<webview::webview *>(w)->resolve(seq, status, result);
}

WEBVIEW_API const webview_version_info_t *webview_version() {
  return &webview::detail::library_version_info;
}

// ---------------------------------------------------------------------------
// Peko desktop chrome: transparency, custom titlebar, and native window drag.
// These are opt-in and only affect desktop backends. A backend without an
// implementation keeps the same signatures as no-ops so the API is uniform.
// ---------------------------------------------------------------------------

#if defined(WEBVIEW_COCOA)

// Bring the objc::msg_send helper and the _cls / _sel / _str literals into
// scope for the functions below.
using namespace webview::detail;

namespace {
// NSWindowStyleMaskFullSizeContentView extends content under the titlebar.
constexpr NSUInteger peko_ns_full_size_content_view = 1u << 15;
// NSWindowTitleHidden.
constexpr NSInteger peko_ns_title_hidden = 1;
// Event masks for the window-move loop. A mask is 1 shifted by the event type
// (LeftMouseUp = 2, MouseMoved = 5, LeftMouseDragged = 6).
constexpr NSUInteger peko_ns_mask_left_mouse_up = 1u << 2;
constexpr NSUInteger peko_ns_mask_mouse_moved = 1u << 5;
constexpr NSUInteger peko_ns_mask_left_mouse_dragged = 1u << 6;
} // namespace

extern "C" void peko_webview_set_transparent(webview_t w, int transparent) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  id content = objc::msg_send<id>(window, "contentView"_sel);
  // The window and web view are opaque and draw a background unless the caller
  // asks for transparency.
  BOOL opaque = transparent ? NO : YES;
  objc::msg_send<void>(window, "setOpaque:"_sel, opaque);
  id color =
      transparent ? objc::msg_send<id>("NSColor"_cls, "clearColor"_sel)
                  : objc::msg_send<id>("NSColor"_cls, "windowBackgroundColor"_sel);
  objc::msg_send<void>(window, "setBackgroundColor:"_sel, color);
  // WKWebView exposes drawsBackground only through KVC on macOS.
  id number = objc::msg_send<id>("NSNumber"_cls, "numberWithBool:"_sel, opaque);
  objc::msg_send<void>(content, "setValue:forKey:"_sel, number,
                       "drawsBackground"_str);
}

extern "C" void peko_webview_set_decorations(webview_t w, int decorated) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  auto mask = objc::msg_send<NSUInteger>(window, "styleMask"_sel);
  if (decorated) {
    mask &= ~peko_ns_full_size_content_view;
    objc::msg_send<void>(window, "setStyleMask:"_sel, mask);
    objc::msg_send<void>(window, "setTitlebarAppearsTransparent:"_sel, NO);
    objc::msg_send<void>(window, "setTitleVisibility:"_sel, (NSInteger)0);
  } else {
    mask |= peko_ns_full_size_content_view;
    objc::msg_send<void>(window, "setStyleMask:"_sel, mask);
    objc::msg_send<void>(window, "setTitlebarAppearsTransparent:"_sel, YES);
    objc::msg_send<void>(window, "setTitleVisibility:"_sel, peko_ns_title_hidden);
  }
}

// Vertically center the three traffic-light buttons in a titlebar of the given
// height. The buttons live inside a titlebar view inside an
// NSTitlebarContainerView; the container is grown to the height and each button
// re-centered. AppKit re-lays these out on every window layout, so this is
// re-applied from notification observers rather than once.
static void peko_center_window_buttons(id window, double height) {
  id close = objc::msg_send<id>(window, "standardWindowButton:"_sel, (NSInteger)0);
  if (!close) {
    return;
  }
  id title_view = objc::msg_send<id>(close, "superview"_sel);
  if (!title_view) {
    return;
  }
  // The buttons sit in a fixed-height titlebar view (about 28pt) at the window
  // top. Rather than resize that view, which AppKit reverts on layout, move each
  // button down within it so its center lands at height/2 from the window top.
  // For a 16pt button in a 28pt view targeting a 40pt strip this is y = 0, which
  // stays inside the view (no clipping).
  double base = objc::msg_send<CGRect>(title_view, "frame"_sel).size.height;
  // The titlebar view may be flipped (origin top-left, y grows down) or not
  // (origin bottom-left, y grows up). Center the button so its middle sits at
  // height/2 from the window top in either coordinate system.
  BOOL flipped = objc::msg_send<BOOL>(title_view, "isFlipped"_sel);

  CGRect close_frame = objc::msg_send<CGRect>(close, "frame"_sel);
  double button_size = close_frame.size.height;
  double target = flipped ? (height / 2.0 - button_size / 2.0)
                          : (base - height / 2.0 - button_size / 2.0);
  double delta = close_frame.origin.y - target;
  if (delta < 0) {
    delta = -delta;
  }
  if (delta < 0.5) {
    return;
  }

  for (NSInteger index = 0; index < 3; index++) {
    id button = objc::msg_send<id>(window, "standardWindowButton:"_sel, index);
    if (!button) {
      continue;
    }
    CGRect button_frame = objc::msg_send<CGRect>(button, "frame"_sel);
    button_frame.origin.y = flipped
                                ? (height / 2.0 - button_frame.size.height / 2.0)
                                : (base - height / 2.0 - button_frame.size.height / 2.0);
    objc::msg_send<void>(button, "setFrameOrigin:"_sel, button_frame.origin);
  }
}

extern "C" void peko_webview_set_titlebar_height(webview_t w, double height) {
  id window = (id)webview_get_window(w);
  if (!window || height <= 0) {
    return;
  }
  peko_center_window_buttons(window, height);

  // The window is not on screen yet when this is called, and AppKit re-lays out
  // the buttons when it displays and on every resize. Re-center from layout
  // notifications so the centering sticks. The notification center copies the
  // block; it captures the window and height by value.
  id center =
      objc::msg_send<id>("NSNotificationCenter"_cls, "defaultCenter"_sel);
  void (^apply)(id) = ^(id note) {
    (void)note;
    peko_center_window_buttons(window, height);
  };
  objc::msg_send<id>(center, "addObserverForName:object:queue:usingBlock:"_sel,
                     "NSWindowDidResizeNotification"_str, window, (id)nullptr,
                     (id)apply);
  objc::msg_send<id>(center, "addObserverForName:object:queue:usingBlock:"_sel,
                     "NSWindowDidBecomeKeyNotification"_str, window,
                     (id)nullptr, (id)apply);
  objc::msg_send<id>(center, "addObserverForName:object:queue:usingBlock:"_sel,
                     "NSWindowDidExposeNotification"_str, window, (id)nullptr,
                     (id)apply);

  // The most reliable trigger: watch the close button's own frame. AppKit moves
  // it back on every titlebar layout (including the async one after the web view
  // loads its content, which fires no window notification). Re-centering on the
  // button's frame change keeps it centered through those layouts; the guard in
  // peko_center_window_buttons prevents an update loop.
  id close = objc::msg_send<id>(window, "standardWindowButton:"_sel, (NSInteger)0);
  if (close) {
    objc::msg_send<void>(close, "setPostsFrameChangedNotifications:"_sel, YES);
    objc::msg_send<id>(center,
                       "addObserverForName:object:queue:usingBlock:"_sel,
                       "NSViewFrameDidChangeNotification"_str, close,
                       (id)nullptr, (id)apply);
  }
}

extern "C" void peko_webview_begin_drag(webview_t w) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  // Follow the live cursor to move the window. The DOM drag shim posts this
  // asynchronously, so the originating mouse event is gone, which rules out
  // performWindowDragWithEvent. The web view also captures the mouse events, so
  // a loop that moves only on dequeued drag events never advances. Reading the
  // global cursor position each frame sidesteps both: it repositions the window
  // to track the cursor until the button is released. The guard makes a message
  // that arrives after a plain click return at once.
  if ((objc::msg_send<NSUInteger>("NSEvent"_cls, "pressedMouseButtons"_sel) &
       1u) == 0) {
    return;
  }

  id app = objc::msg_send<id>("NSApplication"_cls, "sharedApplication"_sel);
  CGPoint start = objc::msg_send<CGPoint>("NSEvent"_cls, "mouseLocation"_sel);
  CGRect frame = objc::msg_send<CGRect>(window, "frame"_sel);
  double origin_x = frame.origin.x;
  double origin_y = frame.origin.y;
  id mode = "NSEventTrackingRunLoopMode"_str;
  NSUInteger mask = peko_ns_mask_left_mouse_up | peko_ns_mask_mouse_moved |
                    peko_ns_mask_left_mouse_dragged;

  while ((objc::msg_send<NSUInteger>("NSEvent"_cls, "pressedMouseButtons"_sel) &
          1u) != 0) {
    CGPoint now = objc::msg_send<CGPoint>("NSEvent"_cls, "mouseLocation"_sel);
    objc::msg_send<void>(
        window, "setFrameOrigin:"_sel,
        CGPointMake(origin_x + (now.x - start.x), origin_y + (now.y - start.y)));
    // Yield about 8 ms and drain queued mouse input so the web view does not
    // also act on the drag.
    id until = objc::msg_send<id>("NSDate"_cls,
                                  "dateWithTimeIntervalSinceNow:"_sel, 0.008);
    objc::msg_send<id>(app, "nextEventMatchingMask:untilDate:inMode:dequeue:"_sel,
                       mask, until, mode, YES);
  }
}

extern "C" void peko_webview_minimize(webview_t w) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  objc::msg_send<void>(window, "miniaturize:"_sel, (id) nullptr);
}

extern "C" void peko_webview_maximize(webview_t w) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  // zoom toggles between the standard and zoomed frame.
  objc::msg_send<void>(window, "zoom:"_sel, (id) nullptr);
}

extern "C" void peko_webview_close(webview_t w) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  objc::msg_send<void>(window, "close"_sel);
}

extern "C" void peko_webview_activate(webview_t w) {
  // Bring the app in front of other apps and raise the window, so a newly
  // spawned instance opens frontmost rather than behind the launching app.
  auto app = objc::msg_send<id>("NSApplication"_cls, "sharedApplication"_sel);
  objc::msg_send<void>(app, "activateIgnoringOtherApps:"_sel, YES);
  id window = (id)webview_get_window(w);
  if (window) {
    objc::msg_send<void>(window, "makeKeyAndOrderFront:"_sel, (id) nullptr);
    objc::msg_send<void>(window, "orderFrontRegardless"_sel);
  }
}

extern "C" void peko_webview_set_window_buttons_hidden(webview_t w, int hidden) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return;
  }
  BOOL is_hidden = hidden ? YES : NO;
  // The traffic-light controls are the close, miniaturize, and zoom buttons
  // (NSWindowButton 0, 1, 2). macOS keeps them at the top left even on a
  // frameless window; hiding them lets the web UI draw its own controls.
  for (NSInteger which = 0; which <= 2; which++) {
    id button =
        objc::msg_send<id>(window, "standardWindowButton:"_sel, which);
    if (button) {
      objc::msg_send<void>(button, "setHidden:"_sel, is_hidden);
    }
  }
}

extern "C" int peko_webview_has_native_window_controls(webview_t w) {
  id window = (id)webview_get_window(w);
  if (!window) {
    return 0;
  }
  // The window draws native controls when the close button is present and not
  // hidden. A frameless macOS window keeps the traffic lights unless they are
  // hidden explicitly.
  id button =
      objc::msg_send<id>(window, "standardWindowButton:"_sel, (NSInteger)0);
  if (!button) {
    return 0;
  }
  BOOL is_hidden = objc::msg_send<BOOL>(button, "isHidden"_sel);
  return is_hidden ? 0 : 1;
}

#elif defined(WEBVIEW_GTK)

extern "C" void peko_webview_set_transparent(webview_t w, int transparent) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  GtkWidget *child = gtk_bin_get_child(GTK_BIN(window));
  if (child && WEBKIT_IS_WEB_VIEW(child)) {
    GdkRGBA color;
    color.red = 0;
    color.green = 0;
    color.blue = 0;
    color.alpha = transparent ? 0.0 : 1.0;
    webkit_web_view_set_background_color(WEBKIT_WEB_VIEW(child), &color);
  }
  // An RGBA visual plus an app-paintable window lets the transparent web view
  // show what is behind it. This needs a running compositor to take effect.
  if (transparent) {
    GdkScreen *screen = gtk_widget_get_screen(window);
    GdkVisual *visual = gdk_screen_get_rgba_visual(screen);
    if (visual) {
      gtk_widget_set_visual(window, visual);
    }
  }
  gtk_widget_set_app_paintable(window, transparent ? TRUE : FALSE);
}

extern "C" void peko_webview_set_titlebar_height(webview_t w, double height) {
  // The traffic-light layout is macOS specific; no-op elsewhere.
  (void)w;
  (void)height;
}

extern "C" void peko_webview_set_decorations(webview_t w, int decorated) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  gtk_window_set_decorated(GTK_WINDOW(window), decorated ? TRUE : FALSE);
}

extern "C" void peko_webview_begin_drag(webview_t w) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  // gtk_window_begin_move_drag drives the native move on both X11 and Wayland.
  // The pointer position and button come from the seat's current pointer.
  GdkDisplay *display = gtk_widget_get_display(window);
  GdkSeat *seat = gdk_display_get_default_seat(display);
  GdkDevice *pointer = gdk_seat_get_pointer(seat);
  gint x = 0;
  gint y = 0;
  gdk_device_get_position(pointer, nullptr, &x, &y);
  gtk_window_begin_move_drag(GTK_WINDOW(window), 1, x, y, GDK_CURRENT_TIME);
}

extern "C" void peko_webview_minimize(webview_t w) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  gtk_window_iconify(GTK_WINDOW(window));
}

extern "C" void peko_webview_maximize(webview_t w) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  if (gtk_window_is_maximized(GTK_WINDOW(window))) {
    gtk_window_unmaximize(GTK_WINDOW(window));
  } else {
    gtk_window_maximize(GTK_WINDOW(window));
  }
}

extern "C" void peko_webview_close(webview_t w) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  gtk_window_close(GTK_WINDOW(window));
}

extern "C" void peko_webview_activate(webview_t w) {
  auto *window = static_cast<GtkWidget *>(webview_get_window(w));
  if (!window) {
    return;
  }
  // present raises the window and gives it focus above other applications.
  gtk_window_present(GTK_WINDOW(window));
}

extern "C" void peko_webview_set_window_buttons_hidden(webview_t w, int hidden) {
  // A frameless GTK window has no native controls to hide.
  (void)w;
  (void)hidden;
}

extern "C" int peko_webview_has_native_window_controls(webview_t w) {
  // A frameless GTK window draws no native controls over its content, so the
  // web UI provides them.
  (void)w;
  return 0;
}

#elif defined(WEBVIEW_EDGE)

#include <dwmapi.h>

// The Win11 backdrop attribute and values, defined here for SDKs that predate
// them so the call degrades to a no-op rather than failing to build.
#ifndef DWMWA_SYSTEMBACKDROP_TYPE
#define DWMWA_SYSTEMBACKDROP_TYPE 38
#endif
#ifndef DWMSBT_NONE
#define DWMSBT_NONE 1
#endif
#ifndef DWMSBT_MAINWINDOW
#define DWMSBT_MAINWINDOW 2
#endif

extern "C" void peko_webview_set_transparent(webview_t w, int transparent) {
  // The window presents through a DirectComposition visual with no opaque
  // backing, so a fully transparent default background composites the page
  // over the desktop per pixel. CSS rgba panels then read as translucent
  // glass. An opaque default paints solid white behind the page.
  auto *engine = static_cast<webview::webview *>(w);
  auto *controller =
      static_cast<ICoreWebView2Controller *>(engine->controller());
  if (!controller) {
    return;
  }
  ICoreWebView2Controller2 *controller2 = nullptr;
  if (SUCCEEDED(controller->QueryInterface(__uuidof(ICoreWebView2Controller2),
                                           (void **)&controller2)) &&
      controller2) {
    COREWEBVIEW2_COLOR color;
    color.A = transparent ? 0 : 255;
    color.R = 255;
    color.G = 255;
    color.B = 255;
    controller2->put_DefaultBackgroundColor(color);
    controller2->Release();
  }
}

extern "C" void peko_webview_set_titlebar_height(webview_t w, double height) {
  // The traffic-light layout is macOS specific; no-op elsewhere.
  (void)w;
  (void)height;
}

extern "C" void peko_webview_set_decorations(webview_t w, int decorated) {
  auto *engine = static_cast<webview::webview *>(w);
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (!hwnd) {
    return;
  }
  // Framelessness is applied through WM_NCCALCSIZE, which drops the entire
  // non-client area, so no titlebar and no border strip remain. The window
  // keeps its overlapped style so snapping and animations still work; the
  // sizing borders come back through WM_NCHITTEST.
  engine->set_frameless(decorated == 0);
  SetWindowPos(hwnd, nullptr, 0, 0, 0, 0,
               SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER |
                   SWP_NOACTIVATE);
  // The frame change resizes the client area but does not raise WM_SIZE, so the
  // web view is refit to the new client rect here to cover the whole window.
  auto *controller =
      static_cast<ICoreWebView2Controller *>(engine->controller());
  if (controller) {
    RECT bounds = webview::detail::peko_client_bounds_physical(hwnd);
    controller->put_Bounds(bounds);
  }
}

extern "C" void peko_webview_begin_drag(webview_t w) {
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (!hwnd) {
    return;
  }
  // The DOM shim posts on mouse-down while the button is held, so handing the
  // window a caption non-client click starts the native move loop.
  ReleaseCapture();
  SendMessageW(hwnd, WM_NCLBUTTONDOWN, HTCAPTION, 0);
}

extern "C" void peko_webview_minimize(webview_t w) {
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (hwnd) {
    ShowWindow(hwnd, SW_MINIMIZE);
  }
}

extern "C" void peko_webview_maximize(webview_t w) {
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (!hwnd) {
    return;
  }
  // Toggle between maximized and restored.
  ShowWindow(hwnd, IsZoomed(hwnd) ? SW_RESTORE : SW_MAXIMIZE);
}

extern "C" void peko_webview_close(webview_t w) {
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (hwnd) {
    PostMessageW(hwnd, WM_CLOSE, 0, 0);
  }
}

extern "C" void peko_webview_activate(webview_t w) {
  HWND hwnd = static_cast<HWND>(webview_get_window(w));
  if (!hwnd) {
    return;
  }
  // Raise a newly spawned instance above the launching window.
  ShowWindow(hwnd, SW_SHOW);
  BringWindowToTop(hwnd);
  SetForegroundWindow(hwnd);
}

extern "C" void peko_webview_set_window_buttons_hidden(webview_t w, int hidden) {
  // A frameless Win32 window drops its non-client area, so there are no native
  // controls to hide.
  (void)w;
  (void)hidden;
}

extern "C" int peko_webview_has_native_window_controls(webview_t w) {
  // A frameless Win32 window draws no native caption buttons over its content,
  // so the web UI provides the window controls.
  (void)w;
  return 0;
}

#endif

#endif /* WEBVIEW_HEADER */
#endif /* __cplusplus */