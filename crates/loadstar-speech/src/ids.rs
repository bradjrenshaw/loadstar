//! Stable 64-bit ids (speech-spec §2): categories and voices are hashed from their keys so a mod
//! can persist "the user chose NVDA" as a number that survives reinstalls and list reorderings.

/// FNV-1a, 64-bit. Not cryptographic; stable across platforms and versions by definition.
pub fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Never collide with the reserved root auto.
    if h == ROOT_AUTO { 1 } else { h }
}

/// Voice id 0: the root auto — the running screen reader if any, else the default voice of the
/// first synthesizer category.
pub const ROOT_AUTO: u64 = 0;

pub fn category_id(key: &str) -> u64 {
    hash(&format!("category:{key}"))
}

pub fn category_auto(key: &str) -> u64 {
    hash(&format!("auto:{key}"))
}

/// A leaf's id: the backend that owns it plus the backend's own identifier for the voice (a SAPI
/// token id, a Prism backend id), so the same voice reached through a different backend gets a
/// different id and a reinstall that reorders the list changes nothing.
pub fn voice_id(backend: &str, native_id: &str) -> u64 {
    hash(&format!("voice:{backend}/{native_id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_and_distinct() {
        assert_eq!(hash("nvda"), hash("nvda"));
        assert_ne!(hash("nvda"), hash("jaws"));
        assert_ne!(category_id("sapi5"), category_auto("sapi5"));
        assert_ne!(voice_id("sapi", "x"), voice_id("prism", "x"));
        assert_ne!(voice_id("sapi", "x"), ROOT_AUTO);
    }
}
