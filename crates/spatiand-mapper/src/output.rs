//! What a layout can make happen, and the frame the engine hands back.

use serde::{Deserialize, Serialize};

use crate::keys;
use crate::layout::Group;

/// A button on the one virtual gamepad the game sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PadButton {
    A,
    B,
    X,
    Y,
    LeftBumper,
    RightBumper,
    LeftStick,
    RightStick,
    Start,
    Back,
    Guide,
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

impl PadButton {
    pub const ALL: [PadButton; 15] = [
        PadButton::A,
        PadButton::B,
        PadButton::X,
        PadButton::Y,
        PadButton::LeftBumper,
        PadButton::RightBumper,
        PadButton::LeftStick,
        PadButton::RightStick,
        PadButton::Start,
        PadButton::Back,
        PadButton::Guide,
        PadButton::DpadUp,
        PadButton::DpadDown,
        PadButton::DpadLeft,
        PadButton::DpadRight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PadButton::A => "A",
            PadButton::B => "B",
            PadButton::X => "X",
            PadButton::Y => "Y",
            PadButton::LeftBumper => "Left bumper",
            PadButton::RightBumper => "Right bumper",
            PadButton::LeftStick => "Left stick click",
            PadButton::RightStick => "Right stick click",
            PadButton::Start => "Start",
            PadButton::Back => "Select",
            PadButton::Guide => "Guide",
            PadButton::DpadUp => "D-pad up",
            PadButton::DpadDown => "D-pad down",
            PadButton::DpadLeft => "D-pad left",
            PadButton::DpadRight => "D-pad right",
        }
    }

    pub fn bit(self) -> u16 {
        1 << (self as u16)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Left,
    Right,
}

impl Side {
    pub fn label(self) -> &'static str {
        match self {
            Side::Left => "Left",
            Side::Right => "Right",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

impl Direction {
    pub const ALL: [Direction; 4] = [
        Direction::Up,
        Direction::Down,
        Direction::Left,
        Direction::Right,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Left => "left",
            Direction::Right => "right",
        }
    }

    /// As a unit vector, +y up.
    pub fn vector(self) -> (f32, f32) {
        match self {
            Direction::Up => (0.0, 1.0),
            Direction::Down => (0.0, -1.0),
            Direction::Left => (-1.0, 0.0),
            Direction::Right => (1.0, 0.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub const ALL: [MouseButton; 5] = [
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MouseButton::Left => "Left click",
            MouseButton::Right => "Right click",
            MouseButton::Middle => "Middle click",
            MouseButton::Back => "Mouse back",
            MouseButton::Forward => "Mouse forward",
        }
    }

    /// The evdev code a Wayland client expects in `wl_pointer.button`.
    pub fn code(self) -> u32 {
        match self {
            MouseButton::Left => 0x110,
            MouseButton::Right => 0x111,
            MouseButton::Middle => 0x112,
            MouseButton::Back => 0x116,
            MouseButton::Forward => 0x115,
        }
    }
}

/// Something only Spatiand can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Hud,
    Launcher,
    Keyboard,
    Screenshot,
    Recentre,
}

impl Command {
    pub const ALL: [Command; 5] = [
        Command::Hud,
        Command::Launcher,
        Command::Keyboard,
        Command::Screenshot,
        Command::Recentre,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Command::Hud => "Open settings",
            Command::Launcher => "Open the launcher",
            Command::Keyboard => "Show the keyboard",
            Command::Screenshot => "Take a screenshot",
            Command::Recentre => "Recentre",
        }
    }
}

/// One thing an activator does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// A button on the virtual gamepad.
    Pad { button: PadButton },
    /// A gamepad stick pushed all the way over, for a button standing in for a stick.
    Stick { side: Side, direction: Direction },
    /// A gamepad trigger pulled all the way.
    Trigger { side: Side },
    /// A keyboard key, by evdev code.
    Key { code: u16 },
    Mouse { button: MouseButton },
    /// One notch of the mouse wheel each time it fires.
    Wheel { direction: Direction },
    /// Switch to another action set.
    ActionSet { index: usize },
    /// A layer that applies while this is held.
    HoldLayer { index: usize },
    /// A layer that goes on with one press and off with the next.
    ToggleLayer { index: usize },
    Shell { command: Command },
}

impl Action {
    /// What the action is, in the words a picker shows.
    pub fn label(&self) -> String {
        match *self {
            Action::Pad { button } => format!("Gamepad {}", button.label()),
            Action::Stick { side, direction } => {
                format!("{} stick {}", side.label(), direction.label())
            }
            Action::Trigger { side } => format!("{} trigger", side.label()),
            Action::Key { code } => keys::name(code)
                .map(|n| format!("Key {n}"))
                .unwrap_or_else(|| format!("Key {code}")),
            Action::Mouse { button } => button.label().into(),
            Action::Wheel { direction } => format!("Scroll {}", direction.label()),
            Action::ActionSet { index } => format!("Action set {}", index + 1),
            Action::HoldLayer { index } => format!("Hold layer {}", index + 1),
            Action::ToggleLayer { index } => format!("Toggle layer {}", index + 1),
            Action::Shell { command } => command.label().into(),
        }
    }

    /// A few characters for a callout on the controller picture.
    pub fn short_label(&self) -> String {
        match *self {
            Action::Pad { button } => match button {
                PadButton::LeftBumper => "LB".into(),
                PadButton::RightBumper => "RB".into(),
                PadButton::LeftStick => "LS".into(),
                PadButton::RightStick => "RS".into(),
                PadButton::DpadUp => "D-up".into(),
                PadButton::DpadDown => "D-down".into(),
                PadButton::DpadLeft => "D-left".into(),
                PadButton::DpadRight => "D-right".into(),
                other => other.label().into(),
            },
            Action::Key { code } => keys::name(code).map(String::from).unwrap_or_else(|| code.to_string()),
            Action::Mouse { button } => match button {
                MouseButton::Left => "LMB".into(),
                MouseButton::Right => "RMB".into(),
                MouseButton::Middle => "MMB".into(),
                MouseButton::Back => "M4".into(),
                MouseButton::Forward => "M5".into(),
            },
            Action::Trigger { side } => match side {
                Side::Left => "LT".into(),
                Side::Right => "RT".into(),
            },
            _ => self.label(),
        }
    }
}

/// The virtual gamepad, as of one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PadState {
    pub buttons: u16,
    /// −1..1, +y up.
    pub left: (f32, f32),
    pub right: (f32, f32),
    /// 0..1.
    pub left_trigger: f32,
    pub right_trigger: f32,
}

impl PadState {
    pub fn is_down(&self, button: PadButton) -> bool {
        self.buttons & button.bit() != 0
    }

    pub fn is_neutral(&self) -> bool {
        *self == PadState::default()
    }
}

/// A radial menu that is open, for the compositor to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct RadialView {
    pub group: Group,
    pub labels: Vec<String>,
    pub selected: Option<usize>,
}

/// Everything a layout asks for this frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    pub pad: PadState,
    /// Keys held down, by evdev code, sorted.
    pub keys: Vec<u16>,
    /// Mouse buttons held down.
    pub mouse_buttons: Vec<MouseButton>,
    /// Whole pixels of relative mouse motion, +y down as a screen has it.
    pub mouse_motion: (i32, i32),
    /// Wheel notches, `(horizontal, vertical)`, +y up.
    pub wheel: (i32, i32),
    /// Spatiand commands that fired this frame.
    pub commands: Vec<Command>,
    /// Which triggers click Spatiand's pointer, `[left, right]`. The trackpads always *are*
    /// that pointer and are never a layout's to take.
    pub pointer_clicks: [bool; 2],
    pub radial: Option<RadialView>,
}
