//! Light/dark and accent on Windows, from the registry.
//!
//! The decoding is plain arithmetic and is tested on every platform. Only the
//! registry reads are Windows-specific.

#[cfg(windows)]
use super::Appearance;
use super::{ColorScheme, Rgb};

/// `AppsUseLightTheme` is 0 when applications should be dark and 1 when they
/// should be light. A missing value means the system predates the setting,
/// which is light.
pub(crate) fn scheme_from(apps_use_light_theme: Option<u32>) -> ColorScheme {
    match apps_use_light_theme {
        Some(0) => ColorScheme::Dark,
        _ => ColorScheme::Light,
    }
}

/// `AccentColor` is stored as `0xAABBGGRR`.
pub(crate) fn accent_from_abgr(value: u32) -> Rgb {
    Rgb {
        r: (value & 0xFF) as u8,
        g: ((value >> 8) & 0xFF) as u8,
        b: ((value >> 16) & 0xFF) as u8,
    }
}

/// `ColorizationColor` is stored as `0xAARRGGBB`.
pub(crate) fn accent_from_argb(value: u32) -> Rgb {
    Rgb {
        r: ((value >> 16) & 0xFF) as u8,
        g: ((value >> 8) & 0xFF) as u8,
        b: (value & 0xFF) as u8,
    }
}

/// Prefers the accent value, then the colorization value.
pub(crate) fn accent_from(
    accent_color: Option<u32>,
    colorization_color: Option<u32>,
) -> Option<Rgb> {
    accent_color
        .map(accent_from_abgr)
        .or_else(|| colorization_color.map(accent_from_argb))
}

#[cfg(windows)]
pub(crate) fn read_system() -> Appearance {
    let apps_use_light_theme = read_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    );
    let accent_color = read_dword(r"Software\Microsoft\Windows\DWM", "AccentColor");
    let colorization = read_dword(r"Software\Microsoft\Windows\DWM", "ColorizationColor");
    Appearance {
        scheme: scheme_from(apps_use_light_theme),
        accent: accent_from(accent_color, colorization),
    }
}

/// Reads a `REG_DWORD` under `HKEY_CURRENT_USER`. `None` if it is missing or
/// of another type.
#[cfg(windows)]
#[allow(unsafe_code)]
fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};

    let wide = |text: &str| -> Vec<u16> { text.encode_utf16().chain(std::iter::once(0)).collect() };
    let subkey = wide(subkey);
    let value = wide(value);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: `subkey` and `value` are NUL-terminated UTF-16 strings that
    // outlive the call; `data` is a writable `u32` and `size` holds its size in
    // bytes; the type pointer may be null.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            std::ptr::addr_of_mut!(data).cast(),
            &mut size,
        )
    };
    (status == ERROR_SUCCESS).then_some(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_table() {
        assert_eq!(scheme_from(Some(0)), ColorScheme::Dark);
        assert_eq!(scheme_from(Some(1)), ColorScheme::Light);
        assert_eq!(scheme_from(Some(7)), ColorScheme::Light);
        assert_eq!(scheme_from(None), ColorScheme::Light);
    }

    #[test]
    fn accent_byte_orders() {
        // A pure orange, in each stored order.
        assert_eq!(
            accent_from_abgr(0xFF00_88FF),
            Rgb {
                r: 0xFF,
                g: 0x88,
                b: 0x00
            }
        );
        assert_eq!(
            accent_from_argb(0xC4FF_8800),
            Rgb {
                r: 0xFF,
                g: 0x88,
                b: 0x00
            }
        );
    }

    #[test]
    fn accent_preference() {
        let abgr = 0xFF00_88FF;
        let argb = 0xC400_00FF;
        assert_eq!(
            accent_from(Some(abgr), Some(argb)),
            Some(accent_from_abgr(abgr))
        );
        assert_eq!(
            accent_from(None, Some(argb)),
            Some(Rgb { r: 0, g: 0, b: 255 })
        );
        assert_eq!(accent_from(None, None), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_real_registry_answers_or_falls_back() {
        // The result only has to be well formed; accounts differ in what they set.
        let appearance = read_system();
        assert!(matches!(
            appearance.scheme,
            ColorScheme::Light | ColorScheme::Dark
        ));
        assert_eq!(
            read_dword(r"Software\Microsoft\Windows\NoSuchKeyBytescape", "Nothing"),
            None
        );
    }
}
