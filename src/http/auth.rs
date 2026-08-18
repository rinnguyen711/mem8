//! Bearer-token authentication for the HTTP transport.
//!
//! Over stdio there is nothing to authenticate: the agent spawned mem8 as a
//! child process, and the operating system already decided who may do that.
//! Over HTTP every request arrives from an unauthenticated stranger, so the
//! token is the only thing standing between the network and every memory the
//! server holds.

use std::sync::Arc;

/// Shortest token the server will accept.
///
/// Not a strength requirement — no length makes a guessable token safe — but a
/// backstop against the specific accident of a placeholder like `"x"` or
/// `"test"` reaching a running server. Refusing at startup is visible; a weak
/// token silently protecting nothing is not.
pub const MIN_TOKEN_LEN: usize = 16;

/// The configured bearer token.
///
/// Wrapped rather than passed as a bare `String` so it cannot be printed by
/// accident: `Debug` prints a placeholder, and there is no `Display`.
#[derive(Clone)]
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub struct Token(Arc<String>);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the secret, even into a log line written in a panic.
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Build a token, rejecting one too short to be a secret.
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let trimmed = value.trim();

        if trimmed.is_empty() {
            return Err("the authentication token is empty".to_string());
        }
        if trimmed.len() < MIN_TOKEN_LEN {
            return Err(format!(
                "the authentication token is {} characters; at least {MIN_TOKEN_LEN} are \
                 required. Generate one with `openssl rand -hex 32`.",
                trimmed.len()
            ));
        }

        Ok(Self(Arc::new(trimmed.to_string())))
    }

    /// Whether a presented token matches, compared in constant time.
    ///
    /// `==` on strings returns as soon as two bytes differ, so the time taken
    /// reveals how many leading bytes were correct — enough to recover a token
    /// byte by byte over many requests. `ConstantTimeEq` always inspects
    /// everything.
    ///
    /// Lengths are compared first and non-secretly, which is unavoidable: any
    /// comparison of differently-sized inputs reveals that they differ in size.
    /// Only the token's length leaks, never its content.
    #[cfg_attr(not(feature = "http"), allow(dead_code))]
    fn matches(&self, presented: &str) -> bool {
        use subtle::ConstantTimeEq;

        let expected = self.0.as_bytes();
        let presented = presented.as_bytes();

        if expected.len() != presented.len() {
            return false;
        }
        expected.ct_eq(presented).into()
    }
}

/// Read the token from `MEM8_TOKEN`.
///
/// Absent is an error rather than "no authentication required". A server that
/// starts unauthenticated because an environment variable was unset is how a
/// memory store ends up readable by anyone who finds the port.
pub fn token_from_env() -> Result<Token, String> {
    // Unset and set-but-empty are the same situation. Docker Compose passes
    // `MEM8_TOKEN=${MEM8_TOKEN:-}` through as an empty string rather than
    // omitting it, so treating only the unset case as "missing" would give a
    // confusing "token is empty" instead of an explanation.
    match std::env::var("MEM8_TOKEN") {
        Ok(v) if !v.trim().is_empty() => Token::new(v),
        _ => Err(MISSING_TOKEN.to_string()),
    }
}

const MISSING_TOKEN: &str = "MEM8_TOKEN is not set. Serving over HTTP without authentication \
     would expose every memory to anyone who can reach the port. Generate a token with \
     `openssl rand -hex 32` and set MEM8_TOKEN.";

/// The bearer token in an `Authorization` header, if it is well-formed.
///
/// The scheme is matched case-insensitively, as RFC 7235 requires.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
fn bearer_from_header(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Reject anything without a valid bearer token.
///
/// Every failure returns the same bare 401. A response distinguishing "no
/// header" from "wrong token" would confirm to an attacker that a token was
/// well-formed, and there is nothing a legitimate client can do with the
/// difference that the documentation does not already tell it.
#[cfg(feature = "http")]
pub async fn require_token(
    axum::extract::State(token): axum::extract::State<Token>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::http::header;

    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_from_header);

    match presented {
        Some(p) if token.matches(p) => next.run(request).await,
        _ => unauthorized(),
    }
}

#[cfg(feature = "http")]
fn unauthorized() -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    (
        StatusCode::UNAUTHORIZED,
        // Tells a client *how* to authenticate without revealing whether it
        // came close.
        [(header::WWW_AUTHENTICATE, "Bearer")],
        "unauthorized",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_matching_token_is_accepted() {
        let token = Token::new(VALID).unwrap();
        assert!(token.matches(VALID));
    }

    #[test]
    fn a_different_token_is_rejected() {
        let token = Token::new(VALID).unwrap();
        assert!(!token.matches("0123456789abcdef0123456789abcdeg"));
    }

    #[test]
    fn a_prefix_of_the_token_is_rejected() {
        // The case a naive `starts_with` would wrongly accept.
        let token = Token::new(VALID).unwrap();
        assert!(!token.matches("0123456789abcdef"));
    }

    #[test]
    fn a_longer_string_containing_the_token_is_rejected() {
        let token = Token::new(VALID).unwrap();
        assert!(!token.matches(&format!("{VALID}extra")));
    }

    #[test]
    fn an_empty_presented_token_is_rejected() {
        let token = Token::new(VALID).unwrap();
        assert!(!token.matches(""));
    }

    #[test]
    fn a_short_token_is_refused_at_construction() {
        // Refusing here means the server never starts, rather than running with
        // a token that protects nothing.
        let err = Token::new("short").unwrap_err();
        assert!(
            err.contains("16"),
            "the error should state the requirement: {err}"
        );
    }

    #[test]
    fn an_empty_token_is_refused_at_construction() {
        assert!(Token::new("").is_err());
        assert!(Token::new("     ").is_err());
    }

    #[test]
    fn a_token_is_trimmed_before_use() {
        // A trailing newline from `MEM8_TOKEN=$(cat secret)` must not silently
        // make every request fail.
        let token = Token::new(format!("  {VALID}\n")).unwrap();
        assert!(token.matches(VALID));
    }

    #[test]
    fn debug_output_never_contains_the_secret() {
        let token = Token::new(VALID).unwrap();
        let rendered = format!("{token:?}");
        assert!(
            !rendered.contains(VALID),
            "Debug leaked the token: {rendered}"
        );
        assert!(rendered.contains("redacted"));
    }

    /// Every `MEM8_TOKEN` case in one test.
    ///
    /// Environment variables are process-global, so separate tests reading and
    /// writing this one would race under `cargo test`'s default parallelism.
    #[test]
    fn the_environment_token_is_read_and_validated() {
        // SAFETY: single-threaded within this test; see the doc comment above
        // for why the cases are not split into separate tests.
        unsafe {
            std::env::remove_var("MEM8_TOKEN");
        }
        let err = token_from_env().unwrap_err();
        assert!(
            err.contains("MEM8_TOKEN is not set"),
            "an unset token must explain itself: {err}"
        );
        assert!(err.contains("openssl"), "and say how to make one: {err}");

        // Compose passes an unset variable through as an empty string; that is
        // the same situation, not a different one.
        unsafe {
            std::env::set_var("MEM8_TOKEN", "");
        }
        assert!(token_from_env()
            .unwrap_err()
            .contains("MEM8_TOKEN is not set"));

        unsafe {
            std::env::set_var("MEM8_TOKEN", "   ");
        }
        assert!(token_from_env()
            .unwrap_err()
            .contains("MEM8_TOKEN is not set"));

        // Present but too short: a different failure, with its own message.
        unsafe {
            std::env::set_var("MEM8_TOKEN", "abc");
        }
        let short = token_from_env().unwrap_err();
        assert!(
            short.contains("16"),
            "a short token must state the requirement: {short}"
        );

        unsafe {
            std::env::set_var("MEM8_TOKEN", VALID);
        }
        assert!(token_from_env().is_ok());

        unsafe {
            std::env::remove_var("MEM8_TOKEN");
        }
    }

    #[test]
    fn bearer_scheme_is_parsed_case_insensitively() {
        assert_eq!(bearer_from_header("Bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("bearer abc"), Some("abc"));
        assert_eq!(bearer_from_header("BEARER abc"), Some("abc"));
    }

    #[test]
    fn a_non_bearer_scheme_is_not_accepted() {
        assert_eq!(bearer_from_header("Basic abc"), None);
        assert_eq!(bearer_from_header("abc"), None);
        assert_eq!(bearer_from_header("Bearer "), None);
        assert_eq!(bearer_from_header(""), None);
    }

    /// The mitigation has to actually be constant-time, not merely intended to
    /// be. A wrong first byte and a wrong last byte must take indistinguishable
    /// time; `==` would return far sooner on the first.
    ///
    /// Timing tests are inherently noisy, so this compares medians over many
    /// iterations and allows a wide margin. It is built to catch a regression
    /// to `==` -- which differs by orders of magnitude -- not to certify the
    /// absence of a subtle side channel.
    #[test]
    fn comparison_time_does_not_depend_on_the_first_wrong_byte() {
        use std::time::Instant;

        let token = Token::new(VALID).unwrap();
        let wrong_first = format!("X{}", &VALID[1..]);
        let wrong_last = format!("{}X", &VALID[..VALID.len() - 1]);

        fn median_nanos(token: &Token, candidate: &str) -> u128 {
            let mut samples: Vec<u128> = (0..2000)
                .map(|_| {
                    let start = Instant::now();
                    std::hint::black_box(token.matches(std::hint::black_box(candidate)));
                    start.elapsed().as_nanos()
                })
                .collect();
            samples.sort_unstable();
            samples[samples.len() / 2]
        }

        // Warm up, so the first measured run does not pay for cold caches.
        median_nanos(&token, &wrong_first);

        let early = median_nanos(&token, &wrong_first).max(1);
        let late = median_nanos(&token, &wrong_last).max(1);

        let ratio = early.max(late) as f64 / early.min(late) as f64;
        assert!(
            ratio < 5.0,
            "comparison time varies with the position of the first wrong byte \
             (early {early}ns, late {late}ns, ratio {ratio:.1}); is this still constant-time?"
        );
    }
}
