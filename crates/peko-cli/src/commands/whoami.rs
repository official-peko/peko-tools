//! `peko whoami`: print the identity behind the stored CLI session.

use std::process::ExitCode;

use crate::auth;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `whoami` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    // In JSON mode a single identity object is printed for tooling (the IDE). A
    // signed-out state is a normal result there, not an error.
    let json = reporter.is_json();

    let Some(session) = auth::Session::load() else {
        if json {
            print_identity_json(&serde_json::json!({ "authenticated": false }));
            return ExitCode::SUCCESS;
        }
        reporter.error("not logged in");
        reporter.help(format!("run '{} login' to authenticate", cli_info.executable));
        return ExitCode::FAILURE;
    };

    let base = auth::platform_base(cli_info.flags.get_flag("base"));
    match auth::current_user(&base, &session).await {
        Ok(user) => {
            if json {
                print_identity_json(&serde_json::json!({
                    "authenticated": true,
                    "uid": user.uid,
                    "email": user.email,
                    "emailVerified": user.email_verified,
                    "name": user.display_name,
                    "photoUrl": user.photo_url,
                    "role": user.role,
                    "tier": user.tier,
                }));
                return ExitCode::SUCCESS;
            }
            reporter.info(format!("uid:   {}", user.uid));
            if let Some(email) = &user.email {
                reporter.info(format!("email: {email}"));
            }
            if user.email_verified == Some(false) {
                reporter.warning("email not verified — required to publish packages");
            }
            if let Some(name) = &user.display_name {
                reporter.info(format!("name:  {name}"));
            }
            if let Some(role) = &user.role {
                reporter.info(format!("role:  {role}"));
            }
            if let Some(tier) = &user.tier {
                reporter.info(format!("tier:  {tier}"));
            }
            ExitCode::SUCCESS
        }
        Err(auth::AuthError::Unauthorized) => {
            if json {
                print_identity_json(&serde_json::json!({ "authenticated": false }));
                return ExitCode::SUCCESS;
            }
            reporter.error("session expired or revoked");
            reporter.help(format!("run '{} login' to authenticate again", cli_info.executable));
            ExitCode::FAILURE
        }
        Err(e) => {
            if json {
                print_identity_json(&serde_json::json!({ "authenticated": false }));
                return ExitCode::SUCCESS;
            }
            reporter.error(format!("could not read identity: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// Print the identity object as one line of JSON on stdout.
fn print_identity_json(value: &serde_json::Value) {
    println!("{value}");
}
