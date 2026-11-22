//! Player settings (spec 2.10, "Settings"), saved as RON in the user's
//! config folder: `%APPDATA%\IronDots\settings.ron` on Windows,
//! `~/.config/iron-dots/settings.ron` elsewhere. Unknown or missing fields
//! fall back to defaults, so old files keep working.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use winit::keyboard::KeyCode;

/// Actions with a rebindable key (spec 2.9: "all keys can be rebound").
/// Mouse actions and the group keys (Ctrl+1–9) are fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Action {
    Stop,
    ClearQueue,
    BuyLight,
    BuyHeavy,
    Capital,
    Pause,
    SpeedUp,
    SpeedDown,
    CameraLeft,
    CameraRight,
    CameraUp,
    CameraDown,
    Fullscreen,
    DrawFront,
}

impl Action {
    pub const ALL: [Action; 14] = [
        Action::Stop,
        Action::ClearQueue,
        Action::BuyLight,
        Action::BuyHeavy,
        Action::Capital,
        Action::Pause,
        Action::SpeedUp,
        Action::SpeedDown,
        Action::CameraLeft,
        Action::CameraRight,
        Action::CameraUp,
        Action::CameraDown,
        Action::Fullscreen,
        Action::DrawFront,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Stop => "Stop",
            Action::ClearQueue => "Clear queued orders",
            Action::BuyLight => "Buy light unit",
            Action::BuyHeavy => "Buy heavy unit",
            Action::Capital => "Camera to capital",
            Action::Pause => "Pause (single player)",
            Action::SpeedUp => "Faster (single player)",
            Action::SpeedDown => "Slower (single player)",
            Action::CameraLeft => "Camera left",
            Action::CameraRight => "Camera right",
            Action::CameraUp => "Camera up",
            Action::CameraDown => "Camera down",
            Action::Fullscreen => "Full screen / window",
            Action::DrawFront => "Draw a front (positions)",
        }
    }

    pub fn default_key(self) -> KeyCode {
        match self {
            Action::Stop => KeyCode::KeyS,
            Action::ClearQueue => KeyCode::KeyC,
            Action::BuyLight => KeyCode::KeyQ,
            Action::BuyHeavy => KeyCode::KeyW,
            Action::Capital => KeyCode::Home,
            Action::Pause => KeyCode::Space,
            Action::SpeedUp => KeyCode::Equal,
            Action::SpeedDown => KeyCode::Minus,
            Action::CameraLeft => KeyCode::ArrowLeft,
            Action::CameraRight => KeyCode::ArrowRight,
            Action::CameraUp => KeyCode::ArrowUp,
            Action::CameraDown => KeyCode::ArrowDown,
            Action::Fullscreen => KeyCode::F11,
            Action::DrawFront => KeyCode::KeyF,
        }
    }
}

/// Keys that can be bound, with their names in the settings file and UI.
/// Digits and Escape are reserved (groups, menu).
const KEYS: &[(KeyCode, &str)] = {
    use KeyCode::*;
    &[
        (KeyA, "A"),
        (KeyB, "B"),
        (KeyC, "C"),
        (KeyD, "D"),
        (KeyE, "E"),
        (KeyF, "F"),
        (KeyG, "G"),
        (KeyH, "H"),
        (KeyI, "I"),
        (KeyJ, "J"),
        (KeyK, "K"),
        (KeyL, "L"),
        (KeyM, "M"),
        (KeyN, "N"),
        (KeyO, "O"),
        (KeyP, "P"),
        (KeyQ, "Q"),
        (KeyR, "R"),
        (KeyS, "S"),
        (KeyT, "T"),
        (KeyU, "U"),
        (KeyV, "V"),
        (KeyW, "W"),
        (KeyX, "X"),
        (KeyY, "Y"),
        (KeyZ, "Z"),
        (F1, "F1"),
        (F2, "F2"),
        (F3, "F3"),
        (F4, "F4"),
        (F5, "F5"),
        (F6, "F6"),
        (F7, "F7"),
        (F8, "F8"),
        (F9, "F9"),
        (F10, "F10"),
        (F11, "F11"),
        (F12, "F12"),
        (Space, "Space"),
        (Tab, "Tab"),
        (Enter, "Enter"),
        (Backspace, "Backspace"),
        (Home, "Home"),
        (End, "End"),
        (PageUp, "PageUp"),
        (PageDown, "PageDown"),
        (Insert, "Insert"),
        (Delete, "Delete"),
        (ArrowLeft, "Left"),
        (ArrowRight, "Right"),
        (ArrowUp, "Up"),
        (ArrowDown, "Down"),
        (Minus, "-"),
        (Equal, "="),
        (BracketLeft, "["),
        (BracketRight, "]"),
        (Backslash, "\\"),
        (Semicolon, ";"),
        (Quote, "'"),
        (Comma, ","),
        (Period, "."),
        (Slash, "/"),
        (Backquote, "`"),
        (Numpad0, "Num0"),
        (Numpad1, "Num1"),
        (Numpad2, "Num2"),
        (Numpad3, "Num3"),
        (Numpad4, "Num4"),
        (Numpad5, "Num5"),
        (Numpad6, "Num6"),
        (Numpad7, "Num7"),
        (Numpad8, "Num8"),
        (Numpad9, "Num9"),
        (NumpadAdd, "Num+"),
        (NumpadSubtract, "Num-"),
        (NumpadMultiply, "Num*"),
        (NumpadDivide, "Num/"),
        (NumpadEnter, "NumEnter"),
    ]
};

pub fn key_name(k: KeyCode) -> &'static str {
    KEYS.iter().find(|e| e.0 == k).map_or("?", |e| e.1)
}

fn key_by_name(name: &str) -> Option<KeyCode> {
    KEYS.iter().find(|e| e.1 == name).map(|e| e.0)
}

/// Whether `k` may be bound at all.
pub fn bindable(k: KeyCode) -> bool {
    KEYS.iter().any(|e| e.0 == k)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    // Graphics.
    pub fullscreen: bool,
    pub vsync: bool,
    /// Frames per second cap; 0 = none.
    pub fps_limit: u32,
    pub ui_scale: f32,
    // Controls.
    pub edge_scroll: bool,
    /// Camera speed multiplier for edge scrolling and the camera keys.
    pub scroll_speed: f32,
    /// Zoom step multiplier for the mouse wheel.
    pub zoom_speed: f32,
    /// `(action, key name)`; actions missing here use their default key.
    pub keys: Vec<(Action, String)>,
    // Sound (used once sound arrives).
    pub master_volume: f32,
    pub effects_volume: f32,
    pub music_volume: f32,
    pub alert_volume: f32,
    pub mute_unfocused: bool,
    // Online.
    pub name: String,
    pub server: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            fullscreen: true,
            vsync: true,
            fps_limit: 0,
            ui_scale: 1.0,
            edge_scroll: true,
            scroll_speed: 1.0,
            zoom_speed: 1.0,
            keys: Vec::new(),
            master_volume: 0.8,
            effects_volume: 0.8,
            music_volume: 0.5,
            alert_volume: 1.0,
            mute_unfocused: true,
            name: std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_else(|_| "Player".into()),
            server: format!("127.0.0.1:{}", net::DEFAULT_PORT),
        }
    }
}

impl Settings {
    pub fn key(&self, a: Action) -> KeyCode {
        self.keys
            .iter()
            .find(|(b, _)| *b == a)
            .and_then(|(_, name)| key_by_name(name))
            .unwrap_or_else(|| a.default_key())
    }

    /// The action bound to `k`, if any.
    pub fn action(&self, k: KeyCode) -> Option<Action> {
        Action::ALL.into_iter().find(|&a| self.key(a) == k)
    }

    /// Binds `k` to `a`; an action that had `k` gets `a`'s old key (swap).
    pub fn bind(&mut self, a: Action, k: KeyCode) {
        let old = self.key(a);
        if let Some(other) = self.action(k).filter(|&o| o != a) {
            self.set(other, old);
        }
        self.set(a, k);
    }

    fn set(&mut self, a: Action, k: KeyCode) {
        self.keys.retain(|(b, _)| *b != a);
        if k != a.default_key() {
            self.keys.push((a, key_name(k).to_string()));
        }
        self.keys.sort();
    }

    pub fn reset_keys(&mut self) {
        self.keys.clear();
    }

    /// Keeps every value in a sane range (hand-edited files).
    pub fn sanitize(&mut self) {
        self.ui_scale = self.ui_scale.clamp(0.75, 2.0);
        self.scroll_speed = self.scroll_speed.clamp(0.25, 4.0);
        self.zoom_speed = self.zoom_speed.clamp(0.25, 4.0);
        self.fps_limit = if self.fps_limit == 0 { 0 } else { self.fps_limit.clamp(30, 500) };
        for v in [&mut self.master_volume, &mut self.effects_volume, &mut self.music_volume, &mut self.alert_volume] {
            *v = v.clamp(0.0, 1.0);
        }
        self.name = self.name.chars().filter(|c| !c.is_control()).take(32).collect();
    }

    pub fn path() -> Option<PathBuf> {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return Some(PathBuf::from(appdata).join("IronDots").join("settings.ron"));
        }
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok()?;
        Some(base.join("iron-dots").join("settings.ron"))
    }

    /// Loads the settings file, or defaults if there is none or it is broken.
    pub fn load() -> Settings {
        let mut s = Settings::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| ron::from_str::<Settings>(&t).ok())
            .unwrap_or_default();
        s.sanitize();
        s
    }

    pub fn save(&self) {
        let Some(path) = Settings::path() else { return };
        let text = match ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::default()) {
            Ok(t) => t,
            Err(e) => return eprintln!("cannot encode settings: {e}"),
        };
        if let Err(e) = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&path, text))
        {
            eprintln!("cannot save {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebinding_swaps_conflicts_and_survives_a_round_trip() {
        let mut s = Settings::default();
        assert_eq!(s.action(KeyCode::KeyS), Some(Action::Stop));
        s.bind(Action::Stop, KeyCode::KeyC);
        assert_eq!(s.key(Action::Stop), KeyCode::KeyC);
        assert_eq!(s.key(Action::ClearQueue), KeyCode::KeyS, "swapped");
        let text = ron::to_string(&s).unwrap();
        let back: Settings = ron::from_str(&text).unwrap();
        assert_eq!(back, s);
        s.bind(Action::Stop, KeyCode::KeyS);
        assert!(s.keys.is_empty(), "back to defaults");
    }

    #[test]
    fn old_or_partial_files_load() {
        let s: Settings = ron::from_str("(vsync: false, keys: [(Stop, \"X\")])").unwrap();
        assert!(!s.vsync && s.fullscreen);
        assert_eq!(s.key(Action::Stop), KeyCode::KeyX);
    }
}
