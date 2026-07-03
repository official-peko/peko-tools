//! `peko whoami`: print the identity behind the stored CLI session.

use std::process::ExitCode;

use crate::auth;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `whoami` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let Some(session) = auth::Session::load() else {
        reporter.error("not logged in");
        reporter.help(format!("run '{} login' to authenticate", cli_info.executable));
        return ExitCode::FAILURE;
    };

    let base = auth::platform_base(cli_info.flags.get_flag("base"));
    match auth::current_user(&base, &session).await {
        Ok(user) => {
            reporter.info(format!("uid:   {}", user.uid));
            if let Some(email) = &user.email {
                reporter.info(format!("email: {email}"));
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
            reporter.error("session expired or revoked");
            reporter.help(format!("run '{} login' to authenticate again", cli_info.executable));
            ExitCode::FAILURE
        }
        Err(e) => {
            reporter.error(format!("could not read identity: {e}"));
            ExitCode::FAILURE
        }
    }
}
