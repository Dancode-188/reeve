use ratatui::style::Color;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(serde::Deserialize)]
struct RawTheme {
    name: String,
    colors: HashMap<String, String>,
    semantic: HashMap<String, String>,
}

pub struct Theme {
    pub name: String,
    colors: HashMap<String, Color>,
    semantic: HashMap<String, String>,
}

impl Theme {
    pub fn load() -> Self {
        let user_path = std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".config/reeve/theme.toml"));

        if let Some(path) = user_path {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(theme) = Self::from_str(&content) {
                    return theme;
                }
            }
        }

        Self::from_str(include_str!("../../../themes/mocha.toml"))
            .expect("embedded mocha theme must always parse")
    }

    fn from_str(content: &str) -> Result<Self, String> {
        let raw: RawTheme = toml::from_str(content).map_err(|e| e.to_string())?;
        let colors: HashMap<String, Color> = raw
            .colors
            .iter()
            .filter_map(|(k, v)| parse_hex(v).map(|c| (k.clone(), c)))
            .collect();
        Ok(Theme {
            name: raw.name,
            colors,
            semantic: raw.semantic,
        })
    }

    pub fn get(&self, key: &str) -> Color {
        if let Some(alias) = self.semantic.get(key) {
            if let Some(&c) = self.colors.get(alias.as_str()) {
                return c;
            }
        }
        if let Some(&c) = self.colors.get(key) {
            return c;
        }
        Color::Magenta
    }

    pub fn background(&self) -> Color {
        self.get("background")
    }
    pub fn text(&self) -> Color {
        self.get("text")
    }
    pub fn subtext(&self) -> Color {
        self.get("subtext")
    }
    pub fn surface(&self) -> Color {
        self.get("surface")
    }
    pub fn border_focused(&self) -> Color {
        self.get("border_focused")
    }
    pub fn border_idle(&self) -> Color {
        self.get("border_idle")
    }
    pub fn highlight(&self) -> Color {
        self.get("highlight")
    }
    pub fn health_ok(&self) -> Color {
        self.get("health_ok")
    }
    pub fn health_warn(&self) -> Color {
        self.get("health_warn")
    }
    pub fn health_alert(&self) -> Color {
        self.get("health_alert")
    }
    pub fn health_crit(&self) -> Color {
        self.get("health_crit")
    }
    pub fn span_active(&self) -> Color {
        self.get("span_active")
    }
    pub fn span_complete(&self) -> Color {
        self.get("span_complete")
    }
    pub fn span_error(&self) -> Color {
        self.get("span_error")
    }
}

fn parse_hex(s: &str) -> Option<Color> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_mocha_theme_parses() {
        let theme = Theme::load();
        assert_eq!(theme.name, "mocha");
        assert!(matches!(theme.background(), Color::Rgb(_, _, _)));
    }

    #[test]
    fn semantic_key_resolves_through_alias() {
        let theme = Theme::load();
        // health_ok -> green -> #a6e3a1
        let color = theme.health_ok();
        assert!(matches!(color, Color::Rgb(_, _, _)));
        assert_ne!(color, Color::Magenta, "missing key falls back to magenta");
    }
}
