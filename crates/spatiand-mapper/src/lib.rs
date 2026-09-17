//! Controller layouts: what every physical control does, decided per application.
//!
//! This is Spatiand's answer to Steam Input, and it follows Steam Input's model closely enough
//! that anyone who has built a layout in Game Mode can build one here:
//!
//! * a **layout** holds one or more **action sets** — whole layouts an action can switch
//!   between — and **layers**, which override only some controls while held or toggled on;
//! * every button has a **binding**, a list of **activators** (press, long press, double press,
//!   start and release press, chord), each firing one or more **actions**, optionally as a
//!   toggle or on turbo;
//! * every analogue source — both sticks, both trackpads, both triggers, the Deck's gyro and
//!   the glasses' gyro — runs in a **mode** (joystick, directional pad, mouse, scroll wheel,
//!   flick stick, radial menu, trigger, gyro, or Spatiand's own 3D pointer), with a **mode
//!   shift** that swaps it for another mode while a button is held;
//! * actions drive **one** virtual gamepad, the keyboard and the mouse, or Spatiand itself.
//!
//! Nothing here touches hardware. Physical input arrives already merged into a [`Snapshot`] —
//! the Deck, any Bluetooth pad and the glasses read as one controller, which is what makes "one
//! device" true for the game — and output leaves as a [`Frame`] for the compositor to hand to
//! uinput and to its Wayland seat. Everything in between is plain data and a state machine, so
//! all of it is tested without a Deck.
//!
//! STEAM and `⋯` are not in [`Button`] on purpose. They open Spatiand's menus in every layout,
//! the way they open Steam's in Game Mode, so no layout can lock the wearer inside a game.

pub mod editor;
pub mod engine;
pub mod input;
pub mod keys;
pub mod layout;
pub mod output;
pub mod store;
pub mod templates;

pub use editor::Editor;
pub use engine::Engine;
pub use input::{Button, Buttons, Rates, Snapshot, Touch};
pub use layout::{
    ActionSet, Activator, Binding, Controls, Curve, Group, GroupConfig, GyroAxis, GyroEnable,
    GyroOutput, Layer, Layout, Mode, ModeKind, ModeShift, RadialItem, When,
};
pub use output::{
    Action, Command, Direction, Frame, MouseButton, PadButton, PadState, RadialView, Side,
};
pub use store::{AppKey, Store};
