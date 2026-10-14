//! Light/dark, accent and reduced motion on Windows, from the registry and the
//! system parameters.
//!
//! The decoding is plain arithmetic and is tested on every platform. Only the
//! system reads are Windows-specific.

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

/// `SPI_GETCLIENTAREAANIMATION` says whether the system plays animations in
/// windows. Off means the user wants less motion. If the question cannot be
/// asked, motion is left alone.
pub(crate) fn reduce_motion_from(client_area_animation: Option<bool>) -> bool {
    client_area_animation == Some(false)
}

#[cfg(windows)]
pub(crate) fn read_system() -> Appearance {
    use crate::winreg::read_dword;

    let apps_use_light_theme = read_dword(
        r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
        "AppsUseLightTheme",
    );
    let accent_color = read_dword(r"Software\Microsoft\Windows\DWM", "AccentColor");
    let colorization = read_dword(r"Software\Microsoft\Windows\DWM", "ColorizationColor");
    Appearance {
        scheme: scheme_from(apps_use_light_theme),
        accent: accent_from(accent_color, colorization),
        reduce_motion: reduce_motion_from(client_area_animation()),
    }
}

/// Whether the system plays animations in windows, or `None` if the call
/// fails.
#[cfg(windows)]
#[allow(unsafe_code)]
fn client_area_animation() -> Option<bool> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SystemParametersInfoW,
    };
    let mut enabled: i32 = 0;
    // SAFETY: for this action `pvParam` points to a writable `BOOL` (an `i32`),
    // which `enabled` is; `uiParam` must be 0 and `fWinIni` is unused for a get.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            std::ptr::addr_of_mut!(enabled).cast(),
            0,
        )
    };
    (ok != 0).then_some(enabled != 0)
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

    #[test]
    fn reduced_motion_means_animations_are_off() {
        assert!(reduce_motion_from(Some(false)));
        assert!(!reduce_motion_from(Some(true)));
        // An unanswered question leaves the animations on.
        assert!(!reduce_motion_from(None));
    }

    #[cfg(windows)]
    #[test]
    fn the_real_registry_answers_or_falls_back() {
        use crate::winreg::read_dword;

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
        // Asking the system about animations never fails the read.
        let _ = client_area_animation();
    }
}
