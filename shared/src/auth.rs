//! Pure validation rules for usernames/passwords, shared by the client
//! (instant UI feedback before ever sending a `RegisterMsg`/`LoginMsg`) and
//! the server (the actual authority — see `server::auth`). Deliberately
//! simple: this is a two-friend hobby server, not a public service, so the
//! bar is "reject obviously-empty/malformed input," not password
//! complexity rules.

const USERNAME_MIN_LEN: usize = 3;
const USERNAME_MAX_LEN: usize = 20;
const PASSWORD_MIN_LEN: usize = 6;

/// `Ok(())` or a short, user-facing reason.
pub fn validate_username(username: &str) -> Result<(), &'static str> {
    if username.chars().count() < USERNAME_MIN_LEN {
        return Err("username must be at least 3 characters");
    }
    if username.chars().count() > USERNAME_MAX_LEN {
        return Err("username must be at most 20 characters");
    }
    if !username.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err("username may only contain letters, numbers, and underscores");
    }
    Ok(())
}

/// `Ok(())` or a short, user-facing reason. Only a minimum length — this
/// isn't an enterprise system, and complexity rules mostly just annoy two
/// friends trying to set up an account together.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < PASSWORD_MIN_LEN {
        return Err("password must be at least 6 characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_username() {
        assert!(validate_username("dan_sont").is_ok());
    }

    #[test]
    fn rejects_a_too_short_username() {
        assert!(validate_username("ab").is_err());
    }

    #[test]
    fn rejects_a_too_long_username() {
        assert!(validate_username(&"a".repeat(21)).is_err());
    }

    #[test]
    fn accepts_the_maximum_length_username() {
        assert!(validate_username(&"a".repeat(20)).is_ok());
    }

    #[test]
    fn rejects_non_alphanumeric_characters() {
        assert!(validate_username("dan sont").is_err());
        assert!(validate_username("dan@sont").is_err());
    }

    #[test]
    fn accepts_a_normal_password() {
        assert!(validate_password("hunter22").is_ok());
    }

    #[test]
    fn rejects_a_too_short_password() {
        assert!(validate_password("short").is_err());
    }

    #[test]
    fn accepts_the_minimum_length_password() {
        assert!(validate_password("123456").is_ok());
    }
}
