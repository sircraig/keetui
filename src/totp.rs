//! TOTP helpers on top of the keepass crate's `otp` feature.
//!
//! Entries store an `otpauth://` URL in the protected `otp` field (the
//! KeePassXC convention); `Entry::get_otp()` parses it.

/// Normalize user input for the otp field: pass otpauth:// URLs through,
/// wrap a bare base32 secret into a KeePassXC-compatible URL.
pub fn normalize_otp(input: &str, title: &str) -> String {
    let trimmed = input.trim();
    if trimmed.starts_with("otpauth://") {
        return trimmed.to_string();
    }
    let secret: String = trimmed
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_uppercase();
    let label = if title.is_empty() { "keetui" } else { title };
    format!(
        "otpauth://totp/{}?secret={}&period=30&digits=6",
        percent_encode(label),
        secret
    )
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

    #[test]
    fn passes_through_otpauth_urls() {
        let url = "otpauth://totp/x?secret=ABC&period=30";
        assert_eq!(normalize_otp(url, "t"), url);
    }

    #[test]
    fn wraps_bare_secret() {
        assert_eq!(
            normalize_otp("abcd efgh", "My Site"),
            "otpauth://totp/My%20Site?secret=ABCDEFGH&period=30&digits=6"
        );
    }

    #[test]
    fn wrapped_secret_parses_with_keepass() {
        use std::str::FromStr;
        let url = normalize_otp("JBSWY3DPEHPK3PXP", "Example");
        let totp = keepass::db::TOTP::from_str(&url).expect("should parse");
        let code = totp.value_at(0);
        assert_eq!(code.code.len(), 6);
    }
}
