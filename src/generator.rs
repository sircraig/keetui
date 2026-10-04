//! Random password generation.

use rand::Rng;
use zeroize::Zeroizing;

const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = "!@#$%^&*()-_=+[]{};:,.<>?/";

pub const MIN_LENGTH: usize = 4;
pub const MAX_LENGTH: usize = 128;

#[derive(Clone, Copy)]
pub struct GenOpts {
    pub length: usize,
    pub lower: bool,
    pub upper: bool,
    pub digits: bool,
    pub symbols: bool,
}

impl Default for GenOpts {
    fn default() -> Self {
        GenOpts {
            length: 20,
            lower: true,
            upper: true,
            digits: true,
            symbols: true,
        }
    }
}

impl GenOpts {
    fn classes(&self) -> Vec<&'static str> {
        let mut classes = Vec::new();
        if self.lower {
            classes.push(LOWER);
        }
        if self.upper {
            classes.push(UPPER);
        }
        if self.digits {
            classes.push(DIGITS);
        }
        if self.symbols {
            classes.push(SYMBOLS);
        }
        if classes.is_empty() {
            classes.push(LOWER);
        }
        classes
    }
}

/// Generate a password containing at least one character from every enabled
/// class (by rejection sampling, so the distribution stays uniform).
pub fn generate(opts: &GenOpts) -> Zeroizing<String> {
    let classes = opts.classes();
    let charset: Vec<char> = classes.iter().flat_map(|c| c.chars()).collect();
    let length = opts.length.clamp(MIN_LENGTH, MAX_LENGTH);

    // rand::rng() is a CSPRNG (ChaCha12) reseeded from the OS.
    let mut rng = rand::rng();
    loop {
        let mut out = Zeroizing::new(String::with_capacity(length));
        for _ in 0..length {
            out.push(charset[rng.random_range(0..charset.len())]);
        }
        if classes
            .iter()
            .all(|class| out.chars().any(|c| class.contains(c)))
        {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_requested_length() {
        let opts = GenOpts::default();
        assert_eq!(generate(&opts).len(), 20);
    }

    #[test]
    fn respects_charset_and_class_coverage() {
        let opts = GenOpts {
            length: 12,
            lower: true,
            upper: false,
            digits: true,
            symbols: false,
        };
        for _ in 0..50 {
            let pw = generate(&opts);
            assert!(pw.chars().all(|c| LOWER.contains(c) || DIGITS.contains(c)));
            assert!(pw.chars().any(|c| LOWER.contains(c)));
            assert!(pw.chars().any(|c| DIGITS.contains(c)));
        }
    }

    #[test]
    fn empty_selection_falls_back_to_lowercase() {
        let opts = GenOpts {
            length: 10,
            lower: false,
            upper: false,
            digits: false,
            symbols: false,
        };
        let pw = generate(&opts);
        assert!(pw.chars().all(|c| LOWER.contains(c)));
    }

    #[test]
    fn length_is_clamped() {
        let opts = GenOpts {
            length: 1,
            ..GenOpts::default()
        };
        assert_eq!(generate(&opts).len(), MIN_LENGTH);
    }
}
