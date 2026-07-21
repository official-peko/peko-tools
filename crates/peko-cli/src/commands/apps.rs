//! `peko apps`: list and inspect the platform apps the session owns.
//!
//! Backs the link-and-deploy flow: `list` is what a project picks from when it
//! has no `app_id` yet, and `show` resolves an already-linked id to its name and
//! capabilities. Both are read-only; linking itself is `peko link`, a local edit
//! to `peko.toml`.
//!
//! In `--json` mode a signed-out session is a normal result rather than an
//! error, matching `peko whoami`. The IDE drives its sign-in state off that
//! field instead of parsing an error, so it never has to distinguish "not signed
//! in" from "the call failed".

use std::process::ExitCode;

use serde::Deserialize;

use crate::auth;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// One app in the platform's projection. Fields absent for an app's
/// capabilities are omitted by the platform rather than nulled, so everything
/// past the identity is optional.
#[derive(Debug, Deserialize, serde::Serialize)]
struct App {
    id: String,
    slug: Option<String>,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(default)]
    capabilities: Capabilities,
    status: Option<String>,
    #[serde(rename = "statusReason")]
    status_reason: Option<String>,
    /// The SSR framework, set only when `capabilities.server`.
    framework: Option<String>,
    /// The native bundle id, present only when `capabilities.distribution`.
    #[serde(rename = "bundleId")]
    bundle_id: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at: Option<String>,
}

/// What an app is allowed to do. Fixed when the app is created on the platform,
/// so a project linked to an app without a capability can never deploy that way
/// and the caller should say so rather than let the deploy fail late.
#[derive(Debug, Default, Deserialize, serde::Serialize)]
struct Capabilities {
    /// Has an SSR backend served by the platform (`peko deploy server`).
    #[serde(default)]
    server: bool,
    /// Ships native binaries to app stores (`peko deploy app`).
    #[serde(default)]
    distribution: bool,
}

/// The `GET /api/apps` envelope.
#[derive(Debug, Deserialize)]
struct AppsResponse {
    #[serde(default)]
    apps: Vec<App>,
    /// Opaque cursor for the next page, when the account has more than `limit`.
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

impl App {
    /// The name to show a user: the display name, else the slug, else the id.
    fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(self.slug.as_deref().filter(|s| !s.is_empty()))
            .unwrap_or(&self.id)
    }

    /// A compact description of what this app can be deployed as.
    fn capability_summary(&self) -> String {
        match (self.capabilities.server, self.capabilities.distribution) {
            (true, true) => "server, distribution".to_owned(),
            (true, false) => "server".to_owned(),
            (false, true) => "distribution".to_owned(),
            (false, false) => "no deploy capabilities".to_owned(),
        }
    }
}

/// Execute the `apps` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    match cli_info.arguments.get(1).map(String::as_str) {
        Some("list") | None => list(cli_info, reporter).await,
        Some("show") => show(cli_info, reporter).await,
        Some(other) => {
            reporter.error(format!("no such subcommand '{other}' for 'apps' command"));
            reporter.help(format!(
                "run '{} help apps' to see a list of valid subcommands",
                cli_info.executable
            ));
            ExitCode::FAILURE
        }
    }
}

/// `peko apps list` — every app the session owns.
async fn list(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let json = reporter.is_json();
    let Some(token) = id_token(cli_info, reporter, json).await else {
        return signed_out_result(json);
    };
    let base = auth::platform_base(cli_info.flags.get_flag("base"));

    let mut url = format!("{base}/api/apps");
    if let Some(limit) = cli_info.flags.get_flag("limit") {
        url.push_str(&format!("?limit={limit}"));
    }

    let response = match fetch(&url, &token).await {
        Ok(response) => response,
        Err(e) => return report_failure(reporter, json, e),
    };
    if !response.status().is_success() {
        return report_platform_failure(reporter, json, response).await;
    }
    let body: AppsResponse = match response.json().await {
        Ok(body) => body,
        Err(e) => return report_failure(reporter, json, format!("could not read the app list: {e}")),
    };

    if json {
        println!(
            "{}",
            serde_json::json!({
                "authenticated": true,
                "apps": body.apps,
                "nextCursor": body.next_cursor,
            })
        );
        return ExitCode::SUCCESS;
    }

    if body.apps.is_empty() {
        reporter.info("this account owns no apps");
        reporter.help(format!("create one at {base}"));
        return ExitCode::SUCCESS;
    }
    for app in &body.apps {
        reporter.info(format!(
            "{}  {}  ({})",
            app.id,
            app.label(),
            app.capability_summary()
        ));
    }
    if body.next_cursor.is_some() {
        reporter.info("more apps are available; raise --limit to see them");
    }
    ExitCode::SUCCESS
}

/// `peko apps show <app-id>` — one app, for resolving an existing link.
async fn show(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let json = reporter.is_json();
    let Some(app_id) = cli_info.arguments.get(2).map(String::as_str) else {
        reporter.error("`apps show` needs an app id");
        reporter.help(format!("usage: {} apps show <app-id>", cli_info.executable));
        return ExitCode::FAILURE;
    };
    let Some(token) = id_token(cli_info, reporter, json).await else {
        return signed_out_result(json);
    };
    let base = auth::platform_base(cli_info.flags.get_flag("base"));

    let response = match fetch(&format!("{base}/api/apps/{app_id}"), &token).await {
        Ok(response) => response,
        Err(e) => return report_failure(reporter, json, e),
    };

    // 403 and 404 mean different things for a linked project: the app exists but
    // belongs to another account, versus it is gone. The caller re-links in one
    // case and switches account in the other, so they are not collapsed.
    let status = response.status();
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::FORBIDDEN {
        let reason = if status == reqwest::StatusCode::NOT_FOUND {
            "not_found"
        } else {
            "forbidden"
        };
        if json {
            println!(
                "{}",
                serde_json::json!({ "authenticated": true, "app": null, "reason": reason })
            );
            return ExitCode::SUCCESS;
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            reporter.error(format!("no app {app_id} exists"));
            reporter.help("it may have been deleted; link the project to another app");
        } else {
            reporter.error(format!("app {app_id} belongs to another account"));
            reporter.help("sign in as the owner, or link the project to one of your apps");
        }
        return ExitCode::FAILURE;
    }
    if !status.is_success() {
        return report_platform_failure(reporter, json, response).await;
    }

    let app: App = match response.json().await {
        Ok(app) => app,
        Err(e) => return report_failure(reporter, json, format!("could not read the app: {e}")),
    };
    if json {
        println!(
            "{}",
            serde_json::json!({ "authenticated": true, "app": app })
        );
        return ExitCode::SUCCESS;
    }
    reporter.info(format!("id:           {}", app.id));
    reporter.info(format!("name:         {}", app.label()));
    reporter.info(format!("capabilities: {}", app.capability_summary()));
    if let Some(status) = &app.status {
        reporter.info(format!("status:       {status}"));
    }
    if let Some(reason) = &app.status_reason {
        reporter.info(format!("reason:       {reason}"));
    }
    if let Some(framework) = &app.framework {
        reporter.info(format!("framework:    {framework}"));
    }
    if let Some(bundle_id) = &app.bundle_id {
        reporter.info(format!("bundle id:    {bundle_id}"));
    }
    ExitCode::SUCCESS
}

/// A fresh ID token for the stored session, or `None` when signed out. The
/// caller decides how to report that, since it is a normal state in JSON mode.
async fn id_token(cli_info: &CLIInfo, reporter: &Reporter, json: bool) -> Option<String> {
    let session = auth::Session::load()?;
    match auth::fresh_id_token(&session).await {
        Ok(token) => Some(token),
        Err(e) => {
            if !json {
                reporter.error(format!("could not refresh the session: {e}"));
                reporter.help(format!("run '{} login' to authenticate again", cli_info.executable));
            }
            None
        }
    }
}

/// GET `url` with the bearer token. Sends no `Origin` and no App Check header:
/// the route's same-origin assertion passes only when `Origin` is absent, and a
/// valid bearer already exempts the call from attestation.
async fn fetch(url: &str, token: &str) -> Result<reqwest::Response, String> {
    let http = reqwest::Client::builder()
        .user_agent(concat!("peko-cli/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("could not build the HTTP client: {e}"))?;
    http.get(url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("could not reach the platform: {e}"))
}

/// The signed-out result: success with `authenticated: false` in JSON mode so
/// the IDE can render a sign-in prompt, a failure otherwise.
fn signed_out_result(json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::json!({ "authenticated": false }));
        return ExitCode::SUCCESS;
    }
    ExitCode::FAILURE
}

/// Report a local failure (network, decode) in whichever mode is active.
fn report_failure(reporter: &Reporter, json: bool, message: impl Into<String>) -> ExitCode {
    let message = message.into();
    if json {
        println!("{}", serde_json::json!({ "error": message }));
    } else {
        reporter.error(message);
    }
    ExitCode::FAILURE
}

/// Report a platform error response through the shared explainer, which handles
/// the legal gate and the App Check flavoured 401.
async fn report_platform_failure(
    reporter: &Reporter,
    json: bool,
    response: reqwest::Response,
) -> ExitCode {
    let status = response.status().as_u16();
    let failure = auth::explain_failure(response).await;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "authenticated": status != 401,
                "error": failure.message,
                "status": status,
            })
        );
        return ExitCode::FAILURE;
    }
    reporter.error(failure.message);
    if let Some(help) = failure.help {
        reporter.help(help);
    }
    ExitCode::FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform omits fields an app's capabilities do not apply to, so the
    /// projection must survive everything past the id being absent.
    #[test]
    fn parses_a_minimal_app() {
        let app: App = serde_json::from_str(r#"{"id":"abc123"}"#).unwrap();
        assert_eq!(app.id, "abc123");
        assert_eq!(app.label(), "abc123");
        assert!(!app.capabilities.server);
        assert!(!app.capabilities.distribution);
        assert_eq!(app.capability_summary(), "no deploy capabilities");
    }

    #[test]
    fn parses_a_full_app() {
        let app: App = serde_json::from_str(
            r#"{"id":"jspD8Y7Klm5kYRcZ4uXI","slug":"todossr","displayName":"todossr",
                "capabilities":{"server":true,"distribution":true},
                "status":"live","statusReason":null,"framework":"next",
                "bundleId":"com.example.todo","createdAt":"2026-07-10T00:17:18Z",
                "updatedAt":"2026-07-18T12:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(app.label(), "todossr");
        assert_eq!(app.capability_summary(), "server, distribution");
        assert_eq!(app.framework.as_deref(), Some("next"));
        assert_eq!(app.bundle_id.as_deref(), Some("com.example.todo"));
    }

    /// The label falls back rather than showing an empty string, which a blank
    /// display name would otherwise produce in the picker.
    #[test]
    fn label_falls_back_through_slug_to_id() {
        let blank: App =
            serde_json::from_str(r#"{"id":"abc","slug":"the-slug","displayName":""}"#).unwrap();
        assert_eq!(blank.label(), "the-slug");
        let no_names: App = serde_json::from_str(r#"{"id":"abc","slug":""}"#).unwrap();
        assert_eq!(no_names.label(), "abc");
    }

    /// Each capability decides which deploy a caller may offer, so the single
    /// capability cases must not read as both or neither.
    #[test]
    fn capability_summary_covers_each_combination() {
        let one_of = |server, distribution| App {
            id: "x".into(),
            slug: None,
            display_name: None,
            capabilities: Capabilities {
                server,
                distribution,
            },
            status: None,
            status_reason: None,
            framework: None,
            bundle_id: None,
            created_at: None,
            updated_at: None,
        };
        assert_eq!(one_of(true, false).capability_summary(), "server");
        assert_eq!(one_of(false, true).capability_summary(), "distribution");
    }

    /// An empty account is a normal result, not an error, and must not be
    /// confused with a failed call.
    #[test]
    fn parses_an_empty_and_a_paged_list() {
        let empty: AppsResponse = serde_json::from_str(r#"{"apps":[]}"#).unwrap();
        assert!(empty.apps.is_empty());
        assert!(empty.next_cursor.is_none());

        let paged: AppsResponse =
            serde_json::from_str(r#"{"apps":[{"id":"a"}],"nextCursor":"tok"}"#).unwrap();
        assert_eq!(paged.apps.len(), 1);
        assert_eq!(paged.next_cursor.as_deref(), Some("tok"));
    }
}
