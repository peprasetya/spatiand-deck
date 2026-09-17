//! The layout editor, as a state machine the compositor draws.
//!
//! Laid out the way Steam's controller configurator is: a page for the whole layout listing
//! every control with what it does now, a page per control, a mode picker for the analogue
//! sources, activators under each button, and an action picker grouped by what the action
//! drives. Driven by the D-pad and A/B like every other menu in Spatiand, with left and right
//! adjusting whatever the selected row holds, and by the pointer through [`Editor::click`].
//!
//! Edits apply at once — the compositor hands [`Editor::layout`] to the running engine after
//! every [`Event::Changed`] — so a sensitivity can be tried while it is being set, as in Steam.

use crate::input::Button;
use crate::keys;
use crate::layout::{
    Activator, Binding, Controls, Curve, Group, GroupConfig, GyroAxis, GyroEnable, GyroOutput,
    Layer, Layout, Mode, ModeKind, ModeShift, RadialItem, When,
};
use crate::output::{Action, Command, Direction, MouseButton, PadButton, Side};
use crate::store::AppKey;
use crate::templates;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    Left,
    Right,
    Accept,
    Back,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    None,
    /// The layout changed; hand it to the engine.
    Changed,
    /// The wearer asked to go back to this application's default layout.
    Reset,
    /// Backed out of the top page.
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub value: Option<String>,
}

/// Where a label goes on the controller picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Callout {
    Button(Button),
    Group(Group),
}

/// How many rows the card shows at once.
///
/// The editor does not draw anything, so this is the one number it borrows from whatever does:
/// it is used only to decide whether a page is long enough to need a position in its footer.
const VISIBLE_ROWS: usize = 6;

#[derive(Debug, Clone, PartialEq)]
pub struct View {
    pub title: String,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub detail: String,
    pub footer: String,
    /// What to write beside each control on the controller picture. Empty on pages that are
    /// not about the whole controller.
    pub callouts: Vec<(Callout, String)>,
}

/// A binding the editor can reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Button(Button),
    Sub {
        group: Group,
        shifted: bool,
        sub: Sub,
    },
    Radial {
        group: Group,
        shifted: bool,
        item: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    Up,
    Down,
    Left,
    Right,
    Clockwise,
    CounterClockwise,
    SoftPull,
    FullPull,
}

impl Sub {
    fn label(self) -> &'static str {
        match self {
            Sub::Up => "Up",
            Sub::Down => "Down",
            Sub::Left => "Left",
            Sub::Right => "Right",
            Sub::Clockwise => "Clockwise",
            Sub::CounterClockwise => "Anticlockwise",
            Sub::SoftPull => "Soft pull",
            Sub::FullPull => "Full pull",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Gamepad,
    Stick,
    Trigger,
    Keyboard,
    Mouse,
    Sets,
    Spatiand,
}

impl Category {
    const ALL: [Category; 7] = [
        Category::Gamepad,
        Category::Stick,
        Category::Trigger,
        Category::Keyboard,
        Category::Mouse,
        Category::Sets,
        Category::Spatiand,
    ];

    fn label(self) -> &'static str {
        match self {
            Category::Gamepad => "Gamepad button",
            Category::Stick => "Gamepad stick direction",
            Category::Trigger => "Gamepad trigger",
            Category::Keyboard => "Keyboard key",
            Category::Mouse => "Mouse button or wheel",
            Category::Sets => "Action set or layer",
            Category::Spatiand => "Spatiand",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Root,
    Buttons,
    Group { group: Group, shifted: bool },
    Modes { group: Group, shifted: bool },
    Binding(Target),
    Activator(Target, usize),
    Categories(Target, usize, Option<usize>),
    Choices(Target, usize, Option<usize>, Category),
    Templates,
    Sets,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Float {
    Sensitivity,
    Deadzone,
    Outer,
    Notch,
    TurnPixels,
    SoftThreshold,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Flag {
    InvertX,
    InvertY,
    EightWay,
    OnRelease,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Field {
    Set,
    Layer,
    Mode { group: Group, shifted: bool },
    ShiftButton(Group),
    Float { group: Group, shifted: bool, which: Float },
    Flag { group: Group, shifted: bool, which: Flag },
    Curve { group: Group, shifted: bool },
    StickOutput { group: Group, shifted: bool },
    TriggerOutput { group: Group, shifted: bool },
    GyroOutput { group: Group, shifted: bool },
    GyroEnable { group: Group, shifted: bool },
    GyroButton { group: Group, shifted: bool },
    GyroAxis { group: Group, shifted: bool },
    When(Target, usize),
    WhenTime(Target, usize),
    Chord(Target, usize),
    Toggle(Target, usize),
    Turbo(Target, usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Do {
    Revert,
    Reset,
    AddActivator(Target),
    RemoveActivator(Target, usize),
    SetAction(Target, usize, Option<usize>, Action),
    RemoveAction(Target, usize, usize),
    SetMode(Group, bool, ModeKind),
    Template(usize),
    SelectSet(usize),
    AddSet,
    RemoveSet,
    SelectLayer(Option<usize>),
    AddLayer,
    RemoveLayer,
    AddRadialItem(Group, bool),
    RemoveRadialItem(Group, bool, usize),
}

#[derive(Debug, Clone, PartialEq)]
enum Op {
    Open(Page),
    /// Left and right step the field; A opens the page if there is one, or steps forward.
    Cycle(Field, Option<Page>),
    /// Left and right step the field; A does nothing.
    Slider(Field),
    Do(Do),
}

struct Built {
    title: String,
    rows: Vec<(Row, Op, String)>,
    detail: String,
}

pub struct Editor {
    app: AppKey,
    app_name: String,
    layout: Layout,
    original: Layout,
    set: usize,
    layer: Option<usize>,
    stack: Vec<(Page, usize)>,
}

const TURBO_STEPS: [Option<u32>; 6] = [None, Some(50), Some(100), Some(150), Some(250), Some(500)];

impl Editor {
    pub fn new(app: AppKey, app_name: impl Into<String>, layout: Layout) -> Self {
        let layout = layout.repaired();
        Self {
            app,
            app_name: app_name.into(),
            original: layout.clone(),
            layout,
            set: 0,
            layer: None,
            stack: vec![(Page::Root, 0)],
        }
    }

    pub fn app(&self) -> &AppKey {
        &self.app
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Whether anything differs from the layout the editor opened with.
    pub fn is_dirty(&self) -> bool {
        self.layout != self.original
    }

    /// Replace the layout wholesale, as after [`Event::Reset`].
    pub fn replace(&mut self, layout: Layout) {
        self.layout = layout.repaired();
        self.set = 0;
        self.layer = None;
        self.stack.truncate(1);
    }

    pub fn view(&self) -> View {
        let (page, cursor) = *self.stack.last().expect("the root is never popped");
        let built = self.build(page);
        let cursor = cursor.min(built.rows.len().saturating_sub(1));
        let focused = built.rows.get(cursor);
        let detail = match focused {
            Some((_, _, d)) if !d.is_empty() => d.clone(),
            _ => built.detail.clone(),
        };
        let keys = match focused.map(|(_, op, _)| op) {
            Some(Op::Cycle(..)) | Some(Op::Slider(..)) => "Left / right change    A select    B back",
            _ => "A select    B back",
        };
        // Where you are in the list, on any page long enough to have somewhere to be. The card
        // shows six rows at a time and the Deck has twenty-four buttons, so without this the
        // four back buttons sit below the fold on a page that looks complete -- which reads as
        // "the back buttons cannot be mapped" rather than as "scroll down".
        let footer = if built.rows.len() > VISIBLE_ROWS {
            format!("{} of {}    {keys}", cursor + 1, built.rows.len())
        } else {
            keys.to_string()
        };
        View {
            title: built.title,
            rows: built.rows.into_iter().map(|(row, _, _)| row).collect(),
            cursor,
            detail,
            footer,
            callouts: match page {
                Page::Root | Page::Buttons => self.callouts(),
                _ => Vec::new(),
            },
        }
    }

    /// The pointer clicked a row: select it and press A.
    pub fn click(&mut self, row: usize) -> Event {
        if let Some(top) = self.stack.last_mut() {
            top.1 = row;
        }
        self.handle(Input::Accept)
    }

    pub fn handle(&mut self, input: Input) -> Event {
        let (page, cursor) = *self.stack.last().expect("the root is never popped");
        let built = self.build(page);
        let count = built.rows.len();
        let cursor = cursor.min(count.saturating_sub(1));
        let op = built.rows.get(cursor).map(|(_, op, _)| op.clone());
        match input {
            Input::Up | Input::Down => {
                if count > 0 {
                    let next = if input == Input::Up {
                        (cursor + count - 1) % count
                    } else {
                        (cursor + 1) % count
                    };
                    self.stack.last_mut().unwrap().1 = next;
                }
                Event::None
            }
            Input::Left | Input::Right => {
                let step = if input == Input::Left { -1 } else { 1 };
                match op {
                    Some(Op::Cycle(field, _)) | Some(Op::Slider(field)) => self.adjust(field, step),
                    _ => Event::None,
                }
            }
            Input::Accept => match op {
                Some(Op::Open(page)) => {
                    self.stack.last_mut().unwrap().1 = cursor;
                    self.push(page);
                    Event::None
                }
                Some(Op::Cycle(_, Some(page))) => {
                    self.push(page);
                    Event::None
                }
                Some(Op::Cycle(field, None)) => self.adjust(field, 1),
                Some(Op::Do(action)) => self.run(action),
                Some(Op::Slider(_)) | None => Event::None,
            },
            Input::Back => {
                if self.stack.len() > 1 {
                    self.stack.pop();
                    Event::None
                } else {
                    Event::Close
                }
            }
        }
    }

    fn push(&mut self, page: Page) {
        // A radial item has one activator and nothing to choose between, so its binding page
        // would be a list of one: go straight to the activator.
        let page = match page {
            Page::Binding(t @ Target::Radial { .. }) => Page::Activator(t, 0),
            other => other,
        };
        self.stack.push((page, 0));
    }

    fn pop(&mut self, levels: usize) {
        for _ in 0..levels {
            if self.stack.len() > 1 {
                self.stack.pop();
            }
        }
    }

    // --- what is being edited ---

    fn controls(&self) -> &Controls {
        match self.layer.and_then(|l| self.layout.layers.get(l)) {
            Some(layer) => &layer.controls,
            None => &self.layout.sets[self.set].controls,
        }
    }

    fn controls_mut(&mut self) -> &mut Controls {
        match self.layer {
            Some(l) if l < self.layout.layers.len() => &mut self.layout.layers[l].controls,
            _ => &mut self.layout.sets[self.set].controls,
        }
    }

    fn base(&self) -> &Controls {
        &self.layout.sets[self.set].controls
    }

    /// In a layer, what applies is the layer's own entry or else the set's.
    fn group_config(&self, group: Group) -> Option<&GroupConfig> {
        self.controls().groups.get(&group).or_else(|| {
            if self.layer.is_some() {
                self.base().groups.get(&group)
            } else {
                None
            }
        })
    }

    fn mode(&self, group: Group, shifted: bool) -> Mode {
        let Some(config) = self.group_config(group) else {
            return Mode::None;
        };
        if shifted {
            config
                .shift
                .as_ref()
                .map(|s| s.mode.clone())
                .unwrap_or(Mode::None)
        } else {
            config.mode.clone()
        }
    }

    /// The mode, ready to change. Editing a layer copies the set's entry into it first, so the
    /// change lands in the layer and the set underneath is left as it was.
    fn mode_mut(&mut self, group: Group, shifted: bool) -> &mut Mode {
        if self.layer.is_some() && !self.controls().groups.contains_key(&group) {
            if let Some(inherited) = self.base().groups.get(&group).cloned() {
                self.controls_mut().groups.insert(group, inherited);
            }
        }
        let config = self
            .controls_mut()
            .groups
            .entry(group)
            .or_insert_with(|| GroupConfig::new(Mode::None));
        if shifted {
            let base = config.mode.clone();
            &mut config
                .shift
                .get_or_insert(ModeShift {
                    button: Button::L1,
                    mode: base,
                })
                .mode
        } else {
            &mut config.mode
        }
    }

    fn binding(&self, target: Target) -> Binding {
        match target {
            Target::Button(button) => self
                .controls()
                .buttons
                .get(&button)
                .or_else(|| {
                    if self.layer.is_some() {
                        self.base().buttons.get(&button)
                    } else {
                        None
                    }
                })
                .cloned()
                .unwrap_or_default(),
            Target::Sub {
                group,
                shifted,
                sub,
            } => sub_binding(&self.mode(group, shifted), sub)
                .cloned()
                .unwrap_or_default(),
            Target::Radial {
                group,
                shifted,
                item,
            } => match self.mode(group, shifted) {
                Mode::RadialMenu { items, .. } => items
                    .get(item)
                    .map(|i| Binding {
                        activators: vec![Activator {
                            actions: i.actions.clone(),
                            ..Default::default()
                        }],
                    })
                    .unwrap_or_default(),
                _ => Binding::default(),
            },
        }
    }

    fn with_binding(&mut self, target: Target, change: impl FnOnce(&mut Binding)) {
        match target {
            Target::Button(button) => {
                if self.layer.is_some() && !self.controls().buttons.contains_key(&button) {
                    if let Some(inherited) = self.base().buttons.get(&button).cloned() {
                        self.controls_mut().buttons.insert(button, inherited);
                    }
                }
                let in_set = self.layer.is_none();
                let controls = self.controls_mut();
                let binding = controls.buttons.entry(button).or_default();
                change(binding);
                // An empty binding in a set is the same as none; in a layer it means "unbound
                // while the layer is on", which is worth keeping.
                if in_set && binding.activators.is_empty() {
                    controls.buttons.remove(&button);
                }
            }
            Target::Sub {
                group,
                shifted,
                sub,
            } => {
                if let Some(binding) = sub_binding_mut(self.mode_mut(group, shifted), sub) {
                    change(binding);
                }
            }
            Target::Radial {
                group,
                shifted,
                item,
            } => {
                if let Mode::RadialMenu { items, .. } = self.mode_mut(group, shifted) {
                    if let Some(entry) = items.get_mut(item) {
                        let mut binding = Binding {
                            activators: vec![Activator {
                                actions: entry.actions.clone(),
                                ..Default::default()
                            }],
                        };
                        change(&mut binding);
                        entry.actions = binding
                            .activators
                            .into_iter()
                            .next()
                            .map(|a| a.actions)
                            .unwrap_or_default();
                        entry.label = entry
                            .actions
                            .first()
                            .map(|a| a.label())
                            .unwrap_or_else(|| format!("Item {}", item + 1));
                    }
                }
            }
        }
    }

    fn set_name(&self, index: usize) -> String {
        self.layout
            .sets
            .get(index)
            .map(|s| s.name.clone())
            .unwrap_or_else(|| format!("Action set {}", index + 1))
    }

    fn layer_name(&self, index: usize) -> String {
        self.layout
            .layers
            .get(index)
            .map(|l| l.name.clone())
            .unwrap_or_else(|| format!("Layer {}", index + 1))
    }

    /// An action's label, with set and layer numbers replaced by their names.
    fn action_label(&self, action: &Action) -> String {
        match *action {
            Action::ActionSet { index } => format!("Switch to {}", self.set_name(index)),
            Action::HoldLayer { index } => format!("Hold {}", self.layer_name(index)),
            Action::ToggleLayer { index } => format!("Toggle {}", self.layer_name(index)),
            _ => action.label(),
        }
    }

    fn binding_summary(&self, binding: &Binding) -> String {
        let bound: Vec<&Activator> = binding
            .activators
            .iter()
            .filter(|a| !a.actions.is_empty())
            .collect();
        let Some(first) = bound.first() else {
            return "Unbound".into();
        };
        let actions = first
            .actions
            .iter()
            .map(|a| self.action_label(a))
            .collect::<Vec<_>>()
            .join(" + ");
        let mut text = match first.when {
            When::Press => actions,
            other => format!("{}: {actions}", other.label()),
        };
        if bound.len() > 1 {
            text.push_str(&format!(" (+{} more)", bound.len() - 1));
        }
        text
    }

    fn target_label(&self, target: Target) -> String {
        match target {
            Target::Button(b) => b.label().into(),
            Target::Sub {
                group,
                shifted,
                sub,
            } => format!(
                "{}{} {}",
                group.label(),
                if shifted { " (shifted)" } else { "" },
                sub.label().to_lowercase()
            ),
            Target::Radial { group, item, .. } => format!("{} item {}", group.label(), item + 1),
        }
    }

    fn callouts(&self) -> Vec<(Callout, String)> {
        let effective = self.layout.effective(self.set, self.layer);
        let mut out = Vec::new();
        for button in Button::ALL {
            if let Some(binding) = effective.buttons.get(&button) {
                let short = binding.short();
                if !short.is_empty() {
                    out.push((Callout::Button(button), short));
                }
            }
        }
        for group in Group::ALL {
            if let Some(config) = effective.groups.get(&group) {
                let kind = ModeKind::of(&config.mode);
                if kind != ModeKind::None {
                    out.push((Callout::Group(group), kind.label().into()));
                }
            }
        }
        out
    }

    // --- pages ---

    fn build(&self, page: Page) -> Built {
        match page {
            Page::Root => self.root_page(),
            Page::Buttons => self.buttons_page(),
            Page::Group { group, shifted } => self.group_page(group, shifted),
            Page::Modes { group, shifted } => Built {
                title: format!("{} mode", group.label()),
                rows: group
                    .modes()
                    .iter()
                    .map(|kind| {
                        let current = ModeKind::of(&self.mode(group, shifted)) == *kind;
                        (
                            Row {
                                label: kind.label().into(),
                                value: current.then(|| "Current".into()),
                            },
                            Op::Do(Do::SetMode(group, shifted, *kind)),
                            kind.detail().into(),
                        )
                    })
                    .collect(),
                detail: String::new(),
            },
            Page::Binding(target) => self.binding_page(target),
            Page::Activator(target, index) => self.activator_page(target, index),
            Page::Categories(target, index, slot) => {
                let mut rows: Vec<(Row, Op, String)> = Category::ALL
                    .iter()
                    .map(|c| {
                        (
                            Row {
                                label: c.label().into(),
                                value: None,
                            },
                            Op::Open(Page::Choices(target, index, slot, *c)),
                            String::new(),
                        )
                    })
                    .collect();
                if let Some(slot) = slot {
                    rows.push((
                        row("Remove this action", None),
                        Op::Do(Do::RemoveAction(target, index, slot)),
                        String::new(),
                    ));
                }
                Built {
                    title: "Choose an action".into(),
                    rows,
                    detail: "What should this do? A game with controller support wants gamepad \
                             actions; one without wants the keyboard and mouse."
                        .into(),
                }
            }
            Page::Choices(target, index, slot, category) => Built {
                title: category.label().into(),
                rows: self
                    .choices(category)
                    .into_iter()
                    .map(|action| {
                        (
                            Row {
                                label: self.action_label(&action),
                                value: None,
                            },
                            Op::Do(Do::SetAction(target, index, slot, action)),
                            String::new(),
                        )
                    })
                    .collect(),
                detail: String::new(),
            },
            Page::Templates => Built {
                title: "Templates".into(),
                rows: templates::all()
                    .into_iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let current = t.name == self.layout.name;
                        (
                            Row {
                                label: t.name.clone(),
                                value: current.then(|| "Current".into()),
                            },
                            Op::Do(Do::Template(i)),
                            t.description.clone(),
                        )
                    })
                    .collect(),
                detail: String::new(),
            },
            Page::Sets => self.sets_page(),
        }
    }

    fn root_page(&self) -> Built {
        let mut rows = vec![
            (
                row("Template", Some(self.layout.name.clone())),
                Op::Open(Page::Templates),
                "Start again from one of the built-in layouts.".to_string(),
            ),
            (
                row("Action set", Some(self.set_name(self.set))),
                Op::Cycle(Field::Set, Some(Page::Sets)),
                "Which action set you are editing. A switches, adds and removes sets and layers."
                    .to_string(),
            ),
            (
                row(
                    "Editing",
                    Some(match self.layer {
                        Some(l) => self.layer_name(l),
                        None => "The whole action set".into(),
                    }),
                ),
                Op::Cycle(Field::Layer, Some(Page::Sets)),
                "Edit the action set itself, or a layer that changes some controls while it is on."
                    .to_string(),
            ),
        ];
        let effective = self.layout.effective(self.set, self.layer);
        let bound = Button::ALL
            .iter()
            .filter(|b| effective.buttons.get(b).is_some_and(|x| !x.is_empty()))
            .count();
        rows.push((
            row("Buttons", Some(format!("{bound} of {} bound", Button::ALL.len()))),
            Op::Open(Page::Buttons),
            "Face buttons, D-pad, bumpers, back buttons, clicks and the glasses' buttons."
                .to_string(),
        ));
        for group in Group::ALL {
            let config = self.group_config(group);
            let mode = config.map(|c| ModeKind::of(&c.mode)).unwrap_or(ModeKind::None);
            let mut value = mode.label().to_string();
            if let Some(shift) = config.and_then(|c| c.shift.as_ref()) {
                value.push_str(&format!(", shifts with {}", shift.button.label()));
            }
            rows.push((
                row(group.label(), Some(value)),
                Op::Open(Page::Group {
                    group,
                    shifted: false,
                }),
                mode.detail().to_string(),
            ));
        }
        if self.is_dirty() {
            rows.push((
                row("Undo changes", None),
                Op::Do(Do::Revert),
                "Put back the layout as it was when you opened this.".to_string(),
            ));
        }
        rows.push((
            row("Use the default layout", None),
            Op::Do(Do::Reset),
            format!(
                "Forget this layout and use the default for {}.",
                if self.app.is_steam_game() {
                    "a game"
                } else {
                    "an application"
                }
            ),
        ));
        Built {
            title: format!("Controller: {}", self.app_name),
            rows,
            detail: String::new(),
        }
    }

    fn buttons_page(&self) -> Built {
        Built {
            title: "Buttons".into(),
            rows: Button::ALL
                .iter()
                .map(|b| {
                    (
                        row(b.label(), Some(self.binding_summary(&self.binding(Target::Button(*b))))),
                        Op::Open(Page::Binding(Target::Button(*b))),
                        String::new(),
                    )
                })
                .collect(),
            detail: "STEAM and the \u{22ef} button always open Spatiand's menus, in every layout."
                .into(),
        }
    }

    fn binding_page(&self, target: Target) -> Built {
        let binding = self.binding(target);
        let mut rows: Vec<(Row, Op, String)> = binding
            .activators
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let actions = if a.actions.is_empty() {
                    "Nothing".to_string()
                } else {
                    a.actions
                        .iter()
                        .map(|x| self.action_label(x))
                        .collect::<Vec<_>>()
                        .join(" + ")
                };
                (
                    row(a.when.label(), Some(actions)),
                    Op::Open(Page::Activator(target, i)),
                    String::new(),
                )
            })
            .collect();
        rows.push((
            row("Add an activator", None),
            Op::Do(Do::AddActivator(target)),
            "Another way to press this: a long press, a double press, a chord with another button."
                .into(),
        ));
        Built {
            title: self.target_label(target),
            rows,
            detail: "Each activator fires its actions at a different moment of a press.".into(),
        }
    }

    fn activator_page(&self, target: Target, index: usize) -> Built {
        let binding = self.binding(target);
        let activator = binding.activators.get(index).cloned().unwrap_or_default();
        let radial = matches!(target, Target::Radial { .. });
        let mut rows = Vec::new();
        if !radial {
            rows.push((
                row("Activation", Some(activator.when.label().into())),
                Op::Cycle(Field::When(target, index), None),
                "Regular fires while held. Long and double press wait to see which it was; start \
                 and release fire once; a chord needs another button held too."
                    .to_string(),
            ));
            match activator.when {
                When::LongPress { ms } | When::DoublePress { ms } => rows.push((
                    row("Time", Some(format!("{ms} ms"))),
                    Op::Slider(Field::WhenTime(target, index)),
                    String::new(),
                )),
                When::Chord { with } => rows.push((
                    row("Together with", Some(with.label().into())),
                    Op::Cycle(Field::Chord(target, index), None),
                    String::new(),
                )),
                _ => {}
            }
            rows.push((
                row("Toggle", Some(on_off(activator.toggle))),
                Op::Cycle(Field::Toggle(target, index), None),
                "On with one press and off with the next.".to_string(),
            ));
            rows.push((
                row(
                    "Turbo",
                    Some(match activator.turbo_ms {
                        Some(ms) => format!("Every {ms} ms"),
                        None => "Off".into(),
                    }),
                ),
                Op::Cycle(Field::Turbo(target, index), None),
                "Pulses the actions on and off for as long as it is held.".to_string(),
            ));
        }
        for (i, action) in activator.actions.iter().enumerate() {
            rows.push((
                row(&format!("Action {}", i + 1), Some(self.action_label(action))),
                Op::Open(Page::Categories(target, index, Some(i))),
                String::new(),
            ));
        }
        rows.push((
            row("Add an action", None),
            Op::Open(Page::Categories(target, index, None)),
            "Several actions fire together, as a key combination does.".to_string(),
        ));
        match target {
            Target::Radial {
                group,
                shifted,
                item,
            } => rows.push((
                row("Remove this item", None),
                Op::Do(Do::RemoveRadialItem(group, shifted, item)),
                String::new(),
            )),
            _ => rows.push((
                row("Remove this activator", None),
                Op::Do(Do::RemoveActivator(target, index)),
                String::new(),
            )),
        }
        Built {
            title: self.target_label(target),
            rows,
            detail: String::new(),
        }
    }

    fn group_page(&self, group: Group, shifted: bool) -> Built {
        let mode = self.mode(group, shifted);
        let kind = ModeKind::of(&mode);
        let f = |which| Field::Float {
            group,
            shifted,
            which,
        };
        let flag = |which| Field::Flag {
            group,
            shifted,
            which,
        };
        let sub = |s| Target::Sub {
            group,
            shifted,
            sub: s,
        };
        let mut rows = vec![(
            row("Mode", Some(kind.label().into())),
            Op::Cycle(Field::Mode { group, shifted }, Some(Page::Modes { group, shifted })),
            kind.detail().to_string(),
        )];
        match &mode {
            Mode::None | Mode::PointerClick => {}
            Mode::Joystick {
                output,
                sensitivity,
                deadzone,
                outer,
                curve,
                invert_x,
                invert_y,
            } => {
                rows.push((
                    row("Output", Some(format!("{} stick", output.label()))),
                    Op::Cycle(Field::StickOutput { group, shifted }, None),
                    String::new(),
                ));
                rows.push(slider("Sensitivity", format!("{sensitivity:.1}x"), f(Float::Sensitivity)));
                rows.push(slider("Dead zone", percent(*deadzone), f(Float::Deadzone)));
                rows.push(slider("Outer ring", percent(*outer), f(Float::Outer)));
                rows.push((
                    row("Response curve", Some(curve.label().into())),
                    Op::Cycle(Field::Curve { group, shifted }, None),
                    String::new(),
                ));
                rows.push(toggle("Invert horizontal", *invert_x, flag(Flag::InvertX)));
                rows.push(toggle("Invert vertical", *invert_y, flag(Flag::InvertY)));
            }
            Mode::Dpad {
                up,
                down,
                left,
                right,
                deadzone,
                eight_way,
            } => {
                for (s, b) in [(Sub::Up, up), (Sub::Down, down), (Sub::Left, left), (Sub::Right, right)] {
                    rows.push((
                        row(s.label(), Some(self.binding_summary(b))),
                        Op::Open(Page::Binding(sub(s))),
                        String::new(),
                    ));
                }
                rows.push(slider("Dead zone", percent(*deadzone), f(Float::Deadzone)));
                rows.push(toggle("Eight-way", *eight_way, flag(Flag::EightWay)));
            }
            Mode::Mouse {
                sensitivity,
                invert_x,
                invert_y,
            } => {
                rows.push(slider("Sensitivity", format!("{sensitivity:.1}x"), f(Float::Sensitivity)));
                rows.push(toggle("Invert horizontal", *invert_x, flag(Flag::InvertX)));
                rows.push(toggle("Invert vertical", *invert_y, flag(Flag::InvertY)));
            }
            Mode::ScrollWheel {
                degrees_per_notch,
                clockwise,
                counter_clockwise,
            } => {
                rows.push(slider(
                    "Turn per notch",
                    format!("{degrees_per_notch:.0} degrees"),
                    f(Float::Notch),
                ));
                rows.push((
                    row("Clockwise", Some(self.binding_summary(clockwise))),
                    Op::Open(Page::Binding(sub(Sub::Clockwise))),
                    String::new(),
                ));
                rows.push((
                    row("Anticlockwise", Some(self.binding_summary(counter_clockwise))),
                    Op::Open(Page::Binding(sub(Sub::CounterClockwise))),
                    String::new(),
                ));
            }
            Mode::FlickStick { pixels_per_turn } => {
                rows.push(slider(
                    "Mouse movement for a full turn",
                    format!("{pixels_per_turn:.0} px"),
                    f(Float::TurnPixels),
                ));
            }
            Mode::RadialMenu { items, on_release } => {
                for (i, item) in items.iter().enumerate() {
                    rows.push((
                        row(&format!("Item {}", i + 1), Some(item.label.clone())),
                        Op::Open(Page::Binding(Target::Radial {
                            group,
                            shifted,
                            item: i,
                        })),
                        String::new(),
                    ));
                }
                rows.push((
                    row("Add an item", None),
                    Op::Do(Do::AddRadialItem(group, shifted)),
                    String::new(),
                ));
                rows.push(toggle("Fire on release", *on_release, flag(Flag::OnRelease)));
            }
            Mode::Trigger {
                output,
                soft_threshold,
                deadzone,
                soft,
                full,
            } => {
                rows.push((
                    row(
                        "Analogue output",
                        Some(match output {
                            Some(side) => format!("{} trigger", side.label()),
                            None => "None".into(),
                        }),
                    ),
                    Op::Cycle(Field::TriggerOutput { group, shifted }, None),
                    String::new(),
                ));
                rows.push(slider("Dead zone", percent(*deadzone), f(Float::Deadzone)));
                rows.push(slider("Soft pull point", percent(*soft_threshold), f(Float::SoftThreshold)));
                rows.push((
                    row("Soft pull", Some(self.binding_summary(soft))),
                    Op::Open(Page::Binding(sub(Sub::SoftPull))),
                    String::new(),
                ));
                rows.push((
                    row("Full pull", Some(self.binding_summary(full))),
                    Op::Open(Page::Binding(sub(Sub::FullPull))),
                    String::new(),
                ));
            }
            Mode::Gyro {
                output,
                enable,
                sensitivity,
                horizontal,
                deadzone,
                invert_x,
                invert_y,
            } => {
                rows.push((
                    row("Output", Some(output.label())),
                    Op::Cycle(Field::GyroOutput { group, shifted }, None),
                    "Mouse aims directly. Camera turns a stick faster the faster you turn. Tilt \
                     holds a stick over as far as you lean, to steer."
                        .into(),
                ));
                rows.push((
                    row("Gyro is", Some(enable.label().into())),
                    Op::Cycle(Field::GyroEnable { group, shifted }, None),
                    String::new(),
                ));
                if let Some(button) = enable.button() {
                    rows.push((
                        row("Button", Some(button.label().into())),
                        Op::Cycle(Field::GyroButton { group, shifted }, None),
                        String::new(),
                    ));
                }
                rows.push(slider("Sensitivity", format!("{sensitivity:.1}x"), f(Float::Sensitivity)));
                rows.push((
                    row(
                        "Turn left and right by",
                        Some(match horizontal {
                            GyroAxis::Yaw => "Turning".into(),
                            GyroAxis::Roll => "Tilting".into(),
                        }),
                    ),
                    Op::Cycle(Field::GyroAxis { group, shifted }, None),
                    String::new(),
                ));
                rows.push(slider("Dead zone", format!("{deadzone:.2} deg/s"), f(Float::Deadzone)));
                rows.push(toggle("Invert horizontal", *invert_x, flag(Flag::InvertX)));
                rows.push(toggle("Invert vertical", *invert_y, flag(Flag::InvertY)));
            }
        }
        if !shifted {
            let shift = self.group_config(group).and_then(|c| c.shift.as_ref());
            rows.push((
                row(
                    "Mode shift",
                    Some(match shift {
                        Some(s) => s.button.label().into(),
                        None => "Off".into(),
                    }),
                ),
                Op::Cycle(Field::ShiftButton(group), None),
                "While this button is held, the source runs in another mode.".into(),
            ));
            if let Some(shift) = shift {
                rows.push((
                    row("Shifted mode", Some(ModeKind::of(&shift.mode).label().into())),
                    Op::Open(Page::Group {
                        group,
                        shifted: true,
                    }),
                    String::new(),
                ));
            }
        }
        Built {
            title: format!("{}{}", group.label(), if shifted { " (shifted)" } else { "" }),
            rows,
            detail: String::new(),
        }
    }

    fn sets_page(&self) -> Built {
        let mut rows = Vec::new();
        for (i, set) in self.layout.sets.iter().enumerate() {
            rows.push((
                row(
                    &format!("Action set: {}", set.name),
                    (i == self.set && self.layer.is_none()).then(|| "Editing".into()),
                ),
                Op::Do(Do::SelectSet(i)),
                String::new(),
            ));
        }
        rows.push((
            row("Add an action set", None),
            Op::Do(Do::AddSet),
            "A copy of the current set, to change into something else. Bind an action to switch \
             to it."
                .into(),
        ));
        for (i, layer) in self.layout.layers.iter().enumerate() {
            rows.push((
                row(
                    &format!("Layer: {}", layer.name),
                    (self.layer == Some(i)).then(|| "Editing".into()),
                ),
                Op::Do(Do::SelectLayer(Some(i))),
                String::new(),
            ));
        }
        rows.push((
            row("Add a layer", None),
            Op::Do(Do::AddLayer),
            "A layer changes only the controls you set in it, while held or toggled on.".into(),
        ));
        if self.layer.is_some() {
            rows.push((row("Remove this layer", None), Op::Do(Do::RemoveLayer), String::new()));
        } else if self.layout.sets.len() > 1 {
            rows.push((
                row("Remove this action set", None),
                Op::Do(Do::RemoveSet),
                String::new(),
            ));
        }
        Built {
            title: "Action sets and layers".into(),
            rows,
            detail: String::new(),
        }
    }

    fn choices(&self, category: Category) -> Vec<Action> {
        match category {
            Category::Gamepad => PadButton::ALL
                .iter()
                .map(|b| Action::Pad { button: *b })
                .collect(),
            Category::Stick => [Side::Left, Side::Right]
                .iter()
                .flat_map(|side| {
                    Direction::ALL.iter().map(move |direction| Action::Stick {
                        side: *side,
                        direction: *direction,
                    })
                })
                .collect(),
            Category::Trigger => vec![
                Action::Trigger { side: Side::Left },
                Action::Trigger { side: Side::Right },
            ],
            Category::Keyboard => keys::KEYS
                .iter()
                .map(|(code, _)| Action::Key { code: *code })
                .collect(),
            Category::Mouse => MouseButton::ALL
                .iter()
                .map(|b| Action::Mouse { button: *b })
                .chain(Direction::ALL.iter().map(|d| Action::Wheel { direction: *d }))
                .collect(),
            Category::Sets => (0..self.layout.sets.len())
                .map(|index| Action::ActionSet { index })
                .chain((0..self.layout.layers.len()).map(|index| Action::HoldLayer { index }))
                .chain((0..self.layout.layers.len()).map(|index| Action::ToggleLayer { index }))
                .collect(),
            Category::Spatiand => Command::ALL
                .iter()
                .map(|c| Action::Shell { command: *c })
                .collect(),
        }
    }

    // --- changes ---

    fn adjust(&mut self, field: Field, step: i32) -> Event {
        match field {
            Field::Set => {
                let count = self.layout.sets.len();
                self.set = cycle_index(self.set, count, step);
                self.layer = None;
                return Event::None;
            }
            Field::Layer => {
                let count = self.layout.layers.len() + 1;
                let current = self.layer.map(|l| l + 1).unwrap_or(0);
                let next = cycle_index(current, count, step);
                self.layer = next.checked_sub(1);
                return Event::None;
            }
            Field::Mode { group, shifted } => {
                let modes = group.modes();
                let current = ModeKind::of(&self.mode(group, shifted));
                let at = modes.iter().position(|m| *m == current).unwrap_or(0);
                let next = modes[cycle_index(at, modes.len(), step)];
                *self.mode_mut(group, shifted) = next.default_mode(group);
            }
            Field::ShiftButton(group) => {
                let current = self
                    .group_config(group)
                    .and_then(|c| c.shift.as_ref())
                    .map(|s| s.button);
                let options: Vec<Option<Button>> =
                    std::iter::once(None).chain(Button::ALL.iter().copied().map(Some)).collect();
                let at = options.iter().position(|o| *o == current).unwrap_or(0);
                let next = options[cycle_index(at, options.len(), step)];
                self.mode_mut(group, false);
                let config = self.controls_mut().groups.get_mut(&group).expect("just made");
                match next {
                    None => config.shift = None,
                    Some(button) => {
                        let base = config.mode.clone();
                        config
                            .shift
                            .get_or_insert(ModeShift { button, mode: base })
                            .button = button;
                    }
                }
            }
            Field::Float {
                group,
                shifted,
                which,
            } => {
                let (low, high, delta) = match which {
                    Float::Sensitivity => (0.1, 5.0, 0.1),
                    Float::Deadzone => match group.kind() {
                        crate::layout::GroupKind::Gyro => (0.0, 10.0, 0.25),
                        _ => (0.0, 0.6, 0.02),
                    },
                    Float::Outer => (0.5, 1.0, 0.02),
                    Float::Notch => (10.0, 120.0, 5.0),
                    Float::TurnPixels => (500.0, 20000.0, 250.0),
                    Float::SoftThreshold => (0.05, 0.95, 0.05),
                };
                if let Some(value) = float_mut(self.mode_mut(group, shifted), which) {
                    let next = (*value + delta * step as f64).clamp(low, high);
                    // Rounded to the step, so repeated presses do not wander off by float error.
                    *value = (next / delta).round() * delta;
                }
            }
            Field::Flag {
                group,
                shifted,
                which,
            } => {
                if let Some(value) = flag_mut(self.mode_mut(group, shifted), which) {
                    *value = !*value;
                }
            }
            Field::Curve { group, shifted } => {
                if let Mode::Joystick { curve, .. } = self.mode_mut(group, shifted) {
                    let at = Curve::ALL.iter().position(|c| c == curve).unwrap_or(0);
                    *curve = Curve::ALL[cycle_index(at, Curve::ALL.len(), step)];
                }
            }
            Field::StickOutput { group, shifted } => {
                if let Mode::Joystick { output, .. } = self.mode_mut(group, shifted) {
                    *output = other_side(*output);
                }
            }
            Field::TriggerOutput { group, shifted } => {
                if let Mode::Trigger { output, .. } = self.mode_mut(group, shifted) {
                    let options = [None, Some(Side::Left), Some(Side::Right)];
                    let at = options.iter().position(|o| o == output).unwrap_or(0);
                    *output = options[cycle_index(at, options.len(), step)];
                }
            }
            Field::GyroOutput { group, shifted } => {
                if let Mode::Gyro { output, .. } = self.mode_mut(group, shifted) {
                    let options = [
                        GyroOutput::Mouse,
                        GyroOutput::Camera { side: Side::Right },
                        GyroOutput::Camera { side: Side::Left },
                        GyroOutput::Tilt { side: Side::Left },
                        GyroOutput::Tilt { side: Side::Right },
                    ];
                    let at = options.iter().position(|o| o == output).unwrap_or(0);
                    *output = options[cycle_index(at, options.len(), step)];
                }
            }
            Field::GyroEnable { group, shifted } => {
                if let Mode::Gyro { enable, .. } = self.mode_mut(group, shifted) {
                    let button = enable.button().unwrap_or(Button::RPadTouch);
                    let options = [
                        GyroEnable::Always,
                        GyroEnable::WhileHeld { button },
                        GyroEnable::Toggle { button },
                        GyroEnable::OffWhileHeld { button },
                    ];
                    let at = options
                        .iter()
                        .position(|o| std::mem::discriminant(o) == std::mem::discriminant(enable))
                        .unwrap_or(0);
                    *enable = options[cycle_index(at, options.len(), step)];
                }
            }
            Field::GyroButton { group, shifted } => {
                if let Mode::Gyro { enable, .. } = self.mode_mut(group, shifted) {
                    if let Some(button) = enable.button() {
                        let at = Button::ALL.iter().position(|b| *b == button).unwrap_or(0);
                        let next = Button::ALL[cycle_index(at, Button::ALL.len(), step)];
                        *enable = match *enable {
                            GyroEnable::WhileHeld { .. } => GyroEnable::WhileHeld { button: next },
                            GyroEnable::Toggle { .. } => GyroEnable::Toggle { button: next },
                            GyroEnable::OffWhileHeld { .. } => {
                                GyroEnable::OffWhileHeld { button: next }
                            }
                            GyroEnable::Always => GyroEnable::Always,
                        };
                    }
                }
            }
            Field::GyroAxis { group, shifted } => {
                if let Mode::Gyro { horizontal, .. } = self.mode_mut(group, shifted) {
                    *horizontal = match horizontal {
                        GyroAxis::Yaw => GyroAxis::Roll,
                        GyroAxis::Roll => GyroAxis::Yaw,
                    };
                }
            }
            Field::When(target, index) => self.with_binding(target, |b| {
                if let Some(a) = b.activators.get_mut(index) {
                    let options = [
                        When::Press,
                        When::LongPress { ms: When::LONG_MS },
                        When::DoublePress { ms: When::DOUBLE_MS },
                        When::StartPress,
                        When::ReleasePress,
                        When::Chord { with: Button::L1 },
                    ];
                    let at = options
                        .iter()
                        .position(|o| std::mem::discriminant(o) == std::mem::discriminant(&a.when))
                        .unwrap_or(0);
                    a.when = options[cycle_index(at, options.len(), step)];
                }
            }),
            Field::WhenTime(target, index) => self.with_binding(target, |b| {
                if let Some(a) = b.activators.get_mut(index) {
                    match &mut a.when {
                        When::LongPress { ms } | When::DoublePress { ms } => {
                            *ms = (*ms as i64 + 50 * step as i64).clamp(100, 1500) as u32;
                        }
                        _ => {}
                    }
                }
            }),
            Field::Chord(target, index) => self.with_binding(target, |b| {
                if let Some(a) = b.activators.get_mut(index) {
                    if let When::Chord { with } = &mut a.when {
                        let at = Button::ALL.iter().position(|x| x == with).unwrap_or(0);
                        *with = Button::ALL[cycle_index(at, Button::ALL.len(), step)];
                    }
                }
            }),
            Field::Toggle(target, index) => self.with_binding(target, |b| {
                if let Some(a) = b.activators.get_mut(index) {
                    a.toggle = !a.toggle;
                }
            }),
            Field::Turbo(target, index) => self.with_binding(target, |b| {
                if let Some(a) = b.activators.get_mut(index) {
                    let at = TURBO_STEPS.iter().position(|t| *t == a.turbo_ms).unwrap_or(0);
                    a.turbo_ms = TURBO_STEPS[cycle_index(at, TURBO_STEPS.len(), step)];
                }
            }),
        }
        Event::Changed
    }

    fn run(&mut self, action: Do) -> Event {
        match action {
            Do::Revert => {
                self.layout = self.original.clone();
                self.set = self.set.min(self.layout.sets.len() - 1);
                self.layer = None;
                self.stack.truncate(1);
                Event::Changed
            }
            Do::Reset => {
                self.stack.truncate(1);
                Event::Reset
            }
            Do::AddActivator(target) => {
                let mut index = 0;
                self.with_binding(target, |b| {
                    b.activators.push(Activator::default());
                    index = b.activators.len() - 1;
                });
                self.push(Page::Activator(target, index));
                Event::Changed
            }
            Do::RemoveActivator(target, index) => {
                self.with_binding(target, |b| {
                    if index < b.activators.len() {
                        b.activators.remove(index);
                    }
                });
                self.pop(1);
                Event::Changed
            }
            Do::SetAction(target, index, slot, chosen) => {
                self.with_binding(target, |b| {
                    while b.activators.len() <= index {
                        b.activators.push(Activator::default());
                    }
                    let actions = &mut b.activators[index].actions;
                    match slot {
                        Some(i) if i < actions.len() => actions[i] = chosen,
                        _ => actions.push(chosen),
                    }
                });
                // Back past the list and the categories to the activator that asked.
                self.pop(2);
                Event::Changed
            }
            Do::RemoveAction(target, index, slot) => {
                self.with_binding(target, |b| {
                    if let Some(a) = b.activators.get_mut(index) {
                        if slot < a.actions.len() {
                            a.actions.remove(slot);
                        }
                    }
                });
                self.pop(1);
                Event::Changed
            }
            Do::SetMode(group, shifted, kind) => {
                *self.mode_mut(group, shifted) = kind.default_mode(group);
                self.pop(1);
                Event::Changed
            }
            Do::Template(index) => {
                if let Some(template) = templates::all().into_iter().nth(index) {
                    self.layout = template;
                    self.set = 0;
                    self.layer = None;
                }
                self.pop(1);
                Event::Changed
            }
            Do::SelectSet(index) => {
                self.set = index.min(self.layout.sets.len() - 1);
                self.layer = None;
                self.pop(1);
                Event::None
            }
            Do::AddSet => {
                let mut copy = self.layout.sets[self.set].clone();
                copy.name = format!("Action set {}", self.layout.sets.len() + 1);
                self.layout.sets.push(copy);
                self.set = self.layout.sets.len() - 1;
                self.layer = None;
                Event::Changed
            }
            Do::RemoveSet => {
                if self.layout.sets.len() > 1 {
                    self.layout.sets.remove(self.set);
                    self.set = 0;
                }
                Event::Changed
            }
            Do::SelectLayer(layer) => {
                self.layer = layer.filter(|l| *l < self.layout.layers.len());
                self.pop(1);
                Event::None
            }
            Do::AddLayer => {
                self.layout.layers.push(Layer {
                    name: format!("Layer {}", self.layout.layers.len() + 1),
                    controls: Controls::default(),
                });
                self.layer = Some(self.layout.layers.len() - 1);
                Event::Changed
            }
            Do::RemoveLayer => {
                if let Some(l) = self.layer.take() {
                    if l < self.layout.layers.len() {
                        self.layout.layers.remove(l);
                    }
                }
                Event::Changed
            }
            Do::AddRadialItem(group, shifted) => {
                if let Mode::RadialMenu { items, .. } = self.mode_mut(group, shifted) {
                    items.push(RadialItem {
                        label: format!("Item {}", items.len() + 1),
                        actions: Vec::new(),
                    });
                }
                Event::Changed
            }
            Do::RemoveRadialItem(group, shifted, item) => {
                if let Mode::RadialMenu { items, .. } = self.mode_mut(group, shifted) {
                    if item < items.len() {
                        items.remove(item);
                    }
                }
                self.pop(1);
                Event::Changed
            }
        }
    }
}

fn row(label: &str, value: Option<String>) -> Row {
    Row {
        label: label.into(),
        value,
    }
}

fn slider(label: &str, value: String, field: Field) -> (Row, Op, String) {
    (row(label, Some(value)), Op::Slider(field), String::new())
}

fn toggle(label: &str, on: bool, field: Field) -> (Row, Op, String) {
    (row(label, Some(on_off(on))), Op::Cycle(field, None), String::new())
}

fn on_off(on: bool) -> String {
    if on { "On" } else { "Off" }.into()
}

fn percent(value: f64) -> String {
    format!("{:.0}%", value * 100.0)
}

fn other_side(side: Side) -> Side {
    match side {
        Side::Left => Side::Right,
        Side::Right => Side::Left,
    }
}

fn cycle_index(at: usize, count: usize, step: i32) -> usize {
    if count == 0 {
        return 0;
    }
    (at as i64 + step as i64).rem_euclid(count as i64) as usize
}

fn sub_binding(mode: &Mode, sub: Sub) -> Option<&Binding> {
    match (mode, sub) {
        (Mode::Dpad { up, .. }, Sub::Up) => Some(up),
        (Mode::Dpad { down, .. }, Sub::Down) => Some(down),
        (Mode::Dpad { left, .. }, Sub::Left) => Some(left),
        (Mode::Dpad { right, .. }, Sub::Right) => Some(right),
        (Mode::ScrollWheel { clockwise, .. }, Sub::Clockwise) => Some(clockwise),
        (Mode::ScrollWheel { counter_clockwise, .. }, Sub::CounterClockwise) => {
            Some(counter_clockwise)
        }
        (Mode::Trigger { soft, .. }, Sub::SoftPull) => Some(soft),
        (Mode::Trigger { full, .. }, Sub::FullPull) => Some(full),
        _ => None,
    }
}

fn sub_binding_mut(mode: &mut Mode, sub: Sub) -> Option<&mut Binding> {
    match (mode, sub) {
        (Mode::Dpad { up, .. }, Sub::Up) => Some(up),
        (Mode::Dpad { down, .. }, Sub::Down) => Some(down),
        (Mode::Dpad { left, .. }, Sub::Left) => Some(left),
        (Mode::Dpad { right, .. }, Sub::Right) => Some(right),
        (Mode::ScrollWheel { clockwise, .. }, Sub::Clockwise) => Some(clockwise),
        (Mode::ScrollWheel { counter_clockwise, .. }, Sub::CounterClockwise) => {
            Some(counter_clockwise)
        }
        (Mode::Trigger { soft, .. }, Sub::SoftPull) => Some(soft),
        (Mode::Trigger { full, .. }, Sub::FullPull) => Some(full),
        _ => None,
    }
}

fn float_mut(mode: &mut Mode, which: Float) -> Option<&mut f64> {
    match (mode, which) {
        (Mode::Joystick { sensitivity, .. }, Float::Sensitivity)
        | (Mode::Mouse { sensitivity, .. }, Float::Sensitivity)
        | (Mode::Gyro { sensitivity, .. }, Float::Sensitivity) => Some(sensitivity),
        (Mode::Joystick { deadzone, .. }, Float::Deadzone)
        | (Mode::Dpad { deadzone, .. }, Float::Deadzone)
        | (Mode::Trigger { deadzone, .. }, Float::Deadzone)
        | (Mode::Gyro { deadzone, .. }, Float::Deadzone) => Some(deadzone),
        (Mode::Joystick { outer, .. }, Float::Outer) => Some(outer),
        (Mode::ScrollWheel {
            degrees_per_notch, ..
        }, Float::Notch) => Some(degrees_per_notch),
        (Mode::FlickStick { pixels_per_turn }, Float::TurnPixels) => Some(pixels_per_turn),
        (Mode::Trigger { soft_threshold, .. }, Float::SoftThreshold) => Some(soft_threshold),
        _ => None,
    }
}

fn flag_mut(mode: &mut Mode, which: Flag) -> Option<&mut bool> {
    match (mode, which) {
        (Mode::Joystick { invert_x, .. }, Flag::InvertX)
        | (Mode::Mouse { invert_x, .. }, Flag::InvertX)
        | (Mode::Gyro { invert_x, .. }, Flag::InvertX) => Some(invert_x),
        (Mode::Joystick { invert_y, .. }, Flag::InvertY)
        | (Mode::Mouse { invert_y, .. }, Flag::InvertY)
        | (Mode::Gyro { invert_y, .. }, Flag::InvertY) => Some(invert_y),
        (Mode::Dpad { eight_way, .. }, Flag::EightWay) => Some(eight_way),
        (Mode::RadialMenu { on_release, .. }, Flag::OnRelease) => Some(on_release),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(AppKey::Steam(1677740), "Stumble Guys", templates::gamepad())
    }

    /// Move to the row with this label and press A.
    fn choose(editor: &mut Editor, label: &str) -> Event {
        let view = editor.view();
        let index = view
            .rows
            .iter()
            .position(|r| r.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no row {label:?} on {:?}: {:?}",
                    view.title,
                    view.rows.iter().map(|r| &r.label).collect::<Vec<_>>()
                )
            });
        editor.click(index)
    }

    fn select(editor: &mut Editor, label: &str) {
        let index = editor
            .view()
            .rows
            .iter()
            .position(|r| r.label == label)
            .expect("row exists");
        editor.stack.last_mut().unwrap().1 = index;
    }

    fn value_of(editor: &Editor, label: &str) -> Option<String> {
        editor
            .view()
            .rows
            .into_iter()
            .find(|r| r.label == label)
            .and_then(|r| r.value)
    }

    #[test]
    fn the_top_page_lists_every_button_group_and_every_analogue_source() {
        let view = editor().view();
        assert_eq!(view.title, "Controller: Stumble Guys");
        for group in Group::ALL {
            assert!(view.rows.iter().any(|r| r.label == group.label()), "{group:?}");
        }
        assert!(view.rows.iter().any(|r| r.label == "Buttons"));
        assert!(!view.callouts.is_empty(), "the picture has labels");
    }

    #[test]
    fn rebinding_a_to_space_through_the_pickers() {
        let mut e = editor();
        choose(&mut e, "Buttons");
        choose(&mut e, "A");
        assert_eq!(e.view().title, "A");
        choose(&mut e, "Regular press");
        choose(&mut e, "Action 1");
        choose(&mut e, "Keyboard key");
        assert_eq!(choose(&mut e, "Key Space"), Event::Changed);
        assert_eq!(e.view().title, "A", "back at the activator that asked");
        assert_eq!(
            e.layout().sets[0].controls.buttons[&Button::A],
            Binding::key(57)
        );
        assert!(e.is_dirty());
    }

    #[test]
    fn a_long_press_can_be_added_beside_the_regular_one() {
        let mut e = editor();
        choose(&mut e, "Buttons");
        choose(&mut e, "X");
        assert_eq!(choose(&mut e, "Add an activator"), Event::Changed);
        select(&mut e, "Activation");
        e.handle(Input::Right);
        assert_eq!(value_of(&e, "Activation").as_deref(), Some("Long press"));
        select(&mut e, "Time");
        e.handle(Input::Right);
        assert_eq!(value_of(&e, "Time").as_deref(), Some("450 ms"));
        choose(&mut e, "Add an action");
        choose(&mut e, "Gamepad button");
        choose(&mut e, "Gamepad Y");
        let binding = &e.layout().sets[0].controls.buttons[&Button::X];
        assert_eq!(binding.activators.len(), 2);
        assert_eq!(binding.activators[1].when, When::LongPress { ms: 450 });
    }

    #[test]
    fn changing_a_mode_and_a_slider_edits_the_layout() {
        let mut e = editor();
        choose(&mut e, "Right stick");
        select(&mut e, "Sensitivity");
        e.handle(Input::Right);
        e.handle(Input::Right);
        assert_eq!(value_of(&e, "Sensitivity").as_deref(), Some("1.2x"));
        choose(&mut e, "Mode");
        assert_eq!(e.view().title, "Right stick mode");
        choose(&mut e, "Flick stick");
        assert!(matches!(
            e.layout().sets[0].controls.groups[&Group::RightStick].mode,
            Mode::FlickStick { .. }
        ));
    }

    #[test]
    fn gyro_can_be_turned_on_with_a_button_of_your_choosing() {
        let mut e = editor();
        choose(&mut e, "Gyro");
        select(&mut e, "Mode");
        e.handle(Input::Right);
        assert_eq!(value_of(&e, "Mode").as_deref(), Some("Gyro"));
        assert_eq!(value_of(&e, "Gyro is").as_deref(), Some("On while held"));
        assert_eq!(value_of(&e, "Button").as_deref(), Some("Right trackpad touch"));
        select(&mut e, "Button");
        e.handle(Input::Left);
        assert_eq!(value_of(&e, "Button").as_deref(), Some("Left trackpad touch"));
    }

    #[test]
    fn a_mode_shift_edits_its_own_mode_without_touching_the_base() {
        let mut e = editor();
        choose(&mut e, "Right stick");
        select(&mut e, "Mode shift");
        e.handle(Input::Right);
        assert_eq!(value_of(&e, "Mode shift").as_deref(), Some("A"));
        choose(&mut e, "Shifted mode");
        assert_eq!(e.view().title, "Right stick (shifted)");
        select(&mut e, "Mode");
        e.handle(Input::Right);
        let config = &e.layout().sets[0].controls.groups[&Group::RightStick];
        assert!(matches!(config.mode, Mode::Joystick { .. }), "base untouched");
        assert!(config.shift.is_some());
    }

    #[test]
    fn editing_a_layer_leaves_the_action_set_alone() {
        let mut e = editor();
        choose(&mut e, "Editing");
        choose(&mut e, "Add a layer");
        assert_eq!(e.layer, Some(0));
        e.handle(Input::Back);
        choose(&mut e, "Buttons");
        choose(&mut e, "B");
        choose(&mut e, "Regular press");
        choose(&mut e, "Action 1");
        choose(&mut e, "Keyboard key");
        choose(&mut e, "Key E");
        assert_eq!(e.layout().layers[0].controls.buttons[&Button::B], Binding::key(18));
        assert_eq!(
            e.layout().sets[0].controls.buttons[&Button::B],
            Binding::pad(PadButton::B)
        );
    }

    #[test]
    fn a_template_replaces_the_layout_and_undo_brings_it_back() {
        let mut e = editor();
        choose(&mut e, "Template");
        choose(&mut e, "Keyboard and mouse");
        assert_eq!(e.layout().name, "Keyboard and mouse");
        assert_eq!(choose(&mut e, "Undo changes"), Event::Changed);
        assert_eq!(e.layout().name, "Gamepad");
        assert!(!e.is_dirty());
    }

    #[test]
    fn back_climbs_out_and_closes_at_the_top() {
        let mut e = editor();
        choose(&mut e, "Buttons");
        assert_eq!(e.handle(Input::Back), Event::None);
        assert_eq!(e.handle(Input::Back), Event::Close);
    }

    #[test]
    fn a_radial_item_goes_straight_to_its_actions() {
        let mut e = editor();
        choose(&mut e, "Left stick");
        choose(&mut e, "Mode");
        choose(&mut e, "Radial menu");
        choose(&mut e, "Item 2");
        assert!(e.view().rows.iter().any(|r| r.label == "Action 1"));
        assert!(!e.view().rows.iter().any(|r| r.label == "Activation"));
        choose(&mut e, "Action 1");
        choose(&mut e, "Spatiand");
        choose(&mut e, "Take a screenshot");
        match &e.layout().sets[0].controls.groups[&Group::LeftStick].mode {
            Mode::RadialMenu { items, .. } => assert_eq!(items[1].label, "Take a screenshot"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn up_and_down_wrap_round_the_list() {
        let mut e = editor();
        e.handle(Input::Up);
        assert_eq!(e.view().cursor, e.view().rows.len() - 1);
        e.handle(Input::Down);
        assert_eq!(e.view().cursor, 0);
    }
}
