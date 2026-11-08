//! Color themes: the ones built in (themes/, from gpui-component, Apache-2.0)
//! and any in ~/.config/vmac/themes/*.json, in gpui-kit's theme format.
//! vmac's syntax and Markdown colors are the theme's base colors, so a theme
//! recolors everything.

use std::path::Path;

/// The built-in theme files.
pub const BUILT_IN: &[&str] = &[
    include_str!("../themes/adventure.json"),
    include_str!("../themes/alduin.json"),
    include_str!("../themes/asciinema.json"),
    include_str!("../themes/aurora.json"),
    include_str!("../themes/ayu.json"),
    include_str!("../themes/catppuccin.json"),
    include_str!("../themes/everforest.json"),
    include_str!("../themes/fahrenheit.json"),
    include_str!("../themes/flexoki.json"),
    include_str!("../themes/gruvbox.json"),
    include_str!("../themes/harper.json"),
    include_str!("../themes/hybrid.json"),
    include_str!("../themes/jellybeans.json"),
    include_str!("../themes/kibble.json"),
    include_str!("../themes/macos-classic.json"),
    include_str!("../themes/mellifluous.json"),
    include_str!("../themes/molokai.json"),
    include_str!("../themes/solarized.json"),
    include_str!("../themes/spaceduck.json"),
    include_str!("../themes/tokyonight.json"),
    include_str!("../themes/twilight.json"),
];

/// Theme files in a folder (your themes), as text.
pub fn user_themes(dir: &Path) -> Vec<String> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect())
        .unwrap_or_default();
    files.sort();
    files.iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect()
}

/// The themes in a file: (name, dark?).
pub fn names(file: &str) -> Vec<(String, bool)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(file) else { return Vec::new() };
    v["themes"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|t| Some((t["name"].as_str()?.to_string(), t["mode"].as_str() == Some("dark"))))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_built_in_theme_loads() {
        let mut registry = gpui_kit::component::ThemeRegistry::default();
        let mut count = 0;
        for file in BUILT_IN {
            registry.load_themes_from_str(file).expect("a theme file gpui-kit reads");
            count += names(file).len();
        }
        assert!(count >= 30, "{count} themes");
        assert_eq!(registry.themes().len(), count, "names are unique");
        assert!(names(BUILT_IN[5]).iter().any(|(n, dark)| n == "Catppuccin Mocha" && *dark));
    }
}
