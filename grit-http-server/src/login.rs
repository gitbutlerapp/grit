//! Sign-ins from `grit auth <url>` that wait for the user to approve them in
//! the browser.
//!
//! The command line asks for a login ([`PendingLogins::start`]) and polls with
//! the secret device code it gets back ([`PendingLogins::poll`]). The user
//! opens the authorize page with the short user code, signs in, and approves
//! ([`PendingLogins::approve`]) or declines ([`PendingLogins::deny`]). The next
//! poll after an approval hands the command line the approving user, once; the
//! server then creates the token.
//!
//! Logins live only in memory: a restart drops them, and the command line just
//! starts over.

use std::time::{Duration, Instant};

use grit_lib::remote::grit_server_auth::{LoginPoll, LoginStart};

use crate::store::User;

/// How long a login waits for approval.
pub const LOGIN_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Seconds the command line waits between polls.
pub const POLL_INTERVAL_SECS: u64 = 2;

/// Most logins waiting at once, so anonymous requests cannot grow memory
/// without bound.
pub const MAX_PENDING: usize = 256;

/// Letters of user codes: capitals and digits without `0`, `1`, `I` and `O`.
const USER_CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Why a login could not be started.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    /// [`MAX_PENDING`] logins are already waiting.
    #[error("too many sign-ins are waiting; try again in a few minutes")]
    TooMany,
    /// The operating system could not supply random bytes for the codes.
    #[error("cannot generate a sign-in code: {0}")]
    Random(getrandom::Error),
}

/// What a poll found, before the server turns an approval into a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// Not approved yet.
    Pending,
    /// Approved by this user; the login is now used up.
    Approved(User),
    /// Declined; the login is now used up.
    Denied,
    /// Expired, used up, or never started.
    Expired,
}

impl PollOutcome {
    /// The wire answer for every outcome except an approval, which needs a token.
    #[must_use]
    pub fn without_grant(&self) -> Option<LoginPoll> {
        match self {
            Self::Pending => Some(LoginPoll::Pending),
            Self::Approved(_) => None,
            Self::Denied => Some(LoginPoll::Denied),
            Self::Expired => Some(LoginPoll::Expired),
        }
    }
}

#[derive(Debug)]
enum State {
    Waiting,
    Approved(User),
    Denied,
}

#[derive(Debug)]
struct Pending {
    device_code: String,
    user_code: String,
    expires: Instant,
    state: State,
}

/// The logins waiting for approval.
#[derive(Debug, Default)]
pub struct PendingLogins {
    pending: Vec<Pending>,
}

impl PendingLogins {
    /// Start a login at `now`.
    ///
    /// # Errors
    ///
    /// [`LoginError::TooMany`] when [`MAX_PENDING`] logins are waiting, and
    /// [`LoginError::Random`] when the codes cannot be generated.
    pub fn start(&mut self, now: Instant) -> Result<LoginStart, LoginError> {
        self.prune(now);
        if self.pending.len() >= MAX_PENDING {
            return Err(LoginError::TooMany);
        }
        let mut secret = [0u8; 32];
        getrandom::getrandom(&mut secret).map_err(LoginError::Random)?;
        let device_code = hex::encode(secret);
        let user_code = loop {
            let code = user_code()?;
            if !self.pending.iter().any(|p| p.user_code == code) {
                break code;
            }
        };
        self.pending.push(Pending {
            device_code: device_code.clone(),
            user_code: user_code.clone(),
            expires: now + LOGIN_LIFETIME,
            state: State::Waiting,
        });
        Ok(LoginStart {
            device_code,
            user_code,
            interval: POLL_INTERVAL_SECS,
            expires_in: LOGIN_LIFETIME.as_secs(),
        })
    }

    /// Whether `user_code` names a login still waiting for approval at `now`.
    #[must_use]
    pub fn is_waiting(&self, user_code: &str, now: Instant) -> bool {
        let code = normalize_user_code(user_code);
        self.pending
            .iter()
            .any(|p| p.user_code == code && p.expires > now && matches!(p.state, State::Waiting))
    }

    /// Approve the waiting login `user_code` for `user`. Returns `false` when
    /// there is no such login waiting.
    pub fn approve(&mut self, user_code: &str, user: User, now: Instant) -> bool {
        self.decide(user_code, now, State::Approved(user))
    }

    /// Decline the waiting login `user_code`. Returns `false` when there is no
    /// such login waiting.
    pub fn deny(&mut self, user_code: &str, now: Instant) -> bool {
        self.decide(user_code, now, State::Denied)
    }

    /// Answer a poll for `device_code` at `now`. An approval or a refusal is
    /// reported once; the login is then forgotten.
    pub fn poll(&mut self, device_code: &str, now: Instant) -> PollOutcome {
        self.prune(now);
        let Some(index) = self
            .pending
            .iter()
            .position(|p| p.device_code == device_code)
        else {
            return PollOutcome::Expired;
        };
        if matches!(self.pending[index].state, State::Waiting) {
            return PollOutcome::Pending;
        }
        match self.pending.swap_remove(index).state {
            State::Waiting => PollOutcome::Pending,
            State::Approved(user) => PollOutcome::Approved(user),
            State::Denied => PollOutcome::Denied,
        }
    }

    fn decide(&mut self, user_code: &str, now: Instant, decision: State) -> bool {
        self.prune(now);
        let code = normalize_user_code(user_code);
        let Some(login) = self
            .pending
            .iter_mut()
            .find(|p| p.user_code == code && matches!(p.state, State::Waiting))
        else {
            return false;
        };
        login.state = decision;
        true
    }

    fn prune(&mut self, now: Instant) {
        self.pending.retain(|p| p.expires > now);
    }
}

/// Uppercase a user code and drop spaces, so `abcd-2345 ` matches `ABCD-2345`.
fn normalize_user_code(code: &str) -> String {
    code.trim().to_ascii_uppercase()
}

/// A random user code like `ABCD-2345` (40 bits).
fn user_code() -> Result<String, LoginError> {
    let alphabet = USER_CODE_ALPHABET.len();
    let mut chars = String::with_capacity(9);
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).map_err(LoginError::Random)?;
    for (i, byte) in bytes.iter().enumerate() {
        if i == 4 {
            chars.push('-');
        }
        // 256 is a multiple of the 32-letter alphabet, so this is uniform.
        chars.push(char::from(
            USER_CODE_ALPHABET[usize::from(*byte) % alphabet],
        ));
    }
    Ok(chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bob() -> User {
        User {
            id: 7,
            email: "bob@example.com".to_owned(),
            admin: false,
            must_change_password: false,
        }
    }

    fn started(logins: &mut PendingLogins, now: Instant) -> LoginStart {
        match logins.start(now) {
            Ok(login) => login,
            Err(e) => panic!("start login: {e}"),
        }
    }

    #[test]
    fn an_approved_login_is_handed_out_once() {
        let now = Instant::now();
        let mut logins = PendingLogins::default();
        let login = started(&mut logins, now);
        assert_eq!(login.user_code.len(), 9);
        assert_eq!(login.device_code.len(), 64);
        assert_eq!(login.interval, POLL_INTERVAL_SECS);

        assert_eq!(logins.poll(&login.device_code, now), PollOutcome::Pending);
        assert!(logins.is_waiting(&login.user_code.to_lowercase(), now));
        assert!(logins.approve(&login.user_code, bob(), now));
        assert!(!logins.is_waiting(&login.user_code, now));
        assert!(
            !logins.approve(&login.user_code, bob(), now),
            "decided once"
        );
        assert_eq!(
            logins.poll(&login.device_code, now),
            PollOutcome::Approved(bob())
        );
        assert_eq!(logins.poll(&login.device_code, now), PollOutcome::Expired);
    }

    #[test]
    fn denied_unknown_and_expired_logins() {
        let now = Instant::now();
        let mut logins = PendingLogins::default();
        let denied = started(&mut logins, now);
        assert!(logins.deny(&denied.user_code, now));
        assert_eq!(logins.poll(&denied.device_code, now), PollOutcome::Denied);
        assert_eq!(logins.poll("nope", now), PollOutcome::Expired);

        let old = started(&mut logins, now);
        let later = now + LOGIN_LIFETIME;
        assert!(!logins.is_waiting(&old.user_code, later));
        assert!(!logins.approve(&old.user_code, bob(), later));
        assert_eq!(logins.poll(&old.device_code, later), PollOutcome::Expired);
    }

    #[test]
    fn waiting_logins_are_capped() {
        let now = Instant::now();
        let mut logins = PendingLogins::default();
        for _ in 0..MAX_PENDING {
            started(&mut logins, now);
        }
        assert!(matches!(logins.start(now), Err(LoginError::TooMany)));
        assert!(
            logins.start(now + LOGIN_LIFETIME).is_ok(),
            "expired ones free up room"
        );
    }

    #[test]
    fn outcomes_map_to_wire_answers() {
        assert_eq!(
            PollOutcome::Pending.without_grant(),
            Some(LoginPoll::Pending)
        );
        assert_eq!(PollOutcome::Denied.without_grant(), Some(LoginPoll::Denied));
        assert_eq!(
            PollOutcome::Expired.without_grant(),
            Some(LoginPoll::Expired)
        );
        assert_eq!(PollOutcome::Approved(bob()).without_grant(), None);
    }
}
