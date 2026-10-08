//! Default identity values from config and the system (Git `ident.c`).

use crate::config::ConfigSet;
use crate::ident_resolve::IdentityEnv;

/// The real user id of the calling process (Unix `getuid`); `0` elsewhere.
#[must_use]
pub fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        nix::unistd::getuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Git `ident_default_name()` for this merged config: `user.name` if that key was ever set
/// (including to `""`), otherwise the passwd short name (Unix) or `USER` / `"unknown"`.
#[must_use]
pub fn ident_default_name<E: IdentityEnv>(config: &ConfigSet, env: &E) -> String {
    if config.get_last_entry("user.name").is_some() {
        config
            .get("user.name")
            .map(|s| s.trim().to_owned())
            .unwrap_or_default()
    } else {
        passwd_short_username(env)
    }
}

fn env_user_fallback<E: IdentityEnv>(env: &E) -> String {
    env.var("USER")
        .or_else(|| env.var("USERNAME"))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(unix)]
fn passwd_short_username<E: IdentityEnv>(env: &E) -> String {
    // `User::from_uid` wraps `getpwuid_r`; the short name is its `pw_name`.
    match nix::unistd::User::from_uid(nix::unistd::Uid::current()) {
        Ok(Some(user)) => user.name,
        _ => env_user_fallback(env),
    }
}

#[cfg(not(unix))]
fn passwd_short_username<E: IdentityEnv>(env: &E) -> String {
    env_user_fallback(env)
}
