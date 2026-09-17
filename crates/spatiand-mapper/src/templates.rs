//! The layouts a new application starts from, as Steam offers templates.

use crate::input::Button;
use crate::layout::{
    ActionSet, Binding, Controls, Group, GroupConfig, GyroEnable, GyroOutput, Layout, Mode,
    ModeKind,
};
use crate::output::{Action, Direction, MouseButton, PadButton, Side};

/// Every template, in the order the editor lists them.
pub fn all() -> Vec<Layout> {
    vec![gamepad(), gamepad_with_gyro(), keyboard_and_mouse(), desktop()]
}

fn single_set(name: &str, description: &str, controls: Controls) -> Layout {
    Layout {
        name: name.into(),
        description: description.into(),
        sets: vec![ActionSet {
            name: "Default".into(),
            controls,
        }],
        layers: Vec::new(),
    }
}

fn gamepad_controls() -> Controls {
    let mut c = Controls::default();
    for (button, pad) in [
        (Button::A, PadButton::A),
        (Button::B, PadButton::B),
        (Button::X, PadButton::X),
        (Button::Y, PadButton::Y),
        (Button::DpadUp, PadButton::DpadUp),
        (Button::DpadDown, PadButton::DpadDown),
        (Button::DpadLeft, PadButton::DpadLeft),
        (Button::DpadRight, PadButton::DpadRight),
        (Button::L1, PadButton::LeftBumper),
        (Button::R1, PadButton::RightBumper),
        (Button::LStick, PadButton::LeftStick),
        (Button::RStick, PadButton::RightStick),
        (Button::Menu, PadButton::Start),
        (Button::View, PadButton::Back),
    ] {
        c.buttons.insert(button, Binding::pad(pad));
    }
    for group in [
        Group::LeftStick,
        Group::RightStick,
        Group::LeftTrigger,
        Group::RightTrigger,
    ] {
        let kind = if group.kind() == crate::layout::GroupKind::Trigger {
            ModeKind::Trigger
        } else {
            ModeKind::Joystick
        };
        c.groups
            .insert(group, GroupConfig::new(kind.default_mode(group)));
    }
    c
}

pub fn gamepad() -> Layout {
    single_set(
        "Gamepad",
        "For games with controller support. Every control does what its gamepad twin does.",
        gamepad_controls(),
    )
}

pub fn gamepad_with_gyro() -> Layout {
    let mut c = gamepad_controls();
    c.groups.insert(
        Group::Gyro,
        GroupConfig::new(Mode::Gyro {
            output: GyroOutput::Camera { side: Side::Right },
            enable: GyroEnable::WhileHeld {
                button: Button::RPadTouch,
            },
            sensitivity: 1.0,
            horizontal: Default::default(),
            deadzone: 0.5,
            invert_x: false,
            invert_y: false,
        }),
    );
    single_set(
        "Gamepad with gyro aiming",
        "A gamepad, plus aiming by moving the Deck while your thumb rests on the right trackpad.",
        c,
    )
}

pub fn keyboard_and_mouse() -> Layout {
    let mut c = Controls::default();
    let keys = [
        (Button::A, 57),        // Space
        (Button::B, 46),        // C
        (Button::X, 19),        // R
        (Button::Y, 33),        // F
        (Button::L1, 16),       // Q
        (Button::R1, 18),       // E
        (Button::DpadUp, 2),    // 1
        (Button::DpadRight, 3), // 2
        (Button::DpadDown, 4),  // 3
        (Button::DpadLeft, 5),  // 4
        (Button::Menu, 1),      // Esc
        (Button::View, 15),     // Tab
        (Button::LStick, 42),   // Left Shift
        (Button::L4, 42),       // Left Shift
        (Button::R4, 29),       // Left Ctrl
        (Button::L5, 44),       // Z
        (Button::R5, 47),       // V
    ];
    for (button, code) in keys {
        c.buttons.insert(button, Binding::key(code));
    }
    c.buttons.insert(
        Button::RStick,
        Binding::press(Action::Mouse {
            button: MouseButton::Middle,
        }),
    );
    c.groups.insert(
        Group::LeftStick,
        GroupConfig::new(Mode::Dpad {
            up: Binding::key(17),
            down: Binding::key(31),
            left: Binding::key(30),
            right: Binding::key(32),
            deadzone: 0.3,
            eight_way: true,
        }),
    );
    c.groups.insert(
        Group::RightStick,
        GroupConfig::new(ModeKind::Mouse.default_mode(Group::RightStick)),
    );
    for (group, button) in [
        (Group::RightTrigger, MouseButton::Left),
        (Group::LeftTrigger, MouseButton::Right),
    ] {
        c.groups.insert(
            group,
            GroupConfig::new(Mode::Trigger {
                output: None,
                soft_threshold: 0.4,
                deadzone: 0.0,
                soft: Binding::press(Action::Mouse { button }),
                full: Binding::default(),
            }),
        );
    }
    single_set(
        "Keyboard and mouse",
        "For games without controller support: WASD on the left stick, the mouse on the right.",
        c,
    )
}

/// How Spatiand's desktop has always behaved, as a layout.
///
/// The trackpads are not in it, because they are not in any layout: both pads aim the laser
/// pointer in every application, and the triggers click it here. The D-pad and face buttons
/// type the keys a list or a dialog wants.
pub fn desktop() -> Layout {
    let mut c = Controls::default();
    for (button, code) in [
        (Button::DpadUp, 103),
        (Button::DpadDown, 108),
        (Button::DpadLeft, 105),
        (Button::DpadRight, 106),
        (Button::A, 28),
        (Button::B, 1),
        (Button::X, 14),
        (Button::Y, 15),
    ] {
        c.buttons.insert(button, Binding::key(code));
    }
    c.groups
        .insert(Group::LeftTrigger, GroupConfig::new(Mode::PointerClick));
    c.groups
        .insert(Group::RightTrigger, GroupConfig::new(Mode::PointerClick));
    c.groups.insert(
        Group::LeftStick,
        GroupConfig::new(Mode::Dpad {
            up: Binding::key(103),
            down: Binding::key(108),
            left: Binding::key(105),
            right: Binding::key(106),
            deadzone: 0.5,
            eight_way: false,
        }),
    );
    c.groups.insert(
        Group::RightStick,
        GroupConfig::new(Mode::ScrollWheel {
            degrees_per_notch: 30.0,
            clockwise: Binding::press(Action::Wheel {
                direction: Direction::Down,
            }),
            counter_clockwise: Binding::press(Action::Wheel {
                direction: Direction::Up,
            }),
        }),
    );
    single_set(
        "Spatial desktop",
        "Spatiand's own controls: the triggers click the pointer and the buttons type.",
        c,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_survives_a_trip_through_a_file() {
        for layout in all() {
            let text = toml::to_string_pretty(&layout)
                .unwrap_or_else(|e| panic!("{} does not serialise: {e}", layout.name));
            let back: Layout = toml::from_str(&text)
                .unwrap_or_else(|e| panic!("{} does not parse back: {e}\n{text}", layout.name));
            assert_eq!(back, layout, "{} changed on the way through", layout.name);
        }
    }

    #[test]
    fn template_names_are_distinct() {
        let names: std::collections::HashSet<_> = all().into_iter().map(|l| l.name).collect();
        assert_eq!(names.len(), all().len());
    }
}
