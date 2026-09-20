//! Remote computers: which ones this headset knows, whether they are there, and adding one.
//!
//! Three pages in one place, because they are one errand:
//!
//! * **The list.** Every paired computer, with whether it can be reached *now*, and a last
//!   row for adding another. A computer that is off says so rather than vanishing: the
//!   question "why is it not in the launcher?" deserves an answer, and it is usually "it is
//!   asleep".
//! * **The address.** Typed on the on-screen keyboard. The one piece of text anything in the
//!   shell asks for, and the only thing this headset cannot find out for itself.
//! * **The comparison.** Both ends show the same six digits, the way Bluetooth pairs a
//!   keyboard. The wearer checks them against the computer's screen and says yes on both.
//!   Nothing is typed from one to the other, so nothing is there to be overheard.
//!
//! Like everything in the shell, this decides what should happen and never does it. Whether a
//! computer is online, what its code is and whether it said yes all arrive from the compositor,
//! which is the part with a network.

use crate::grid::Direction;

/// One paired computer, as the list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRow {
    /// What it calls itself, or the address until it has said.
    pub label: String,
    /// What it was added as. The key everything else uses.
    pub address: String,
    pub status: HostStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostStatus {
    Online,
    Connecting,
    Offline,
    /// It answered and would not have this headset — forgotten on its side, usually.
    Refused,
}

impl HostStatus {
    pub fn label(self) -> &'static str {
        match self {
            HostStatus::Online => "online",
            HostStatus::Connecting => "connecting",
            HostStatus::Offline => "offline",
            HostStatus::Refused => "not paired",
        }
    }
}

/// How a pairing is going, as the compositor last said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PairingView {
    pub address: String,
    /// The six digits, once both certificates are known.
    pub code: Option<String>,
    /// One line about where things are.
    pub status: String,
    /// The computer has said yes.
    pub host_accepted: bool,
    /// It is over, one way or the other; A or B goes back to the list.
    pub finished: bool,
    /// Over, and it worked.
    pub succeeded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    List,
    Address,
    Pairing,
}

/// What pressing A asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostsAction {
    /// Nothing for the compositor; the page changed.
    None,
    /// Start pairing with this address.
    Pair(String),
    /// The wearer has compared the codes and they match.
    Confirm,
    /// Stop pairing.
    Cancel,
    /// Stop knowing this computer.
    Forget(String),
}

/// The remote computers page.
#[derive(Debug, Clone)]
pub struct Hosts {
    rows: Vec<HostRow>,
    cursor: usize,
    page: Page,
    typed: String,
    pairing: PairingView,
    /// The row pressed once for forgetting. Forgetting is the one destructive thing here, and
    /// it asks twice: the second press on the same row does it, and moving away cancels.
    armed: Option<usize>,
    /// Pressed "they match" and waiting for the computer to agree.
    confirmed: bool,
}

impl Default for Hosts {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            cursor: 0,
            page: Page::List,
            typed: String::new(),
            pairing: PairingView::default(),
            armed: None,
            confirmed: false,
        }
    }
}

/// The label of the last row of the list.
pub const ADD_LABEL: &str = "Add a computer";

impl Hosts {
    pub fn page(&self) -> Page {
        self.page
    }

    pub fn rows(&self) -> &[HostRow] {
        &self.rows
    }

    /// Rows the list shows: the computers and then the adding row.
    pub fn len(&self) -> usize {
        self.rows.len() + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn cursor(&self) -> usize {
        self.cursor.min(self.len() - 1)
    }

    pub fn typed(&self) -> &str {
        &self.typed
    }

    pub fn pairing(&self) -> &PairingView {
        &self.pairing
    }

    pub fn confirmed(&self) -> bool {
        self.confirmed
    }

    /// Whether the row at `index` has been pressed once, for forgetting.
    pub fn is_armed(&self, index: usize) -> bool {
        self.armed == Some(index)
    }

    /// Replace the list. Keeps the cursor on the same computer where it can.
    pub fn set_rows(&mut self, rows: Vec<HostRow>) {
        let focused = self.rows.get(self.cursor()).map(|r| r.address.clone());
        self.rows = rows;
        if let Some(address) = focused {
            if let Some(i) = self.rows.iter().position(|r| r.address == address) {
                self.cursor = i;
            }
        }
        self.cursor = self.cursor.min(self.len() - 1);
        if self.armed.is_some_and(|i| i >= self.rows.len()) {
            self.armed = None;
        }
    }

    pub fn set_pairing(&mut self, view: PairingView) {
        self.pairing = view;
    }

    /// Start at the list, as the HUD row opens it.
    pub fn open(&mut self) {
        self.page = Page::List;
        self.armed = None;
    }

    pub fn step(&mut self, direction: Direction) -> bool {
        if self.page != Page::List {
            return false;
        }
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
    pub fn activate(&mut self) -> HostsAction {
        match self.page {
            Page::List => {
                let cursor = self.cursor();
                if cursor == self.rows.len() {
                    self.page = Page::Address;
                    self.typed.clear();
                    return HostsAction::None;
                }
                if self.armed == Some(cursor) {
                    self.armed = None;
                    return HostsAction::Forget(self.rows[cursor].address.clone());
                }
                self.armed = Some(cursor);
                HostsAction::None
            }
            Page::Address => self.submit(),
            Page::Pairing => {
                if self.pairing.finished {
                    self.page = Page::List;
                    return HostsAction::None;
                }
                if self.pairing.code.is_some() && !self.confirmed {
                    self.confirmed = true;
                    return HostsAction::Confirm;
                }
                HostsAction::None
            }
        }
    }

    /// B. `false` means there is nowhere further back inside this page: close it.
    pub fn back(&mut self) -> (bool, HostsAction) {
        match self.page {
            Page::List => (false, HostsAction::None),
            Page::Address => {
                self.page = Page::List;
                (true, HostsAction::None)
            }
            Page::Pairing => {
                self.page = Page::List;
                let action = if self.pairing.finished {
                    HostsAction::None
                } else {
                    HostsAction::Cancel
                };
                (true, action)
            }
        }
    }

    /// Whether typing should come here rather than go to a window.
    pub fn wants_text(&self) -> bool {
        self.page == Page::Address
    }

    /// Some text from the keyboard.
    ///
    /// Only what can be in an address: a name, an IPv4 or IPv6 address, a port. Anything else
    /// is dropped here rather than failing later with a message about DNS.
    pub fn type_text(&mut self, text: &str) {
        if self.page != Page::Address {
            return;
        }
        for c in text.chars() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']' | '_') {
                if self.typed.len() < 253 {
                    self.typed.push(c.to_ascii_lowercase());
                }
            }
        }
    }

    pub fn backspace(&mut self) {
        if self.page == Page::Address {
            self.typed.pop();
        }
    }

    /// Enter, or A on the address page.
    pub fn submit(&mut self) -> HostsAction {
        if self.page != Page::Address {
            return HostsAction::None;
        }
        let address = self.typed.trim().to_string();
        if address.is_empty() {
            return HostsAction::None;
        }
        self.page = Page::Pairing;
        self.confirmed = false;
        self.pairing = PairingView {
            address: address.clone(),
            status: format!("Looking for {address}…"),
            ..PairingView::default()
        };
        HostsAction::Pair(address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: &str, status: HostStatus) -> HostRow {
        HostRow {
            label: address.to_string(),
            address: address.to_string(),
            status,
        }
    }

    #[test]
    fn the_list_always_ends_with_a_way_to_add_one() {
        let mut hosts = Hosts::default();
        assert_eq!(hosts.len(), 1, "even with nothing paired there is the adding row");
        hosts.set_rows(vec![row("workshop", HostStatus::Online)]);
        assert_eq!(hosts.len(), 2);
        hosts.step(Direction::Down);
        assert_eq!(hosts.activate(), HostsAction::None);
        assert_eq!(hosts.page(), Page::Address);
    }

    #[test]
    fn an_address_is_typed_cleaned_and_submitted() {
        let mut hosts = Hosts::default();
        hosts.activate();
        assert!(hosts.wants_text());
        hosts.type_text("WorkShop ");
        hosts.type_text("/;");
        assert_eq!(hosts.typed(), "workshop", "lower-cased, spaces and junk dropped");
        hosts.backspace();
        hosts.type_text("p:47600");
        assert_eq!(
            hosts.activate(),
            HostsAction::Pair("workshop:47600".into())
        );
        assert_eq!(hosts.page(), Page::Pairing);
        assert!(!hosts.wants_text(), "typing goes back to windows once it is sent");
    }

    #[test]
    fn nothing_typed_is_not_submitted() {
        let mut hosts = Hosts::default();
        hosts.activate();
        assert_eq!(hosts.activate(), HostsAction::None);
        assert_eq!(hosts.page(), Page::Address);
    }

    #[test]
    fn a_code_can_be_confirmed_once_and_only_once_it_exists() {
        let mut hosts = Hosts::default();
        hosts.activate();
        hosts.type_text("workshop");
        hosts.activate();
        assert_eq!(hosts.activate(), HostsAction::None, "no code yet, nothing to agree to");
        hosts.set_pairing(PairingView {
            address: "workshop".into(),
            code: Some("482 913".into()),
            ..PairingView::default()
        });
        assert_eq!(hosts.activate(), HostsAction::Confirm);
        assert!(hosts.confirmed());
        assert_eq!(hosts.activate(), HostsAction::None, "a second press is not a second yes");
    }

    #[test]
    fn backing_out_of_a_pairing_cancels_it_but_not_one_that_is_over() {
        let mut hosts = Hosts::default();
        hosts.activate();
        hosts.type_text("x");
        hosts.activate();
        assert_eq!(hosts.back(), (true, HostsAction::Cancel));
        assert_eq!(hosts.page(), Page::List);

        hosts.step(Direction::Down);
        hosts.activate();
        hosts.type_text("x");
        hosts.activate();
        hosts.set_pairing(PairingView {
            finished: true,
            succeeded: true,
            ..PairingView::default()
        });
        assert_eq!(hosts.back(), (true, HostsAction::None));
    }

    #[test]
    fn forgetting_takes_two_presses_on_the_same_row() {
        let mut hosts = Hosts::default();
        hosts.set_rows(vec![
            row("a", HostStatus::Online),
            row("b", HostStatus::Offline),
        ]);
        assert_eq!(hosts.activate(), HostsAction::None);
        assert!(hosts.is_armed(0));
        // Moving away disarms: a second press somewhere else is not a confirmation.
        hosts.step(Direction::Down);
        assert!(!hosts.is_armed(0));
        assert_eq!(hosts.activate(), HostsAction::None);
        assert_eq!(hosts.activate(), HostsAction::Forget("b".into()));
    }

    #[test]
    fn a_refreshed_list_keeps_the_cursor_on_the_same_computer() {
        let mut hosts = Hosts::default();
        hosts.set_rows(vec![row("a", HostStatus::Online), row("b", HostStatus::Online)]);
        hosts.step(Direction::Down);
        hosts.set_rows(vec![
            row("new", HostStatus::Online),
            row("a", HostStatus::Online),
            row("b", HostStatus::Offline),
        ]);
        assert_eq!(hosts.rows()[hosts.cursor()].address, "b");
    }
}
