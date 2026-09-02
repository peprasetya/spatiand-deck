//! Grouping applications the way a Start menu does.
//!
//! Showing every installed application at once was the first attempt, and forty glass bubbles
//! spread over four pages is not a menu — it is a directory listing you have to read. Grouping
//! turns "find Chrome among forty" into "Internet, then Chrome", which is two decisions of
//! about five options each.
//!
//! The groups come from the freedesktop `Categories` key, which every packaged application
//! sets and which is therefore the same vocabulary KDE's own menu uses. Entries list several
//! categories — a browser is `Network;WebBrowser;` — so the job here is picking **one** to
//! file it under, and doing that predictably.

/// A named group of applications, in the order it should be offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group {
    /// What the wearer sees.
    pub label: &'static str,
    /// The freedesktop category that puts an entry here.
    pub key: &'static str,
    /// Icon name to look up in the theme for the group's own bubble.
    pub icon: &'static str,
}

/// The groups, most-used first.
///
/// Ordered by how often someone reaches for them rather than alphabetically: the first row of
/// the launcher is the one you can select without moving the cursor, so it should hold the
/// things people actually open. Alphabetical would put Development first and Internet fifth.
pub const GROUPS: &[Group] = &[
    // Games first, and not only because this is a games console. The order is also the
    // tie-break rule, and Steam's own entry claims `Network;FileTransfer;Game` -- so putting
    // Internet first would file Steam under Internet, which is technically defensible and not
    // what anybody looking for it would expect.
    Group {
        label: "Games",
        key: "Game",
        icon: "applications-games",
    },
    Group {
        label: "Internet",
        key: "Network",
        icon: "applications-internet",
    },
    Group {
        label: "Media",
        key: "AudioVideo",
        icon: "applications-multimedia",
    },
    Group {
        label: "Graphics",
        key: "Graphics",
        icon: "applications-graphics",
    },
    Group {
        label: "Office",
        key: "Office",
        icon: "applications-office",
    },
    Group {
        label: "Development",
        key: "Development",
        icon: "applications-development",
    },
    Group {
        label: "System",
        key: "System",
        icon: "applications-system",
    },
    Group {
        label: "Settings",
        key: "Settings",
        icon: "preferences-system",
    },
    Group {
        label: "Utilities",
        key: "Utility",
        icon: "applications-utilities",
    },
];

/// Where anything unclassified goes.
pub const OTHER: Group = Group {
    label: "Other",
    key: "",
    icon: "applications-other",
};

/// Which group an application belongs in.
///
/// An entry usually lists several categories, so this walks [`GROUPS`] in order and takes the
/// first that matches — which means the ordering above is also the tie-break rule. A game that
/// also claims `Utility` files under Games, which is what anyone would expect.
pub fn group_for(categories: &[String]) -> Group {
    for group in GROUPS {
        if categories.iter().any(|c| c == group.key) {
            return *group;
        }
    }
    OTHER
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cats(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_browser_files_under_internet() {
        assert_eq!(
            group_for(&cats(&["Network", "WebBrowser"])).label,
            "Internet"
        );
    }

    #[test]
    fn an_unclassified_entry_goes_to_other_rather_than_vanishing() {
        // Dropping it would make an installed application unreachable from the launcher, with
        // no indication that it exists.
        assert_eq!(group_for(&[]).label, "Other");
        assert_eq!(group_for(&cats(&["X-Something-Vendor"])).label, "Other");
    }

    #[test]
    fn the_order_of_groups_is_the_tie_break() {
        // Steam claims both Game and Network, in that file's order Network first. Games comes
        // first in GROUPS, so that is where it lands regardless of the order in the file --
        // and the rule is the same anywhere else two categories collide.
        assert_eq!(group_for(&cats(&["Network", "Game"])).label, "Games");
        assert_eq!(group_for(&cats(&["Game", "Network"])).label, "Games");
    }

    #[test]
    fn every_group_has_a_distinct_key_and_label() {
        // A duplicate key would make one group permanently empty and its applications file
        // under whichever came first, which is very hard to spot.
        for (i, a) in GROUPS.iter().enumerate() {
            for b in &GROUPS[i + 1..] {
                assert_ne!(a.key, b.key, "duplicate key {}", a.key);
                assert_ne!(a.label, b.label, "duplicate label {}", a.label);
            }
            assert_ne!(a.key, OTHER.key, "a group cannot share Other's empty key");
        }
    }

    #[test]
    fn internet_comes_before_development() {
        // The first row is what you can select without moving, so it should hold what people
        // actually open. Alphabetical ordering would invert this.
        let position = |label: &str| GROUPS.iter().position(|g| g.label == label);
        assert!(position("Internet") < position("Development"));
        assert!(position("Games") < position("Settings"));
    }

    #[test]
    fn the_group_count_fits_a_single_page() {
        // Nine groups against a twelve-bubble page: the whole point is that the top level is
        // one glance with no paging.
        assert!(GROUPS.len() + 1 <= 12, "{} groups plus Other", GROUPS.len());
    }
}
