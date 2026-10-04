//! TOTP helpers on top of the keepass crate's `otp` feature.
//!
//! Entries store an `otpauth://` URL in the protected `otp` field (the
//! KeePassXC convention). Always go through [`parse`] rather than
//! `Entry::get_otp()`: it rejects settings that make code generation panic.

use std::str::FromStr;

use keepass::db::TOTP;
use zeroize::Zeroizing;

/// keepass-rs accepts any period and digit count, but code generation
/// (totp-lite) divides by the period and by 10^digits, so a zero period or
/// 64+ digits panics. Allow what authenticators actually use, with room.
const MAX_PERIOD: u64 = 24 * 60 * 60;
const MAX_DIGITS: u32 = 10;

/// Parse an `otpauth://` URL, refusing settings that can't produce codes.
pub fn parse(url: &str) -> Result<TOTP, String> {
    let totp = TOTP::from_str(url).map_err(|e| format!("invalid OTP data ({e})"))?;
    if !(1..=MAX_PERIOD).contains(&totp.period) {
        return Err(format!("invalid OTP period: {}s", totp.period));
    }
    if !(1..=MAX_DIGITS).contains(&totp.digits) {
        return Err(format!("invalid OTP length: {} digits", totp.digits));
    }
    Ok(totp)
}

/// Normalize user input for the otp field: pass otpauth:// URLs through,
/// wrap a bare base32 secret into a KeePassXC-compatible URL. Buffers are
/// sized up front so no reallocation leaves the secret behind unwiped.
pub fn normalize_otp(input: &str, title: &str) -> Zeroizing<String> {
    let trimmed = input.trim();
    if trimmed
        .get(..10)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("otpauth://"))
    {
        return Zeroizing::new(trimmed.to_string());
    }
    let mut secret = Zeroizing::new(String::with_capacity(trimmed.len()));
    secret.extend(trimmed.chars().filter(|c| !c.is_whitespace()));
    secret.make_ascii_uppercase();
    let label = percent_encode(if title.is_empty() { "keetui" } else { title });

    const PARTS: [&str; 3] = ["otpauth://totp/", "?secret=", "&period=30&digits=6"];
    let len = PARTS.iter().map(|p| p.len()).sum::<usize>() + label.len() + secret.len();
    let mut url = Zeroizing::new(String::with_capacity(len));
    for (part, value) in PARTS.iter().zip([label.as_str(), secret.as_str(), ""]) {
        url.push_str(part);
        url.push_str(value);
    }
    url
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "JBSWY3DPEHPK3PXP";

    fn url(params: &str) -> String {
        format!("otpauth://totp/x?secret={SECRET}{params}")
    }

    #[test]
    fn passes_through_otpauth_urls() {
        let url = "otpauth://totp/x?secret=ABC&period=30";
        assert_eq!(normalize_otp(url, "t").as_str(), url);
        // The scheme is case-insensitive.
        let url = "OTPAUTH://totp/x?secret=ABC";
        assert_eq!(normalize_otp(url, "t").as_str(), url);
    }

    #[test]
    fn wraps_bare_secret() {
        let url = normalize_otp("abcd efgh", "My Site");
        assert_eq!(
            url.as_str(),
            "otpauth://totp/My%20Site?secret=ABCDEFGH&period=30&digits=6"
        );
        assert_eq!(url.len(), url.capacity(), "sized exactly, never grown");
    }

    #[test]
    fn wrapped_secret_parses_with_keepass() {
        let url = normalize_otp(SECRET, "Example");
        let totp = parse(&url).expect("should parse");
        let code = totp.value_at(0);
        assert_eq!(code.code.len(), 6);
    }

    #[test]
    fn rejects_settings_that_crash_code_generation() {
        // Each of these parses fine in keepass-rs and then panics in
        // totp-lite when a code is generated.
        for bad in ["&period=0", "&digits=0", "&digits=64", "&digits=4294967295"] {
            assert!(parse(&url(bad)).is_err(), "{bad} should be rejected");
        }
        for bad in ["&period=86401", "&digits=11"] {
            assert!(parse(&url(bad)).is_err(), "{bad} should be rejected");
        }
        assert!(parse("otpauth://totp/x?secret=not-base32!").is_err());
        assert!(parse("https://example.com").is_err());
    }

    #[test]
    fn accepted_settings_generate_codes() {
        for digits in 1..=MAX_DIGITS {
            for period in [1, 30, 60, MAX_PERIOD] {
                let totp = parse(&url(&format!("&period={period}&digits={digits}"))).unwrap();
                assert_eq!(totp.value_at(u64::MAX / 2).code.len(), digits as usize);
            }
        }
    }
}
