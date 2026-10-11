//! The Margin look: the app layer of the Margin design system (neutral
//! `app-*` grounds, hairline dividers, near-square corners, no shadows) with
//! vermilion as the one brand accent, on IBM Plex Sans.
//!
//! Colors live in `assets/margin-theme.json` as a gpui-kit theme set (a light
//! "Paper" and a dark "Ink" theme); the system appearance picks between them.

use std::borrow::Cow;
use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeSet};
use gpui_kit::*;

const THEME_SET: &str = include_str!("../assets/margin-theme.json");
/// OFL-licensed (see `assets/IBMPlex-LICENSE.txt`); Latin only, so CJK text
/// falls back to the system face.
const PLEX_SANS: &[u8] = include_bytes!("../assets/IBMPlexSans-Regular.ttf");
const UI_FONT: &str = "IBM Plex Sans";

/// App-layer type sizes: `ui-sm` for rows and controls, `ui-xs` for
/// metadata and section headings.
pub(crate) const UI_SM: Pixels = px(12.);
pub(crate) const UI_XS: Pixels = px(11.);
/// Item titles (asset name in the inspector, empty-state heading). Margin
/// never emphasizes with bold, so size alone carries them.
pub(crate) const TITLE: Pixels = px(16.);
/// Row height of app-layer lists (sidebar, list view header).
pub(crate) const ROW: Pixels = px(24.);
/// `radius-sm`: badges, chips and other small marks.
pub(crate) const RADIUS_SM: Pixels = px(4.);

/// `app-selected`: the selected row, card or sidebar item. gpui-kit clamps
/// `list_active` to 20% alpha, so this reads the unclamped sidebar token.
pub(crate) fn selected_bg(theme: &Theme) -> Hsla {
    theme.sidebar_accent
}

/// `app-hover`: hover on rows, cards and sidebar items.
pub(crate) fn hover_bg(theme: &Theme) -> Hsla {
    theme.list_hover
}

/// Install the Margin themes as gpui-kit's light and dark themes. Call once
/// after `gpui_kit::init`, before the first `Theme::sync_system_appearance`.
pub(crate) fn init(cx: &mut App) {
    let font = match cx.text_system().add_fonts(vec![Cow::Borrowed(PLEX_SANS)]) {
        Ok(()) => Some(SharedString::from(UI_FONT)),
        Err(err) => {
            eprintln!("loading {UI_FONT} failed: {err}");
            None
        }
    };
    let set: ThemeSet = serde_json::from_str(THEME_SET).expect("margin-theme.json parses");
    let theme = Theme::global_mut(cx);
    for mut config in set.themes {
        if font.is_some() {
            config.font_family = font.clone();
        }
        if config.mode.is_dark() {
            theme.dark_theme = Rc::new(config);
        } else {
            theme.light_theme = Rc::new(config);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn theme_set_has_one_light_and_one_dark_theme() {
        let set: ThemeSet = serde_json::from_str(THEME_SET).unwrap();
        let modes: Vec<bool> = set.themes.iter().map(|t| t.mode.is_dark()).collect();
        assert_eq!(modes, [false, true]);
        for theme in &set.themes {
            assert_eq!(theme.radius, Some(2), "radius-xs for buttons and inputs");
            assert_eq!(theme.shadow, Some(false), "hairlines, not shadows");
        }
    }
}
