//! A layout, as data: what it is on disk and what the editor edits.
//!
//! Serialised to TOML, one file per application. Every setting has a default, so a file that
//! names only a mode is a complete description, and a file written by an older Spatiand still
//! loads after a setting is added.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::input::Button;
use crate::output::{Action, Direction, PadButton, Side};

/// An analogue source, which runs in a [`Mode`] rather than having a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Group {
    LeftStick,
    RightStick,
    LeftTrigger,
    RightTrigger,
    /// The Deck's own gyro, or a Bluetooth pad's.
    Gyro,
    /// The glasses' gyro — the head, not the hands.
    GlassesGyro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupKind {
    Stick,
    Trigger,
    Gyro,
}

impl Group {
    pub const ALL: [Group; 6] = [
        Group::LeftStick,
        Group::RightStick,
        Group::LeftTrigger,
        Group::RightTrigger,
        Group::Gyro,
        Group::GlassesGyro,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Group::LeftStick => "Left stick",
            Group::RightStick => "Right stick",
            Group::LeftTrigger => "Left trigger",
            Group::RightTrigger => "Right trigger",
            Group::Gyro => "Gyro",
            Group::GlassesGyro => "Glasses gyro",
        }
    }

    pub fn kind(self) -> GroupKind {
        match self {
            Group::LeftStick | Group::RightStick => GroupKind::Stick,
            Group::LeftTrigger | Group::RightTrigger => GroupKind::Trigger,
            Group::Gyro | Group::GlassesGyro => GroupKind::Gyro,
        }
    }

    /// Which hand, for the defaults that depend on it. The gyros count as right-handed: the
    /// right stick is the camera on every gamepad.
    pub fn side(self) -> Side {
        match self {
            Group::LeftStick | Group::LeftTrigger => Side::Left,
            _ => Side::Right,
        }
    }

    /// The modes that mean something for this source, in the order a picker offers them.
    pub fn modes(self) -> &'static [ModeKind] {
        match self.kind() {
            GroupKind::Stick => &[
                ModeKind::None,
                ModeKind::Joystick,
                ModeKind::Dpad,
                ModeKind::Mouse,
                ModeKind::ScrollWheel,
                ModeKind::FlickStick,
                ModeKind::RadialMenu,
            ],
            GroupKind::Trigger => &[ModeKind::None, ModeKind::Trigger, ModeKind::PointerClick],
            GroupKind::Gyro => &[ModeKind::None, ModeKind::Gyro],
        }
    }
}

/// A whole layout for one application.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Never empty; the first is where a layout starts.
    pub sets: Vec<ActionSet>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<Layer>,
}

impl Layout {
    /// What applies with this action set and these layers on, later layers winning.
    pub fn effective(&self, set: usize, layers: impl IntoIterator<Item = usize>) -> Controls {
        let mut controls = self
            .sets
            .get(set)
            .or_else(|| self.sets.first())
            .map(|s| s.controls.clone())
            .unwrap_or_default();
        for index in layers {
            if let Some(layer) = self.layers.get(index) {
                for (button, binding) in &layer.controls.buttons {
                    controls.buttons.insert(*button, binding.clone());
                }
                for (group, config) in &layer.controls.groups {
                    controls.groups.insert(*group, config.clone());
                }
            }
        }
        controls
    }

    /// Put right anything a hand-edited file could get wrong: no action sets at all.
    pub fn repaired(mut self) -> Self {
        if self.sets.is_empty() {
            self.sets.push(ActionSet {
                name: "Default".into(),
                controls: Controls::default(),
            });
        }
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionSet {
    pub name: String,
    #[serde(default)]
    pub controls: Controls,
}

/// Controls that override the action set underneath while the layer is on. Anything not named
/// here is left as the set has it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    pub name: String,
    #[serde(default)]
    pub controls: Controls,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Controls {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub buttons: BTreeMap<Button, Binding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<Group, GroupConfig>,
}

/// What one button — or one direction of a directional pad, one pull of a trigger — does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    #[serde(default)]
    pub activators: Vec<Activator>,
}

impl Binding {
    pub fn press(action: Action) -> Self {
        Self {
            activators: vec![Activator {
                actions: vec![action],
                ..Default::default()
            }],
        }
    }

    pub fn pad(button: PadButton) -> Self {
        Self::press(Action::Pad { button })
    }

    pub fn key(code: u16) -> Self {
        Self::press(Action::Key { code })
    }

    pub fn is_empty(&self) -> bool {
        self.activators.iter().all(|a| a.actions.is_empty())
    }

    /// One line saying what this does.
    pub fn summary(&self) -> String {
        let bound: Vec<&Activator> = self
            .activators
            .iter()
            .filter(|a| !a.actions.is_empty())
            .collect();
        let Some(first) = bound.first() else {
            return "Unbound".into();
        };
        let mut text = first.summary();
        if bound.len() > 1 {
            text.push_str(&format!(" (+{} more)", bound.len() - 1));
        }
        text
    }

    /// A few characters for the controller picture.
    pub fn short(&self) -> String {
        self.activators
            .iter()
            .find_map(|a| a.actions.first())
            .map(|a| a.short_label())
            .unwrap_or_default()
    }
}

/// When a binding fires, and what it does then.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Activator {
    #[serde(default)]
    pub when: When,
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Each activation turns the actions on, and the next turns them off.
    #[serde(default, skip_serializing_if = "is_false")]
    pub toggle: bool,
    /// While active, the actions pulse on and off with this period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turbo_ms: Option<u32>,
}

impl Activator {
    pub fn summary(&self) -> String {
        let actions = if self.actions.is_empty() {
            "Nothing".to_string()
        } else {
            self.actions
                .iter()
                .map(|a| a.label())
                .collect::<Vec<_>>()
                .join(" + ")
        };
        match self.when {
            When::Press => actions,
            other => format!("{}: {actions}", other.label()),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Steam's activators.
///
/// When a button has a long press or a double press as well as a plain press, the plain press
/// waits to find out which it was and fires as a short tap on release. Firing it at once — the
/// other possible reading — would make every long press also a short one, and a layout that
/// puts "reload" on a tap and "switch weapon" on a hold would do both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    #[default]
    Press,
    LongPress {
        ms: u32,
    },
    DoublePress {
        ms: u32,
    },
    /// Once, as the button goes down.
    StartPress,
    /// Once, as the button comes up.
    ReleasePress,
    /// While this button and another are both held.
    Chord {
        with: Button,
    },
}

impl When {
    pub const LONG_MS: u32 = 400;
    pub const DOUBLE_MS: u32 = 250;

    pub fn label(self) -> &'static str {
        match self {
            When::Press => "Regular press",
            When::LongPress { .. } => "Long press",
            When::DoublePress { .. } => "Double press",
            When::StartPress => "Start press",
            When::ReleasePress => "Release press",
            When::Chord { .. } => "Chorded press",
        }
    }
}

/// One source's mode, and the mode it shifts to while a button is held.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupConfig {
    #[serde(flatten)]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift: Option<ModeShift>,
}

impl GroupConfig {
    pub fn new(mode: Mode) -> Self {
        Self { mode, shift: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeShift {
    pub button: Button,
    #[serde(flatten)]
    pub mode: Mode,
}

/// How a stick responds between the dead zone and the rim.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Curve {
    #[default]
    Linear,
    /// Reaches speed early: quick turns, less fine control.
    Aggressive,
    /// Stays slow near the centre: fine aim, a long push to go fast.
    Relaxed,
}

impl Curve {
    pub const ALL: [Curve; 3] = [Curve::Linear, Curve::Aggressive, Curve::Relaxed];

    pub fn exponent(self) -> f64 {
        match self {
            Curve::Linear => 1.0,
            Curve::Aggressive => 0.6,
            Curve::Relaxed => 1.8,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Curve::Linear => "Linear",
            Curve::Aggressive => "Aggressive",
            Curve::Relaxed => "Relaxed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GyroOutput {
    /// Turning moves the mouse, the way aiming a pointer would.
    Mouse,
    /// Turning pushes a stick, harder the faster — a camera that follows the hands.
    Camera { side: Side },
    /// Tilting from where it was enabled holds a stick over — steering.
    Tilt { side: Side },
}

impl GyroOutput {
    pub fn label(self) -> String {
        match self {
            GyroOutput::Mouse => "Mouse".into(),
            GyroOutput::Camera { side } => format!("{} stick camera", side.label()),
            GyroOutput::Tilt { side } => format!("{} stick tilt", side.label()),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GyroEnable {
    #[default]
    Always,
    WhileHeld {
        button: Button,
    },
    Toggle {
        button: Button,
    },
    OffWhileHeld {
        button: Button,
    },
}

impl GyroEnable {
    pub fn label(self) -> &'static str {
        match self {
            GyroEnable::Always => "Always on",
            GyroEnable::WhileHeld { .. } => "On while held",
            GyroEnable::Toggle { .. } => "Toggle",
            GyroEnable::OffWhileHeld { .. } => "Off while held",
        }
    }

    pub fn button(self) -> Option<Button> {
        match self {
            GyroEnable::Always => None,
            GyroEnable::WhileHeld { button }
            | GyroEnable::Toggle { button }
            | GyroEnable::OffWhileHeld { button } => Some(button),
        }
    }
}

/// Which rotation turns the output left and right.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GyroAxis {
    /// Turning, as you would a torch.
    #[default]
    Yaw,
    /// Tilting, as you would a steering wheel.
    Roll,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadialItem {
    pub label: String,
    #[serde(default)]
    pub actions: Vec<Action>,
}

/// What an analogue source does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    None,
    /// A gamepad stick.
    Joystick {
        output: Side,
        #[serde(default = "one")]
        sensitivity: f64,
        #[serde(default = "stick_deadzone")]
        deadzone: f64,
        #[serde(default = "outer_ring")]
        outer: f64,
        #[serde(default)]
        curve: Curve,
        #[serde(default)]
        invert_x: bool,
        #[serde(default)]
        invert_y: bool,
    },
    /// Four bindings, one per direction.
    Dpad {
        #[serde(default)]
        up: Binding,
        #[serde(default)]
        down: Binding,
        #[serde(default)]
        left: Binding,
        #[serde(default)]
        right: Binding,
        #[serde(default = "dpad_deadzone")]
        deadzone: f64,
        #[serde(default)]
        eight_way: bool,
    },
    /// Relative mouse motion, by how far the stick is pushed over.
    Mouse {
        #[serde(default = "one")]
        sensitivity: f64,
        #[serde(default)]
        invert_x: bool,
        #[serde(default)]
        invert_y: bool,
    },
    /// Circling the thumb turns a wheel.
    ScrollWheel {
        #[serde(default = "notch_degrees")]
        degrees_per_notch: f64,
        #[serde(default)]
        clockwise: Binding,
        #[serde(default)]
        counter_clockwise: Binding,
    },
    /// Push the stick to face that way, then rotate it to turn: camera by mouse.
    FlickStick {
        #[serde(default = "turn_pixels")]
        pixels_per_turn: f64,
    },
    /// Items round a circle; the thumb picks one and it fires on release or on click.
    RadialMenu {
        #[serde(default)]
        items: Vec<RadialItem>,
        #[serde(default = "yes")]
        on_release: bool,
    },
    /// The trigger clicks the spatial pointer.
    PointerClick,
    /// A gamepad trigger, and bindings for a soft and a full pull.
    Trigger {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<Side>,
        #[serde(default = "soft_pull")]
        soft_threshold: f64,
        #[serde(default)]
        deadzone: f64,
        #[serde(default)]
        soft: Binding,
        #[serde(default)]
        full: Binding,
    },
    Gyro {
        output: GyroOutput,
        #[serde(default)]
        enable: GyroEnable,
        #[serde(default = "one")]
        sensitivity: f64,
        #[serde(default)]
        horizontal: GyroAxis,
        /// Degrees per second below which the hand is taken to be still.
        #[serde(default = "gyro_deadzone")]
        deadzone: f64,
        #[serde(default)]
        invert_x: bool,
        #[serde(default)]
        invert_y: bool,
    },
}

fn one() -> f64 {
    1.0
}
fn yes() -> bool {
    true
}
fn stick_deadzone() -> f64 {
    0.08
}
fn outer_ring() -> f64 {
    0.95
}
fn dpad_deadzone() -> f64 {
    0.3
}
fn notch_degrees() -> f64 {
    30.0
}
fn turn_pixels() -> f64 {
    3600.0
}
fn soft_pull() -> f64 {
    0.5
}
fn gyro_deadzone() -> f64 {
    0.5
}

/// A mode without its settings, for pickers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeKind {
    None,
    Joystick,
    Dpad,
    Mouse,
    ScrollWheel,
    FlickStick,
    RadialMenu,
    PointerClick,
    Trigger,
    Gyro,
}

impl ModeKind {
    pub fn of(mode: &Mode) -> Self {
        match mode {
            Mode::None => ModeKind::None,
            Mode::Joystick { .. } => ModeKind::Joystick,
            Mode::Dpad { .. } => ModeKind::Dpad,
            Mode::Mouse { .. } => ModeKind::Mouse,
            Mode::ScrollWheel { .. } => ModeKind::ScrollWheel,
            Mode::FlickStick { .. } => ModeKind::FlickStick,
            Mode::RadialMenu { .. } => ModeKind::RadialMenu,
            Mode::PointerClick => ModeKind::PointerClick,
            Mode::Trigger { .. } => ModeKind::Trigger,
            Mode::Gyro { .. } => ModeKind::Gyro,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ModeKind::None => "None",
            ModeKind::Joystick => "Joystick",
            ModeKind::Dpad => "Directional pad",
            ModeKind::Mouse => "Mouse",
            ModeKind::ScrollWheel => "Scroll wheel",
            ModeKind::FlickStick => "Flick stick",
            ModeKind::RadialMenu => "Radial menu",
            ModeKind::PointerClick => "Pointer click",
            ModeKind::Trigger => "Trigger",
            ModeKind::Gyro => "Gyro",
        }
    }

    pub fn detail(self) -> &'static str {
        match self {
            ModeKind::None => "Does nothing, and nothing reaches the game from it.",
            ModeKind::Joystick => "Drives a gamepad stick, with a dead zone, an outer ring and a response curve.",
            ModeKind::Dpad => "Four bindings, one for each direction you push.",
            ModeKind::Mouse => "Moves the mouse: a trackpad by how far your thumb travels, a stick by how far it is pushed.",
            ModeKind::ScrollWheel => "Circle your thumb to turn a wheel; each notch fires a binding.",
            ModeKind::FlickStick => "Push the stick to turn that way at once, then rotate it to keep turning.",
            ModeKind::RadialMenu => "Items around a circle. Point at one and let go to fire it.",
            ModeKind::PointerClick => "Clicks the spatial pointer, as on the desktop.",
            ModeKind::Trigger => "Drives a gamepad trigger, with bindings for a soft and a full pull.",
            ModeKind::Gyro => "Turns the movement of the device into mouse or stick movement.",
        }
    }

    /// This mode with sensible settings for this source.
    pub fn default_mode(self, group: Group) -> Mode {
        let side = group.side();
        match self {
            ModeKind::None => Mode::None,
            ModeKind::Joystick => Mode::Joystick {
                output: side,
                sensitivity: 1.0,
                deadzone: stick_deadzone(),
                outer: outer_ring(),
                curve: Curve::Linear,
                invert_x: false,
                invert_y: false,
            },
            ModeKind::Dpad => Mode::Dpad {
                up: Binding::pad(PadButton::DpadUp),
                down: Binding::pad(PadButton::DpadDown),
                left: Binding::pad(PadButton::DpadLeft),
                right: Binding::pad(PadButton::DpadRight),
                deadzone: dpad_deadzone(),
                eight_way: false,
            },
            ModeKind::Mouse => Mode::Mouse {
                sensitivity: 1.0,
                invert_x: false,
                invert_y: false,
            },
            ModeKind::ScrollWheel => Mode::ScrollWheel {
                degrees_per_notch: notch_degrees(),
                clockwise: Binding::press(Action::Wheel {
                    direction: Direction::Down,
                }),
                counter_clockwise: Binding::press(Action::Wheel {
                    direction: Direction::Up,
                }),
            },
            ModeKind::FlickStick => Mode::FlickStick {
                pixels_per_turn: turn_pixels(),
            },
            ModeKind::RadialMenu => Mode::RadialMenu {
                items: (0..4)
                    .map(|i| {
                        let (code, name) = crate::keys::KEYS
                            .iter()
                            .find(|(_, n)| *n == ["1", "2", "3", "4"][i])
                            .copied()
                            .unwrap_or((2, "1"));
                        RadialItem {
                            label: format!("Key {name}"),
                            actions: vec![Action::Key { code }],
                        }
                    })
                    .collect(),
                on_release: true,
            },
            ModeKind::PointerClick => Mode::PointerClick,
            ModeKind::Trigger => Mode::Trigger {
                output: Some(side),
                soft_threshold: soft_pull(),
                deadzone: 0.0,
                soft: Binding::default(),
                full: Binding::default(),
            },
            ModeKind::Gyro => Mode::Gyro {
                output: GyroOutput::Mouse,
                // The Deck's gyro is on while a thumb rests on the right pad, as Steam's gyro
                // templates have it: hands on a handheld move all the time, and a camera that
                // followed every one of those movements would be unplayable. The glasses are
                // different -- choosing their gyro at all is choosing to look around with it.
                enable: if group == Group::Gyro {
                    GyroEnable::WhileHeld {
                        button: Button::RPadTouch,
                    }
                } else {
                    GyroEnable::Always
                },
                sensitivity: 1.0,
                horizontal: GyroAxis::Yaw,
                deadzone: gyro_deadzone(),
                invert_x: false,
                invert_y: false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_mode_is_one_its_source_can_run() {
        for group in Group::ALL {
            for kind in group.modes() {
                assert_eq!(ModeKind::of(&kind.default_mode(group)), *kind);
            }
        }
    }

    #[test]
    fn a_layer_overrides_only_what_it_names() {
        let mut base = Controls::default();
        base.buttons.insert(Button::A, Binding::pad(PadButton::A));
        base.buttons.insert(Button::B, Binding::pad(PadButton::B));
        let mut over = Controls::default();
        over.buttons.insert(Button::A, Binding::key(57));
        let layout = Layout {
            name: "t".into(),
            description: String::new(),
            sets: vec![ActionSet {
                name: "s".into(),
                controls: base,
            }],
            layers: vec![Layer {
                name: "l".into(),
                controls: over,
            }],
        };
        let plain = layout.effective(0, []);
        let layered = layout.effective(0, [0]);
        assert_eq!(plain.buttons[&Button::A], Binding::pad(PadButton::A));
        assert_eq!(layered.buttons[&Button::A], Binding::key(57));
        assert_eq!(layered.buttons[&Button::B], Binding::pad(PadButton::B));
    }

    #[test]
    fn a_file_naming_only_a_mode_loads_with_every_default() {
        let text = r#"
            name = "Hand written"
            [[sets]]
            name = "Default"
            [sets.controls.groups.left_stick]
            mode = "joystick"
            output = "left"
            [sets.controls.buttons.a]
            activators = [{ actions = [{ type = "pad", button = "a" }] }]
        "#;
        let layout: Layout = toml::from_str(text).expect("parses");
        let config = &layout.sets[0].controls.groups[&Group::LeftStick];
        match &config.mode {
            Mode::Joystick {
                sensitivity,
                deadzone,
                ..
            } => {
                assert_eq!(*sensitivity, 1.0);
                assert_eq!(*deadzone, stick_deadzone());
            }
            other => panic!("wrong mode {other:?}"),
        }
        assert_eq!(
            layout.sets[0].controls.buttons[&Button::A],
            Binding::pad(PadButton::A)
        );
    }
}
