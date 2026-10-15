//! The operating system's light or dark setting, its accent color and its
//! reduced-motion setting.
//!
//! The setting is read from the system every time [`current`] is called and is
//! never stored in Bytescape's own configuration.
//!
//! # Change notification
//!
//! There is none. Call [`current`] when the window gains focus and compare the
//! result with the previous one; that picks up a change made in the system
//! settings the next time the user looks at Bytescape, with no listener thread
//! or message loop hook. A call is cheap on Windows (a few registry reads and
//! one system-parameter query). On Linux it may start `gdbus` and `gsettings`
//! several times, which takes tens of milliseconds each
//! when the desktop answers and at most 1.5 seconds in total when it does not,
//! so call it on focus, not on every frame, and not on the interface thread if
//! a stall of that length would matter.
//!
//! # How it is read
//!
//! - **Windows:** the `AppsUseLightTheme` registry value for light or dark, and
//!   the DWM `AccentColor` value (falling back to `ColorizationColor`) for the
//!   accent. The registry is used because the alternative, the WinRT
//!   `UISettings` class, needs a WinRT runtime crate. Reduced motion is the
//!   system's client-area animation switch (`SPI_GETCLIENTAREAANIMATION`).
//! - **Linux:** the desktop portal's `org.freedesktop.appearance` settings
//!   `color-scheme`, `accent-color` and `reduced-motion`, read by running
//!   `gdbus call`, so no D-Bus library is linked. When the portal does not
//!   answer:
//!   - light or dark comes from GNOME's `color-scheme` setting (`gsettings`),
//!     and is light if that fails too;
//!   - the accent comes from the `AccentColor` entry of KDE's `kdeglobals`
//!     file, then from GNOME's `accent-color` setting, and is `None` if both
//!     fail;
//!   - motion is reduced if GNOME's `enable-animations` is off or KDE's
//!     `AnimationDurationFactor` in `kdeglobals` is 0. Any one source asking
//!     for less motion is enough, because wrongly dropping an animation costs
//!     little and wrongly playing one can make a person ill.
//!
//! # When the Linux programs are not started
//!
//! Bytescape never opens a network connection, and a D-Bus address can name a
//! TCP transport. So `gdbus` and `gsettings` are started only if the session
//! bus is a local Unix socket: `DBUS_SESSION_BUS_ADDRESS` must hold only
//! `unix:` addresses, or be unset with a `bus` socket in `$XDG_RUNTIME_DIR`
//! (the programs are then told that socket's address). In every other case
//! no program is started and the result comes from the `kdeglobals` file alone
//! (the file is read, not run), which is [`Appearance::default`] if there is
//! none: light, with no system accent, and motion as usual.

/// Whether the system asks for light or dark applications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ColorScheme {
    Light,
    Dark,
}

/// A color in sRGB.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// The accent used when the system does not give one: `#3B82F6`.
pub const DEFAULT_ACCENT: Rgb = Rgb {
    r: 0x3B,
    g: 0x82,
    b: 0xF6,
};

impl Rgb {
    /// Builds a color from channel values between 0 and 1. `None` if any is
    /// outside that range or not a number.
    pub fn from_unit_floats(r: f64, g: f64, b: f64) -> Option<Self> {
        let channel = |value: f64| {
            // `contains` is false for NaN.
            (0.0..=1.0)
                .contains(&value)
                .then(|| (value * 255.0).round() as u8)
        };
        Some(Self {
            r: channel(r)?,
            g: channel(g)?,
            b: channel(b)?,
        })
    }
}

/// What the system says about appearance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub scheme: ColorScheme,
    /// The system accent color, or `None` when the system does not provide one.
    pub accent: Option<Rgb>,
    /// The user asks for less motion: animations should be replaced by
    /// instant changes. `false` when the system says nothing.
    pub reduce_motion: bool,
}

impl Appearance {
    /// The accent to draw with: the system's, or [`DEFAULT_ACCENT`].
    pub fn accent_or_default(&self) -> Rgb {
        self.accent.unwrap_or(DEFAULT_ACCENT)
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: ColorScheme::Light,
            accent: None,
            reduce_motion: false,
        }
    }
}

/// Reads the current setting from the system.
///
/// Never fails: anything unreadable falls back to light, no system accent and
/// normal motion.
pub fn current() -> Appearance {
    #[cfg(target_os = "linux")]
    {
        linux::read_system()
    }
    #[cfg(windows)]
    {
        windows::read_system()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        Appearance::default()
    }
}

#[cfg(target_os = "linux")]
mod linux;
// The session-bus rule is shared with the programs started for handoffs.
#[cfg(target_os = "linux")]
pub(crate) use linux::{Bus, session_bus};
#[cfg(any(windows, test))]
mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_floats_round_and_reject() {
        assert_eq!(
            Rgb::from_unit_floats(0.0, 0.5, 1.0),
            Some(Rgb {
                r: 0,
                g: 128,
                b: 255
            })
        );
        assert_eq!(Rgb::from_unit_floats(-0.1, 0.0, 0.0), None);
        assert_eq!(Rgb::from_unit_floats(0.0, 1.5, 0.0), None);
        assert_eq!(Rgb::from_unit_floats(0.0, 0.0, f64::NAN), None);
        assert_eq!(Rgb::from_unit_floats(f64::INFINITY, 0.0, 0.0), None);
    }

    #[test]
    fn default_accent_is_the_documented_blue() {
        assert_eq!(
            DEFAULT_ACCENT,
            Rgb {
                r: 59,
                g: 130,
                b: 246
            }
        );
        let none = Appearance::default();
        assert_eq!(none.accent_or_default(), DEFAULT_ACCENT);
        let some = Appearance {
            scheme: ColorScheme::Dark,
            accent: Some(Rgb { r: 1, g: 2, b: 3 }),
            reduce_motion: true,
        };
        assert_eq!(some.accent_or_default(), Rgb { r: 1, g: 2, b: 3 });
    }

    #[test]
    fn the_default_asks_for_nothing_special() {
        let none = Appearance::default();
        assert_eq!(none.scheme, ColorScheme::Light);
        assert_eq!(none.accent, None);
        assert!(!none.reduce_motion);
    }

    #[test]
    fn reading_the_real_system_never_panics() {
        let _ = current();
    }
}
