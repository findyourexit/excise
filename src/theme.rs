use clap::ValueEnum;
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum ThemeId {
    #[default]
    ExciseDark,
    ExciseLight,
    HighContrast,
    Monochrome,
    Dracula,
    TokyoNight,
    CatppuccinMocha,
    CatppuccinLatte,
    GruvboxDark,
    GruvboxLight,
    Nord,
    SolarizedDark,
    SolarizedLight,
    OneDark,
    Monokai,
}

impl ThemeId {
    pub const ALL: [Self; 15] = [
        Self::ExciseDark,
        Self::ExciseLight,
        Self::HighContrast,
        Self::Monochrome,
        Self::Dracula,
        Self::TokyoNight,
        Self::CatppuccinMocha,
        Self::CatppuccinLatte,
        Self::GruvboxDark,
        Self::GruvboxLight,
        Self::Nord,
        Self::SolarizedDark,
        Self::SolarizedLight,
        Self::OneDark,
        Self::Monokai,
    ];

    #[must_use]
    pub fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or_default();
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    #[must_use]
    pub const fn attribution(self) -> ThemeAttribution {
        match self {
            Self::ExciseDark | Self::ExciseLight | Self::HighContrast | Self::Monochrome => {
                ThemeAttribution::new("Excise", "MIT", "https://github.com/findyourexit/excise")
            }
            Self::Dracula => ThemeAttribution::new("Dracula", "MIT", "https://draculatheme.com"),
            Self::TokyoNight => ThemeAttribution::new(
                "Tokyo Night",
                "MIT",
                "https://github.com/enkia/tokyo-night-vscode-theme",
            ),
            Self::CatppuccinMocha | Self::CatppuccinLatte => ThemeAttribution::new(
                "Catppuccin",
                "MIT",
                "https://github.com/catppuccin/catppuccin",
            ),
            Self::GruvboxDark | Self::GruvboxLight => {
                ThemeAttribution::new("Gruvbox", "MIT", "https://github.com/morhetz/gruvbox")
            }
            Self::Nord => ThemeAttribution::new("Nord", "MIT", "https://www.nordtheme.com"),
            Self::SolarizedDark | Self::SolarizedLight => {
                ThemeAttribution::new("Solarized", "MIT", "https://ethanschoonover.com/solarized")
            }
            Self::OneDark => {
                ThemeAttribution::new("One Dark", "MIT", "https://github.com/atom/one-dark-syntax")
            }
            Self::Monokai => {
                ThemeAttribution::new("Monokai", "palette attribution", "https://monokai.pro")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ThemeAttribution {
    pub name: &'static str,
    pub license: &'static str,
    pub source: &'static str,
}

impl ThemeAttribution {
    const fn new(name: &'static str, license: &'static str, source: &'static str) -> Self {
        Self {
            name,
            license,
            source,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Theme {
    pub surface_base: Color,
    pub surface_panel: Color,
    pub surface_raised: Color,
    pub surface_selection: Color,
    pub surface_danger: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub text_muted: Color,
    pub text_inverse: Color,
    pub text_danger: Color,
    pub state_scanning: Color,
    pub state_complete: Color,
    pub state_aggregated: Color,
    pub state_rescanning: Color,
    pub state_uncertain: Color,
    pub state_shared: Color,
    pub state_excluded: Color,
    pub border: Color,
    pub focus: Color,
}

impl Theme {
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "keeping every built-in palette in one exhaustive match makes token review auditable"
    )]
    pub const fn for_id(id: ThemeId) -> Self {
        match id {
            ThemeId::ExciseDark => dark(
                rgb(8, 12, 17),
                rgb(15, 23, 32),
                rgb(29, 43, 56),
                rgb(75, 227, 194),
                rgb(255, 82, 102),
                rgb(223, 240, 244),
                rgb(145, 170, 181),
                rgb(255, 199, 95),
                rgb(150, 124, 255),
            ),
            ThemeId::ExciseLight => light(
                rgb(244, 248, 249),
                rgb(228, 237, 239),
                rgb(255, 255, 255),
                rgb(0, 111, 103),
                rgb(181, 31, 57),
                rgb(22, 41, 47),
                rgb(76, 102, 110),
                rgb(153, 91, 0),
                rgb(91, 63, 180),
            ),
            ThemeId::HighContrast => dark(
                Color::Black,
                Color::Black,
                rgb(20, 20, 20),
                Color::Cyan,
                Color::Red,
                Color::White,
                Color::White,
                Color::Yellow,
                Color::Magenta,
            ),
            ThemeId::Monochrome => Self {
                surface_base: Color::Reset,
                surface_panel: Color::Reset,
                surface_raised: Color::Reset,
                surface_selection: Color::Reset,
                surface_danger: Color::Reset,
                text_primary: Color::Reset,
                text_secondary: Color::Reset,
                text_muted: Color::Reset,
                text_inverse: Color::Reset,
                text_danger: Color::Reset,
                state_scanning: Color::Reset,
                state_complete: Color::Reset,
                state_aggregated: Color::Reset,
                state_rescanning: Color::Reset,
                state_uncertain: Color::Reset,
                state_shared: Color::Reset,
                state_excluded: Color::Reset,
                border: Color::Reset,
                focus: Color::Reset,
            },
            ThemeId::Dracula => dark(
                rgb(40, 42, 54),
                rgb(49, 51, 65),
                rgb(68, 71, 90),
                rgb(80, 250, 123),
                rgb(255, 85, 85),
                rgb(248, 248, 242),
                rgb(189, 147, 249),
                rgb(241, 250, 140),
                rgb(255, 121, 198),
            ),
            ThemeId::TokyoNight => dark(
                rgb(26, 27, 38),
                rgb(31, 35, 53),
                rgb(41, 46, 66),
                rgb(125, 207, 255),
                rgb(247, 118, 142),
                rgb(192, 202, 245),
                rgb(169, 177, 214),
                rgb(224, 175, 104),
                rgb(187, 154, 247),
            ),
            ThemeId::CatppuccinMocha => dark(
                rgb(30, 30, 46),
                rgb(49, 50, 68),
                rgb(69, 71, 90),
                rgb(166, 227, 161),
                rgb(243, 139, 168),
                rgb(205, 214, 244),
                rgb(166, 173, 200),
                rgb(249, 226, 175),
                rgb(203, 166, 247),
            ),
            ThemeId::CatppuccinLatte => light(
                rgb(239, 241, 245),
                rgb(230, 233, 239),
                rgb(220, 224, 232),
                rgb(64, 160, 43),
                rgb(210, 15, 57),
                rgb(76, 79, 105),
                rgb(108, 111, 133),
                rgb(223, 142, 29),
                rgb(136, 57, 239),
            ),
            ThemeId::GruvboxDark => dark(
                rgb(40, 40, 40),
                rgb(60, 56, 54),
                rgb(80, 73, 69),
                rgb(184, 187, 38),
                rgb(251, 73, 52),
                rgb(235, 219, 178),
                rgb(168, 153, 132),
                rgb(250, 189, 47),
                rgb(211, 134, 155),
            ),
            ThemeId::GruvboxLight => light(
                rgb(251, 241, 199),
                rgb(235, 219, 178),
                rgb(213, 196, 161),
                rgb(121, 116, 14),
                rgb(204, 36, 29),
                rgb(60, 56, 54),
                rgb(102, 92, 84),
                rgb(181, 118, 20),
                rgb(143, 63, 113),
            ),
            ThemeId::Nord => dark(
                rgb(46, 52, 64),
                rgb(59, 66, 82),
                rgb(67, 76, 94),
                rgb(136, 192, 208),
                rgb(191, 97, 106),
                rgb(236, 239, 244),
                rgb(216, 222, 233),
                rgb(235, 203, 139),
                rgb(180, 142, 173),
            ),
            ThemeId::SolarizedDark => dark(
                rgb(0, 43, 54),
                rgb(7, 54, 66),
                rgb(88, 110, 117),
                rgb(42, 161, 152),
                rgb(220, 50, 47),
                rgb(238, 232, 213),
                rgb(147, 161, 161),
                rgb(181, 137, 0),
                rgb(108, 113, 196),
            ),
            ThemeId::SolarizedLight => light(
                rgb(253, 246, 227),
                rgb(238, 232, 213),
                rgb(147, 161, 161),
                rgb(42, 161, 152),
                rgb(220, 50, 47),
                rgb(0, 43, 54),
                rgb(88, 110, 117),
                rgb(181, 137, 0),
                rgb(108, 113, 196),
            ),
            ThemeId::OneDark => dark(
                rgb(40, 44, 52),
                rgb(44, 49, 58),
                rgb(62, 68, 81),
                rgb(152, 195, 121),
                rgb(224, 108, 117),
                rgb(171, 178, 191),
                rgb(130, 137, 151),
                rgb(229, 192, 123),
                rgb(198, 120, 221),
            ),
            ThemeId::Monokai => dark(
                rgb(39, 40, 34),
                rgb(49, 50, 44),
                rgb(73, 72, 62),
                rgb(166, 226, 46),
                rgb(249, 38, 114),
                rgb(248, 248, 242),
                rgb(174, 174, 164),
                rgb(230, 219, 116),
                rgb(174, 129, 255),
            ),
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the nine palette inputs are the minimal named semantic color groups"
)]
const fn dark(
    base: Color,
    panel: Color,
    raised: Color,
    focus: Color,
    danger: Color,
    primary: Color,
    secondary: Color,
    warning: Color,
    shared: Color,
) -> Theme {
    Theme {
        surface_base: base,
        surface_panel: panel,
        surface_raised: raised,
        surface_selection: focus,
        surface_danger: danger,
        text_primary: primary,
        text_secondary: secondary,
        text_muted: secondary,
        text_inverse: base,
        text_danger: danger,
        state_scanning: warning,
        state_complete: focus,
        state_aggregated: warning,
        state_rescanning: focus,
        state_uncertain: danger,
        state_shared: shared,
        state_excluded: secondary,
        border: secondary,
        focus,
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "light and dark palettes intentionally share the same semantic color groups"
)]
const fn light(
    base: Color,
    panel: Color,
    raised: Color,
    focus: Color,
    danger: Color,
    primary: Color,
    secondary: Color,
    warning: Color,
    shared: Color,
) -> Theme {
    dark(
        base, panel, raised, focus, danger, primary, secondary, warning, shared,
    )
}

const fn rgb(red: u8, green: u8, blue: u8) -> Color {
    Color::Rgb(red, green, blue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_fifteen_themes_cover_every_semantic_role() {
        assert_eq!(ThemeId::ALL.len(), 15);
        for id in ThemeId::ALL {
            let theme = Theme::for_id(id);
            let attribution = id.attribution();
            assert!(!attribution.name.is_empty());
            assert!(!attribution.license.is_empty());
            assert!(attribution.source.starts_with("https://"));
            if id != ThemeId::Monochrome {
                assert_ne!(theme.text_primary, theme.surface_base);
                assert_ne!(theme.focus, theme.surface_base);
                assert_ne!(theme.text_danger, theme.surface_base);
            }
        }
    }

    #[test]
    fn theme_cycle_visits_all_built_ins() {
        let mut current = ThemeId::ExciseDark;
        let mut visited = Vec::new();
        for _ in 0..ThemeId::ALL.len() {
            visited.push(current);
            current = current.next();
        }
        assert_eq!(visited, ThemeId::ALL);
        assert_eq!(current, ThemeId::ExciseDark);
    }
}
