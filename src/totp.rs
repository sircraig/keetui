//! TOTP for entries: parsing the stored settings and generating codes.
//!
//! Entries keep their settings in the protected `otp` field, usually as an
//! `otpauth://totp/...` URL (the KeePassXC convention) and sometimes in
//! KeeOTP's `key=...&size=...&step=...` form. keepass-rs's own parser gets
//! several of these wrong (8 digits when `digits` is absent, no Steam codes,
//! HOTP read as TOTP, lowercase secrets rejected), so keetui does it itself.

use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha1::Sha1;
use sha2::{Sha256, Sha512};
use zeroize::Zeroizing;

/// Code generation divides by the period and by 10^digits: allow what
/// authenticators actually use, with room.
const MAX_PERIOD: u64 = 24 * 60 * 60;
const MAX_DIGITS: u32 = 10;
/// Steam Guard codes are 5 characters from this alphabet.
const STEAM_CHARS: &[u8; 26] = b"23456789BCDFGHJKMNPQRTVWXY";
const STEAM_LENGTH: u32 = 5;

#[derive(Clone, Copy)]
enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

/// Parsed TOTP settings.
pub struct Totp {
    secret: Zeroizing<Vec<u8>>,
    period: u64,
    digits: u32,
    algorithm: Algorithm,
    steam: bool,
}

/// A generated code.
pub struct Code {
    pub code: String,
    /// How much longer the code is valid.
    pub valid_for: Duration,
    pub period: Duration,
}

/// Parse an entry's otp value, either an `otpauth://totp/` URL or KeeOTP's
/// `key=...` form, refusing settings keetui can't turn into correct codes.
pub fn parse(value: &str) -> Result<Totp, String> {
    let value = value.trim();
    let (params, keeotp) = match strip_prefix_ignore_case(value, "otpauth://") {
        Some(rest) => {
            let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
            let kind = head.split('/').next().unwrap_or("");
            if kind.eq_ignore_ascii_case("hotp") {
                return Err(HOTP_UNSUPPORTED.into());
            }
            if !kind.eq_ignore_ascii_case("totp") {
                return Err("invalid OTP data (not an otpauth://totp/ URL)".into());
            }
            (query.split('#').next().unwrap_or(""), false)
        }
        None if is_keeotp(value) => (value, true),
        None => return Err("invalid OTP data (not an otpauth:// URL)".into()),
    };

    let mut secret = None;
    let mut period = 30;
    let mut digits = 6;
    let mut algorithm = Algorithm::Sha1;
    let mut steam = false;
    for (key, val) in form_urlencoded::parse(params.as_bytes()) {
        // Values may be the secret: keep them in wiped buffers.
        let val = Zeroizing::new(val.into_owned());
        match (key.to_ascii_lowercase().as_str(), keeotp) {
            ("secret", false) | ("key", true) => secret = Some(decode_secret(&val)?),
            ("period", false) | ("step", true) => period = number(&val, "period")?,
            ("digits", false) | ("size", true) => digits = number(&val, "length")?,
            ("algorithm", false) | ("otphashmode", true) => algorithm = parse_algorithm(&val)?,
            ("encoder", false) => steam = val.eq_ignore_ascii_case("steam"),
            ("type", true) if !val.eq_ignore_ascii_case("totp") => {
                return Err(HOTP_UNSUPPORTED.into());
            }
            _ => {}
        }
    }
    let secret = secret.ok_or("invalid OTP data (no secret)")?;
    if steam {
        digits = STEAM_LENGTH;
    }
    if !(1..=MAX_PERIOD).contains(&period) {
        return Err(format!("invalid OTP period: {period}s"));
    }
    if !(1..=MAX_DIGITS).contains(&digits) {
        return Err(format!("invalid OTP length: {digits} digits"));
    }
    Ok(Totp {
        secret,
        period,
        digits,
        algorithm,
        steam,
    })
}

const HOTP_UNSUPPORTED: &str = "HOTP (counter-based) codes aren't supported";

impl Totp {
    /// The code for a unix timestamp.
    pub fn value_at(&self, time: u64) -> Code {
        let value = truncate(&self.hmac(time / self.period));
        let code = if self.steam {
            steam_code(value)
        } else {
            let code = u64::from(value) % 10u64.pow(self.digits);
            format!("{code:0width$}", width = self.digits as usize)
        };
        Code {
            code,
            valid_for: Duration::from_secs(self.period - time % self.period),
            period: Duration::from_secs(self.period),
        }
    }

    pub fn value_now(&self) -> Result<Code, SystemTimeError> {
        let time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        Ok(self.value_at(time))
    }

    fn hmac(&self, counter: u64) -> Vec<u8> {
        const ANY_KEY: &str = "HMAC accepts keys of any length";
        let msg = counter.to_be_bytes();
        match self.algorithm {
            Algorithm::Sha1 => Hmac::<Sha1>::new_from_slice(&self.secret)
                .expect(ANY_KEY)
                .chain_update(msg)
                .finalize()
                .into_bytes()
                .to_vec(),
            Algorithm::Sha256 => Hmac::<Sha256>::new_from_slice(&self.secret)
                .expect(ANY_KEY)
                .chain_update(msg)
                .finalize()
                .into_bytes()
                .to_vec(),
            Algorithm::Sha512 => Hmac::<Sha512>::new_from_slice(&self.secret)
                .expect(ANY_KEY)
                .chain_update(msg)
                .finalize()
                .into_bytes()
                .to_vec(),
        }
    }
}

/// RFC 4226 dynamic truncation: 31 bits at an offset given by the last byte.
fn truncate(hash: &[u8]) -> u32 {
    let offset = usize::from(hash[hash.len() - 1] & 0x0f);
    let bytes = [
        hash[offset],
        hash[offset + 1],
        hash[offset + 2],
        hash[offset + 3],
    ];
    u32::from_be_bytes(bytes) & 0x7fff_ffff
}

fn steam_code(mut value: u32) -> String {
    (0..STEAM_LENGTH)
        .map(|_| {
            let c = STEAM_CHARS[(value % 26) as usize] as char;
            value /= 26;
            c
        })
        .collect()
}

/// Base32, case-insensitively and ignoring spaces and `=` padding, as
/// KeePassXC accepts it.
fn decode_secret(text: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let mut clean = Zeroizing::new(String::with_capacity(text.len()));
    clean.extend(text.chars().filter(|&c| !c.is_whitespace() && c != '='));
    clean.make_ascii_uppercase();
    match base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &clean) {
        Some(bytes) if !bytes.is_empty() => Ok(Zeroizing::new(bytes)),
        _ => Err("invalid OTP secret (not base32)".into()),
    }
}

fn number<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("invalid OTP {what}: {text}"))
}

fn parse_algorithm(name: &str) -> Result<Algorithm, String> {
    match name.to_ascii_uppercase().as_str() {
        "SHA1" => Ok(Algorithm::Sha1),
        "SHA256" => Ok(Algorithm::Sha256),
        "SHA512" => Ok(Algorithm::Sha512),
        _ => Err(format!("unsupported OTP algorithm: {name}")),
    }
}

/// KeeOTP's form: query-style pairs with the secret under `key`.
fn is_keeotp(value: &str) -> bool {
    form_urlencoded::parse(value.as_bytes()).any(|(key, _)| key.eq_ignore_ascii_case("key"))
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// Normalize user input for the otp field: pass otpauth:// URLs and KeeOTP
/// values through, wrap a bare base32 secret into a KeePassXC-compatible
/// URL. Buffers are sized up front so no reallocation leaves the secret
/// behind unwiped.
pub fn normalize_otp(input: &str, title: &str) -> Zeroizing<String> {
    let trimmed = input.trim();
    if strip_prefix_ignore_case(trimmed, "otpauth://").is_some() || is_keeotp(trimmed) {
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
    use std::time::Duration;

    // RFC 6238 test secrets: "1234567890" repeated to 20, 32 and 64 bytes.
    const RFC_SHA1: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    const RFC_SHA256: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA";
    const RFC_SHA512: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ\
                              GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNA";

    fn code(value: &str, time: u64) -> String {
        let totp = parse(value).unwrap_or_else(|e| panic!("{value}: {e}"));
        totp.value_at(time).code.clone()
    }

    #[test]
    fn rfc_6238_vectors() {
        for (time, sha1, sha256, sha512) in [
            (59, "94287082", "46119246", "90693936"),
            (1111111109, "07081804", "68084774", "25091201"),
            (1234567890, "89005924", "91819424", "93441116"),
            (2000000000, "69279037", "90698825", "38618901"),
        ] {
            let url = |secret: &str, alg: &str| {
                format!("otpauth://totp/x?secret={secret}&digits=8&algorithm={alg}")
            };
            assert_eq!(code(&url(RFC_SHA1, "SHA1"), time), sha1);
            assert_eq!(code(&url(RFC_SHA256, "SHA256"), time), sha256);
            assert_eq!(code(&url(RFC_SHA512, "SHA512"), time), sha512);
        }
    }

    #[test]
    fn digits_default_to_six() {
        // The Key URI format and KeePassXC default to 6 digits.
        let url = format!("otpauth://totp/Example:alice?secret={RFC_SHA1}&issuer=Example");
        assert_eq!(code(&url, 59), "287082");
    }

    #[test]
    fn secrets_are_case_insensitive() {
        let url = format!(
            "otpauth://totp/x?secret={}&digits=8",
            RFC_SHA1.to_lowercase()
        );
        assert_eq!(code(&url, 59), "94287082");
    }

    #[test]
    fn steam_codes() {
        // KeePassXC's test vectors.
        let url = "otpauth://totp/test:test@example.com?\
                   secret=63BEDWCQZKTQWPESARIERL5DTTQFCJTK&issuer=Steam&encoder=steam";
        assert_eq!(code(url, 1511200518), "FR8RV");
        assert_eq!(code(url, 1511200714), "9P3VP");
    }

    #[test]
    fn keeotp_values() {
        // KeePass + KeeOtp's format, which KeePassXC also reads.
        assert_eq!(code(&format!("key={RFC_SHA1}&size=8"), 59), "94287082");
        let padded = format!("key={RFC_SHA256}%3d%3d%3d%3d&size=8&otpHashMode=Sha256");
        assert_eq!(code(&padded, 59), "46119246");
        let totp = parse("key=JBSWY3DPEHPK3PXP&step=60").unwrap();
        assert_eq!(totp.value_at(0).period, Duration::from_secs(60));
    }

    #[test]
    fn hotp_is_refused() {
        // Counter-based codes need the counter saved back after each use;
        // showing a time-based code instead would just be wrong.
        assert!(parse(&format!("otpauth://hotp/x?secret={RFC_SHA1}&counter=1")).is_err());
        assert!(parse(&format!("key={RFC_SHA1}&type=Hotp")).is_err());
    }

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
        // So are KeeOTP values.
        let keeotp = "key=JBSWY3DPEHPK3PXP&size=8";
        assert_eq!(normalize_otp(keeotp, "t").as_str(), keeotp);
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
    fn wrapped_secret_parses() {
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
