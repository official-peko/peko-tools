//! `peko bridge token`: mint a native-bridge token for the current app.
//!
//! Wraps `POST /api/bridge/token` (owner-authed) for the dev device / manual use.
//! A shipped app does NOT use this — its server mints tokens with the app's bridge
//! credential, which the platform auto-provisions and injects as `PEKO_BRIDGE_KEY`
//! on every `deploy server` (the CLI does nothing for credentials).

use std::process::ExitCode;

use crate::bridge::{BridgeTokenError, request_bridge_token};
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;
use crate::project::PekoProject;

/// Execute the `bridge` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(target) = cli_info.arguments.get(1) else {
        reporter.error("`bridge` requires a subcommand");
        reporter.help(format!(
            "run '{} bridge token' to mint a device bridge token",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    match target.as_str() {
        "token" => token(cli_info, reporter).await,
        other => {
            reporter.error(format!("unknown bridge subcommand '{other}'"));
            reporter.help("the only subcommand is 'token'");
            ExitCode::FAILURE
        }
    }
}

/// Mint a bridge token for the linked app and print it.
async fn token(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // The platform app id: `--app-id`, else [project].app_id from peko.toml.
    let app_id = match cli_info
        .flags
        .get_flag("app-id")
        .or_else(|| PekoProject::from_current_directory().ok().and_then(|p| p.app_id))
    {
        Some(id) if !id.is_empty() => id,
        _ => {
            reporter.error("this project is not linked to a platform app");
            reporter.help(
                "set the app id under [project].app_id in peko.toml (via `peko link`), \
                 or pass --app-id <id>",
            );
            return ExitCode::FAILURE;
        }
    };
    // On refresh the runtime passes the device it was assigned, to keep the same
    // identity; omitted on the first pair so the platform assigns one.
    let device_id = cli_info.flags.get_flag("device-id");

    let Some(session) = crate::auth::Session::load() else {
        reporter.error("not logged in");
        reporter.help(format!(
            "run '{} login' to authenticate first",
            cli_info.executable
        ));
        return ExitCode::FAILURE;
    };
    let base = crate::auth::platform_base(cli_info.flags.get_flag("base"));

    let id_token = match crate::auth::fresh_id_token(&session).await {
        Ok(token) => token,
        Err(crate::auth::AuthError::Unauthorized) => {
            reporter.error("session expired or revoked");
            reporter.help(format!("run '{} login' again", cli_info.executable));
            return ExitCode::FAILURE;
        }
        Err(e) => {
            reporter.error(format!("could not authenticate: {e}"));
            return ExitCode::FAILURE;
        }
    };

    match request_bridge_token(&base, &id_token, &app_id, device_id.as_deref()).await {
        Ok(minted) => {
            if reporter.is_json() {
                // Machine output on stdout for the runtime to parse.
                println!(
                    "{}",
                    serde_json::json!({
                        "token": minted.token,
                        "deviceId": minted.device_id,
                        "expiresIn": minted.expires_in,
                    })
                );
            } else {
                reporter.success(format!(
                    "minted a bridge token for device {} (expires in {}s)",
                    minted.device_id, minted.expires_in
                ));
                reporter.raw(minted.token);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            match e {
                BridgeTokenError::Forbidden(message) => {
                    reporter.error(message);
                    reporter.help(
                        "verify your email from your account page on the Peko web app, then retry",
                    );
                }
                BridgeTokenError::Unauthorized => {
                    reporter.error("the platform could not verify this session");
                    reporter.help(format!("run '{} login' again", cli_info.executable));
                }
                BridgeTokenError::NotFound => {
                    reporter.error("that app was not found on your account");
                    reporter.help("check [project].app_id and that you own the app");
                }
                BridgeTokenError::NotConfigured => {
                    reporter.error("the native bridge is not available on the platform yet");
                }
                BridgeTokenError::RateLimited => {
                    reporter.error("rate limited; try again shortly");
                }
                BridgeTokenError::BadRequest(message) => reporter.error(message),
                other => reporter.error(format!("could not mint a bridge token: {other}")),
            }
            ExitCode::FAILURE
        }
    }
}
