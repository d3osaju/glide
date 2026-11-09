use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    pub fn opposite(self) -> Dir {
        match self {
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Name of the machine running the server (the one with the physical keyboard/mouse).
    pub name: String,
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Base64 32-byte key from `glide genkey`. Can also come from GLIDE_KEY.
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default = "yes")]
    pub clipboard: bool,
    /// Don't switch screens while a mouse button is held (prevents stuck drags).
    #[serde(default = "yes")]
    pub block_switch_while_dragging: bool,
    #[serde(default)]
    pub screens: HashMap<String, ScreenCfg>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenCfg {
    pub left: Option<String>,
    pub right: Option<String>,
    pub up: Option<String>,
    pub down: Option<String>,
    /// Pointer speed multiplier while on this screen (e.g. 0.6 for a Retina Mac).
    #[serde(default = "one")]
    pub speed: f64,
}

fn default_listen() -> String {
    "0.0.0.0:24800".into()
}
fn yes() -> bool {
    true
}
fn one() -> f64 {
    1.0
}

impl ScreenCfg {
    fn get(&self, d: Dir) -> Option<&str> {
        match d {
            Dir::Left => self.left.as_deref(),
            Dir::Right => self.right.as_deref(),
            Dir::Up => self.up.as_deref(),
            Dir::Down => self.down.as_deref(),
        }
    }

    pub fn set(&mut self, d: Dir, v: Option<String>) {
        match d {
            Dir::Left => self.left = v,
            Dir::Right => self.right = v,
            Dir::Up => self.up = v,
            Dir::Down => self.down = v,
        }
    }
}

/// Screen arrangement, shared with the router and editable at runtime.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The server's own screen name.
    pub me: String,
    pub screens: HashMap<String, ScreenCfg>,
}

impl Layout {
    pub fn new(me: &str) -> Self {
        Layout { me: me.to_string(), screens: HashMap::new() }
    }

    /// Neighbour of `from` in direction `d`. Links are mirrored automatically:
    /// `desktop.right = "laptop"` implies `laptop.left = "desktop"`.
    pub fn neighbor(&self, from: &str, d: Dir) -> Option<&str> {
        if let Some(n) = self.screens.get(from).and_then(|s| s.get(d)) {
            return Some(n);
        }
        self.screens
            .iter()
            .find(|(_, s)| s.get(d.opposite()) == Some(from))
            .map(|(name, _)| name.as_str())
    }

    pub fn speed(&self, name: &str) -> f64 {
        self.screens.get(name).map(|s| s.speed).unwrap_or(1.0)
    }

    /// Place `name` on side `d` of `anchor`, removing any previous placement of it.
    pub fn place(&mut self, name: &str, anchor: &str, d: Dir) {
        for s in self.screens.values_mut() {
            for dir in [Dir::Left, Dir::Right, Dir::Up, Dir::Down] {
                if s.get(dir) == Some(name) {
                    s.set(dir, None);
                }
            }
        }
        if let Some(s) = self.screens.get_mut(name) {
            s.set(d.opposite(), None);
        }
        self.screens.entry(anchor.to_string()).or_insert_with(|| ScreenCfg { speed: 1.0, ..Default::default() }).set(d, Some(name.to_string()));
    }
}

impl ServerConfig {
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {path}: {e}"))?;
        let cfg: ServerConfig = toml::from_str(&text).map_err(|e| format!("parsing {path}: {e}"))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        for (name, s) in &self.screens {
            if !(0.05..=20.0).contains(&s.speed) {
                return Err(format!("screens.{name}.speed must be between 0.05 and 20"));
            }
        }
        Ok(())
    }

    pub fn layout(&self) -> Layout {
        Layout {
            me: self.name.clone(),
            screens: self.screens.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg: ServerConfig = toml::from_str(include_str!("../glide.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.layout().neighbor("macbook", Dir::Left), Some("laptop"));
    }

    #[test]
    fn mirrored_links() {
        let cfg: ServerConfig = toml::from_str(
            r#"
            name = "desk"
            [screens.desk]
            right = "laptop"
            [screens.laptop]
            right = "mac"
            speed = 0.5
            "#,
        )
        .unwrap();
        let l = cfg.layout();
        assert_eq!(l.neighbor("desk", Dir::Right), Some("laptop"));
        assert_eq!(l.neighbor("laptop", Dir::Left), Some("desk"));
        assert_eq!(l.neighbor("mac", Dir::Left), Some("laptop"));
        assert_eq!(l.neighbor("desk", Dir::Up), None);
        assert_eq!(l.speed("laptop"), 0.5);
        assert_eq!(l.speed("mac"), 1.0);
    }

    #[test]
    fn place_moves_a_screen() {
        let mut l = Layout::new("desk");
        l.place("laptop", "desk", Dir::Right);
        assert_eq!(l.neighbor("desk", Dir::Right), Some("laptop"));
        l.place("laptop", "desk", Dir::Left);
        assert_eq!(l.neighbor("desk", Dir::Right), None);
        assert_eq!(l.neighbor("desk", Dir::Left), Some("laptop"));
        assert_eq!(l.neighbor("laptop", Dir::Right), Some("desk"));
    }
}
