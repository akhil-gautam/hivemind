//! The access key a node needs before it will do any work.
//!
//! The key is a gate, not a secret: it is compiled into the binary and
//! printed in the README, because the repository is public. It exists so a
//! node is only usable by someone who has the key, and so a stranger cannot
//! quietly run agents on a Mac that is on your network.

/// The access key every node expects.
pub const LICENSE_KEY: &str = "HM-4KD9-7QXR-2ZTM";

/// Normalise a key the way it is stored and compared.
pub fn normalise(key: &str) -> String {
    key.trim().to_ascii_uppercase()
}

/// Is this the right key?
pub fn valid(key: &str) -> bool {
    normalise(key) == LICENSE_KEY
}

/// How a stored key is shown in the interface.
pub fn masked(key: &str) -> String {
    let key = normalise(key);
    if key.len() <= 4 {
        return "•".repeat(key.len());
    }
    format!("{}{}", &key[..4], "•".repeat(key.len() - 4))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_accepted_regardless_of_case_or_padding() {
        assert!(valid(LICENSE_KEY));
        assert!(valid(&LICENSE_KEY.to_lowercase()));
        assert!(valid(&format!("  {}  ", LICENSE_KEY)));
        assert!(!valid("HM-WRONG-KEY-000"));
        assert!(!valid(""));
    }

    #[test]
    fn a_stored_key_is_masked() {
        let shown = masked(LICENSE_KEY);
        assert!(shown.starts_with("HM-4"));
        assert!(!shown.contains("7QXR"));
    }
}
