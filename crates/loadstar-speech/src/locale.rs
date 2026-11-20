//! What a menu shows for a voice's language (speech-spec §2, "The menu tree"): the operating
//! system's own name for a BCP-47 tag — "English (United States)" — so no mod has to carry
//! names for every language a machine's voices cover.

use std::collections::HashMap;
use std::sync::Mutex;

static CACHE: Mutex<Option<HashMap<(String, String), String>>> = Mutex::new(None);

/// The display name of language `tag`, written in language `display` where the system can
/// (macOS: any language; Windows: English, or the user's own UI language for anything else).
/// Falls back to English, then to the tag itself. Cached.
pub fn language_name(tag: &str, display: &str) -> String {
    let display = if display.trim().is_empty() {
        "en"
    } else {
        display
    };
    let key = (tag.to_string(), display.to_string());
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache.get_or_insert_with(HashMap::new);
    if let Some(name) = cache.get(&key) {
        return name.clone();
    }
    let name = system_name(tag, display)
        .filter(|n| !n.trim().is_empty())
        .or_else(|| system_name(tag, "en").filter(|n| !n.trim().is_empty()))
        .unwrap_or_else(|| tag.to_string());
    cache.insert(key, name.clone());
    name
}

#[cfg(target_os = "macos")]
fn system_name(tag: &str, display: &str) -> Option<String> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSLocale, NSString};
    autoreleasepool(|_| {
        let locale =
            NSLocale::localeWithLocaleIdentifier(&NSString::from_str(&display.replace('-', "_")));
        let name = locale
            .localizedStringForLocaleIdentifier(&NSString::from_str(&tag.replace('-', "_")))
            .to_string();
        // An identifier the system does not know comes back as itself.
        (name != tag && name != tag.replace('-', "_")).then_some(name)
    })
}

#[cfg(windows)]
fn system_name(tag: &str, display: &str) -> Option<String> {
    use windows::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_SENGLISHDISPLAYNAME, LOCALE_SLOCALIZEDDISPLAYNAME,
    };
    use windows::core::PCWSTR;
    // Windows names a locale in English or in the user's UI language, nothing else.
    let english = display
        .split(['-', '_'])
        .next()
        .is_some_and(|p| p.eq_ignore_ascii_case("en"));
    let which = if english {
        LOCALE_SENGLISHDISPLAYNAME
    } else {
        LOCALE_SLOCALIZEDDISPLAYNAME
    };
    let wide: Vec<u16> = tag
        .replace('_', "-")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = [0u16; 256];
    // SAFETY: a NUL-terminated locale name and a buffer of the stated length.
    let n = unsafe { GetLocaleInfoEx(PCWSTR(wide.as_ptr()), which, Some(&mut buf)) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..n as usize - 1]))
}

#[cfg(not(any(target_os = "macos", windows)))]
fn system_name(_tag: &str, _display: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_tags_fall_back_to_themselves_and_empty_display_is_english() {
        assert_eq!(language_name("zz-ZZ-nope", ""), "zz-ZZ-nope");
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn the_system_names_languages() {
        assert_eq!(language_name("en-US", "en"), "English (United States)");
        assert_eq!(language_name("fr_FR", "en"), "French (France)");
        assert_eq!(
            language_name("en-US", "en"),
            "English (United States)",
            "cached"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_names_them_in_the_display_language() {
        assert_eq!(language_name("en-US", "fr"), "anglais (États-Unis)");
    }
}
