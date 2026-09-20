//! Bluetooth: the devices this headset already knows, whether each is connected, and adding one.
//!
//! The row used to open the pairing wizard directly, which answers "add a device" and nothing
//! else. The question people actually arrive with is usually "is my keyboard connected?" —
//! after a battery change, after the Deck slept — and the wizard cannot say. So the page
//! opens on the list:
//!
//! * **Every paired device**, with whether it is connected and, where it reports one, its
//!   battery. A on a device connects it or disconnects it.
//! * **Forgetting** is Y, twice on the same row. A device that paired again under a new
//!   address leaves its old self behind here, and that stale entry is worth removing.
//! * **Add a device** opens the wizard, which is still the right tool for discovery and for
//!   typing a keyboard's passkey.
//! * **Turn Bluetooth on or off**, last, where it cannot be pressed on the way to a device.
//!
//! Like everything in the shell, this decides what should happen and never does it; the list
//! and whatever is under way arrive from the compositor, which is the part that talks to BlueZ.

use crate::grid::Direction;

/// One paired device, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    /// Its alias, or the address when it never said a name.
    pub label: String,
    /// The key everything else uses: `AA:BB:CC:DD:EE:FF`.
    pub address: String,
    /// BlueZ's icon name — `input-keyboard`, `audio-headset` — which says what kind of thing
    /// it is better than the name does.
    pub kind: String,
    pub connected: bool,
    /// Percent, for devices that report it.
    pub battery: Option<u8>,
}

impl DeviceRow {
    /// "keyboard", "headset" — the kind in a word, or nothing when BlueZ did not say.
    pub fn kind_word(&self) -> Option<&'static str> {
        Some(match self.kind.as_str() {
            "input-keyboard" => "keyboard",
            "input-mouse" => "mouse",
            "input-gaming" => "controller",
            "input-tablet" => "tablet",
            "audio-headset" | "audio-headphones" => "headphones",
            "audio-card" => "speaker",
            "phone" => "phone",
            "computer" => "computer",
            _ => return None,
        })
    }
}

/// What the compositor last found out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BluetoothView {
    /// `None` until the adapter has been asked, or when there is no adapter at all.
    pub powered: Option<bool>,
    pub devices: Vec<DeviceRow>,
    /// One line about something under way or just finished: "Connecting to BT5.0 Keyboard…",
    /// or why it failed. Empty when there is nothing to say.
    pub note: String,
    /// Addresses with a connect or disconnect under way, so a second press does not queue a
    /// second one.
    pub busy: Vec<String>,
}

/// What pressing a button asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BluetoothAction {
    None,
    Connect(String),
    Disconnect(String),
    Forget(String),
    /// Open the pairing wizard.
    Add,
    Power(bool),
}

/// The label of the row after the devices.
pub const ADD_LABEL: &str = "Add a device";

#[derive(Debug, Clone, Default)]
pub struct Bluetooth {
    view: BluetoothView,
    cursor: usize,
    /// The device row Y was pressed on once. Forgetting is the one thing here that cannot be
    /// undone without pairing again, so it asks twice; moving away cancels.
    armed: Option<usize>,
}

/// Which row the cursor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Row {
    Device(usize),
    Add,
    Power,
}

impl Bluetooth {
    pub fn view(&self) -> &BluetoothView {
        &self.view
    }

    pub fn devices(&self) -> &[DeviceRow] {
        &self.view.devices
    }

    /// Devices, then adding, then the power switch.
    pub fn len(&self) -> usize {
        self.view.devices.len() + 2
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn cursor(&self) -> usize {
        self.cursor.min(self.len() - 1)
    }

    pub fn row(&self, index: usize) -> Row {
        let devices = self.view.devices.len();
        if index < devices {
            Row::Device(index)
        } else if index == devices {
            Row::Add
        } else {
            Row::Power
        }
    }

    pub fn is_armed(&self, index: usize) -> bool {
        self.armed == Some(index)
    }

    pub fn is_busy(&self, address: &str) -> bool {
        self.view.busy.iter().any(|b| b == address)
    }

    /// Start at the top, as the HUD row opens it.
    pub fn open(&mut self) {
        self.cursor = 0;
        self.armed = None;
    }

    /// Replace what is known. Keeps the cursor on the same device where it can, because the
    /// list re-sorts as things connect and a cursor that jumps is a press on the wrong row.
    pub fn set_view(&mut self, view: BluetoothView) {
        let focused = match self.row(self.cursor()) {
            Row::Device(i) => Some(self.view.devices[i].address.clone()),
            _ => None,
        };
        // Nothing had been heard yet: the first list starts at its top device.
        let first = self.view.powered.is_none() && self.view.devices.is_empty();
        let from_end = self.len() - 1 - self.cursor();
        let armed = self
            .armed
            .and_then(|i| self.view.devices.get(i))
            .map(|d| d.address.clone());
        self.view = view;
        self.cursor = match focused {
            Some(address) => self
                .view
                .devices
                .iter()
                .position(|d| d.address == address)
                .unwrap_or(0),
            None if first => 0,
            // On "Add" or the switch: stay on it, counted from the end.
            None => (self.len() - 1).saturating_sub(from_end),
        };
        self.armed = armed.and_then(|a| self.view.devices.iter().position(|d| d.address == a));
    }

    pub fn step(&mut self, direction: Direction) -> bool {
        let before = self.cursor();
        match direction {
            Direction::Up => self.cursor = before.saturating_sub(1),
            Direction::Down => self.cursor = (before + 1).min(self.len() - 1),
            Direction::Left | Direction::Right => {}
        }
        if self.cursor != before {
            self.armed = None;
            true
        } else {
            false
        }
    }

    /// A.
    pub fn activate(&mut self) -> BluetoothAction {
        self.armed = None;
        match self.row(self.cursor()) {
            Row::Device(i) => {
                let device = &self.view.devices[i];
                if self.is_busy(&device.address) {
                    return BluetoothAction::None;
                }
                if device.connected {
                    BluetoothAction::Disconnect(device.address.clone())
                } else {
                    BluetoothAction::Connect(device.address.clone())
                }
            }
            Row::Add => BluetoothAction::Add,
            Row::Power => match self.view.powered {
                Some(on) => BluetoothAction::Power(!on),
                None => BluetoothAction::None,
            },
        }
    }

    /// Y: forget the device under the cursor, on the second press.
    pub fn forget(&mut self) -> BluetoothAction {
        let cursor = self.cursor();
        let Row::Device(i) = self.row(cursor) else {
            return BluetoothAction::None;
        };
        // Already being connected, disconnected or forgotten: the list has not caught up yet.
        if self.is_busy(&self.view.devices[i].address) {
            return BluetoothAction::None;
        }
        if self.armed == Some(cursor) {
            self.armed = None;
            return BluetoothAction::Forget(self.view.devices[i].address.clone());
        }
        self.armed = Some(cursor);
        BluetoothAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(address: &str, connected: bool) -> DeviceRow {
        DeviceRow {
            label: format!("dev {address}"),
            address: address.into(),
            kind: "input-keyboard".into(),
            connected,
            battery: None,
        }
    }

    fn view(devices: Vec<DeviceRow>) -> BluetoothView {
        BluetoothView {
            powered: Some(true),
            devices,
            ..BluetoothView::default()
        }
    }

    #[test]
    fn a_connects_a_disconnected_device_and_disconnects_a_connected_one() {
        let mut bt = Bluetooth::default();
        bt.set_view(view(vec![device("a", false), device("b", true)]));
        assert_eq!(bt.activate(), BluetoothAction::Connect("a".into()));
        bt.step(Direction::Down);
        assert_eq!(bt.activate(), BluetoothAction::Disconnect("b".into()));
    }

    #[test]
    fn a_press_while_one_is_under_way_does_nothing() {
        let mut bt = Bluetooth::default();
        let mut v = view(vec![device("a", false)]);
        v.busy.push("a".into());
        bt.set_view(v);
        assert_eq!(bt.activate(), BluetoothAction::None);
    }

    #[test]
    fn the_list_ends_with_adding_and_then_the_switch() {
        let mut bt = Bluetooth::default();
        bt.set_view(view(vec![device("a", false)]));
        bt.step(Direction::Down);
        assert_eq!(bt.activate(), BluetoothAction::Add);
        bt.step(Direction::Down);
        assert_eq!(bt.activate(), BluetoothAction::Power(false));
        let mut off = view(Vec::new());
        off.powered = Some(false);
        bt.set_view(off);
        assert_eq!(bt.row(bt.cursor()), Row::Power, "stays on the switch as the list empties");
        assert_eq!(bt.activate(), BluetoothAction::Power(true));
    }

    #[test]
    fn forgetting_takes_two_presses_of_y_on_the_same_device() {
        let mut bt = Bluetooth::default();
        bt.set_view(view(vec![device("a", false), device("b", false)]));
        assert_eq!(bt.forget(), BluetoothAction::None);
        assert!(bt.is_armed(0));
        bt.step(Direction::Down);
        assert!(!bt.is_armed(0), "moving away disarms");
        assert_eq!(bt.forget(), BluetoothAction::None);
        assert_eq!(bt.forget(), BluetoothAction::Forget("b".into()));
        bt.step(Direction::Down);
        assert_eq!(bt.forget(), BluetoothAction::None, "Add is not a device");
        assert_eq!(bt.forget(), BluetoothAction::None);
    }

    #[test]
    fn a_refreshed_list_keeps_the_cursor_on_the_same_device() {
        let mut bt = Bluetooth::default();
        bt.set_view(view(vec![device("a", false), device("b", false)]));
        bt.step(Direction::Down);
        bt.set_view(view(vec![device("b", true), device("a", false)]));
        assert_eq!(bt.activate(), BluetoothAction::Disconnect("b".into()));
    }
}
