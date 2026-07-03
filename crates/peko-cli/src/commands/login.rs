//! `peko login`: authenticate the CLI with the Peko platform.
//!
//! Runs the loopback device flow in `crate::auth`, stores the session in the
//! operating system keychain, and confirms the signed-in identity.

use std::process::ExitCode;

use crate::auth;
use crate::cli::CLIInfo;
use crate::cli::reporting::Reporter;

/// Execute the `login` subcommand.
pub async fn execute(cli_info: &CLIInfo, reporter: &Reporter) -> ExitCode {
    let base = auth::platform_base(cli_info.flags.get_flag("base"));

    let session = match auth::login(&base, reporter).await {
        Ok(session) => session,
        Err(e) => {
            reporter.error(format!("login failed: {e}"));
            return ExitCode::FAILURE;
        }
    };

    // The session is stored. Reading the identity is a courtesy confirmation, so
    // a failure here does not undo a successful login.
    match auth::current_user(&base, &session).await {
        Ok(user) => reporter.success(format!("logged in as {}", describe_user(&user))),
        Err(_) => reporter.success("logged in"),
    }
    ExitCode::SUCCESS
}

/// Format an identity for the confirmation line.
fn describe_user(user: &auth::User) -> String {
    let who = user
        .email
        .clone()
        .unwrap_or_else(|| user.uid.clone());
    match &user.role {
        Some(role) => format!("{who} ({role})"),
        None => who,
    }
}
