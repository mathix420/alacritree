//! A stable content digest, shared by anything that names a file after what is
//! in it.

/// FNV-1a is small, deterministic across Rust versions, and sufficient here:
/// the digest disambiguates file names rather than protecting an adversarial
/// namespace.
pub(crate) fn stable_digest(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The digest names files on disk, so a change to it silently orphans
    /// every existing scratchpad. Pin the constant.
    #[test]
    fn the_digest_is_stable_across_builds() {
        assert_eq!(stable_digest(b""), 0xcbf29ce484222325);
        assert_eq!(stable_digest(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn different_inputs_give_different_digests() {
        assert_ne!(stable_digest(b"one"), stable_digest(b"two"));
    }
}
