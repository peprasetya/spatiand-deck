//! A layout, running: one frame of physical input in, one frame of output out.
//!
//! Called once per rendered frame with however long the frame took. Timings — long presses,
//! double presses, turbo, pulses — are measured in that accumulated time rather than read off a
//! clock, which is what lets every one of them be tested by stepping a fake frame length.

use std::collections::{BTreeSet, HashMap, HashSet};

use crate::input::{Button, Buttons, Snapshot};
use crate::layout::{
    Binding, Controls, Curve, Group, GroupKind, GyroAxis, GyroEnable, GyroOutput, Layout, Mode,
    When,
};
use crate::output::{Action, Frame, RadialView, Side};

/// How long a one-shot activation — a tap, a start or release press, a radial pick — is held.
/// Long enough that a game polling at 30 Hz still sees it.
pub const PULSE_SECONDS: f64 = 0.05;
const STICK_MOUSE_PIXELS_PER_SECOND: f64 = 1400.0;
const GYRO_MOUSE_PIXELS_PER_DEGREE: f64 = 14.0;
/// Turning this fast pushes a camera stick all the way over, at sensitivity 1.
const GYRO_CAMERA_FULL_DPS: f64 = 240.0;
/// Tilting this far holds a steering stick all the way over, at sensitivity 1.
const TILT_FULL_DEGREES: f64 = 35.0;
const FLICK_ENGAGE: f64 = 0.9;
const FLICK_RELEASE: f64 = 0.7;
const FLICK_SECONDS: f64 = 0.1;
/// How far a stick goes over before a radial menu opens.
const RADIAL_ENGAGE: f64 = 0.5;
/// How far out a thumb has to be before its angle means anything.
const ANGLE_RADIUS: f64 = 0.3;

/// Where an activation came from: a button, or one binding inside a group's mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Source {
    Button(Button),
    Sub(Group, u8),
    Radial(Group, u8),
}

const SUB_UP: u8 = 0;
const SUB_DOWN: u8 = 1;
const SUB_LEFT: u8 = 2;
const SUB_RIGHT: u8 = 3;
const SUB_CLOCKWISE: u8 = 4;
const SUB_COUNTER: u8 = 5;
const SUB_SOFT: u8 = 6;
const SUB_FULL: u8 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    source: Source,
    activator: u8,
}

#[derive(Debug, Default, Clone)]
struct Track {
    down: bool,
    pressed_at: f64,
    /// This press is the second of a double press.
    second: bool,
    /// A press has just ended and a second may yet follow, until this time.
    double_until: Option<f64>,
}

#[derive(Debug, Default, Clone)]
struct GroupState {
    angle: Option<f64>,
    turned: f64,
    flicking: bool,
    flick_angle: f64,
    flick_owed: f64,
    radial: Option<usize>,
    click_was: bool,
    gyro_on: bool,
    gyro_button_was: bool,
    tilt: (f64, f64),
}

pub struct Engine {
    layout: Layout,
    set: usize,
    toggled_layers: BTreeSet<usize>,
    held_layers: BTreeSet<usize>,
    effective: Controls,
    effective_for: Option<(usize, Vec<usize>)>,
    now: f64,
    tracks: HashMap<Source, Track>,
    pulses: HashMap<Key, f64>,
    level_was: HashSet<Key>,
    latches: HashMap<Key, bool>,
    turbo_since: HashMap<Key, f64>,
    active_was: HashSet<(Key, u8)>,
    groups: HashMap<Group, GroupState>,
    /// Buttons that were down when the controls last changed under them. Each stays silent
    /// until it is let go, so the press that switched action set, or closed a menu, does not
    /// also fire whatever the same button does in the new state.
    suppressed: Buttons,
    last_buttons: Buttons,
    motion_remainder: (f64, f64),
}

impl Engine {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout: layout.repaired(),
            set: 0,
            toggled_layers: BTreeSet::new(),
            held_layers: BTreeSet::new(),
            effective: Controls::default(),
            effective_for: None,
            now: 0.0,
            tracks: HashMap::new(),
            pulses: HashMap::new(),
            level_was: HashSet::new(),
            latches: HashMap::new(),
            turbo_since: HashMap::new(),
            active_was: HashSet::new(),
            groups: HashMap::new(),
            suppressed: Buttons::default(),
            last_buttons: Buttons::default(),
            motion_remainder: (0.0, 0.0),
        }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// Swap the layout, as the editor does on every change. Stays on the same action set when
    /// it still exists, so editing the second set does not throw you back to the first.
    pub fn set_layout(&mut self, layout: Layout) {
        self.layout = layout.repaired();
        if self.set >= self.layout.sets.len() {
            self.set = 0;
        }
        self.toggled_layers.retain(|l| *l < self.layout.layers.len());
        self.effective_for = None;
        self.forget();
    }

    pub fn action_set(&self) -> usize {
        self.set
    }

    /// Whether the layout in force gives this button something to do.
    ///
    /// The compositor asks about the face buttons: they double as the pointer's mouse buttons,
    /// and only where a layout has not claimed them.
    pub fn binds(&self, button: Button) -> bool {
        self.effective
            .buttons
            .get(&button)
            .is_some_and(|binding| !binding.is_empty())
    }

    /// Does the layout in force play the virtual gamepad?
    ///
    /// Asked by the pointer. A game played with a pad reacts to the mouse moving over it --
    /// it swaps its button prompts, shows a cursor, and stalls while it does -- so a laser
    /// merely passing over such a game should not be reported to it as a mouse.
    pub fn drives_pad(&self) -> bool {
        let plays_pad = |action: &Action| {
            matches!(
                action,
                Action::Pad { .. } | Action::Stick { .. } | Action::Trigger { .. }
            )
        };
        self.effective
            .buttons
            .values()
            .flat_map(|binding| &binding.activators)
            .flat_map(|activator| &activator.actions)
            .any(plays_pad)
            || self
                .effective
                .groups
                .values()
                .any(|group| {
                    matches!(
                        group.mode,
                        crate::layout::Mode::Joystick { .. }
                            | crate::layout::Mode::Trigger { output: Some(_), .. }
                    )
                })
    }

    /// Let go of everything, as when a menu opens over the game.
    pub fn reset(&mut self) {
        self.held_layers.clear();
        self.forget();
    }

    fn forget(&mut self) {
        self.suppressed = self.last_buttons;
        self.tracks.clear();
        self.pulses.clear();
        self.level_was.clear();
        self.latches.clear();
        self.turbo_since.clear();
        self.active_was.clear();
        self.groups.clear();
        self.motion_remainder = (0.0, 0.0);
    }

    fn refresh_effective(&mut self) {
        let layers: Vec<usize> = self
            .toggled_layers
            .union(&self.held_layers)
            .copied()
            .collect();
        let key = (self.set, layers);
        if self.effective_for.as_ref() != Some(&key) {
            self.effective = self.layout.effective(key.0, key.1.iter().copied());
            self.effective_for = Some(key);
        }
    }

    /// Run one frame. `dt` is the frame's length in seconds.
    pub fn step(&mut self, input: &Snapshot, dt: f64) -> Frame {
        let dt = dt.clamp(0.0, 0.25);
        self.now += dt;
        self.last_buttons = input.buttons;

        let mut buttons = input.buttons;
        for b in Button::ALL {
            if self.suppressed.is_down(b) {
                if buttons.is_down(b) {
                    buttons.set(b, false);
                } else {
                    self.suppressed.set(b, false);
                }
            }
        }

        self.refresh_effective();
        let controls = self.effective.clone();
        let mut frame = Frame::default();
        let mut motion = (0.0f64, 0.0f64);
        let mut actives: Vec<(Key, Vec<Action>)> = Vec::new();

        for b in Button::ALL {
            match controls.buttons.get(&b) {
                Some(binding) => self.run_binding(
                    Source::Button(b),
                    buttons.is_down(b),
                    binding,
                    &buttons,
                    &mut actives,
                ),
                None => {
                    self.tracks.remove(&Source::Button(b));
                }
            }
        }
        for (group, config) in &controls.groups {
            let mode = match &config.shift {
                Some(shift) if buttons.is_down(shift.button) => &shift.mode,
                _ => &config.mode,
            };
            self.run_group(
                *group,
                mode,
                input,
                &buttons,
                dt,
                &mut frame,
                &mut motion,
                &mut actives,
            );
        }

        self.pulses.retain(|_, until| self.now < *until + 1.0);
        self.apply(actives, &mut frame);

        let mx = motion.0 + self.motion_remainder.0;
        let my = motion.1 + self.motion_remainder.1;
        self.motion_remainder = (mx - mx.trunc(), my - my.trunc());
        frame.mouse_motion = (mx.trunc() as i32, my.trunc() as i32);
        frame.pad.left = clamp_stick(frame.pad.left);
        frame.pad.right = clamp_stick(frame.pad.right);
        frame
    }

    fn pulse_live(&self, key: &Key) -> bool {
        self.pulses.get(key).is_some_and(|until| self.now < *until)
    }

    fn run_binding(
        &mut self,
        source: Source,
        down: bool,
        binding: &Binding,
        buttons: &Buttons,
        actives: &mut Vec<(Key, Vec<Action>)>,
    ) {
        let now = self.now;
        let long_ms = binding.activators.iter().find_map(|a| match a.when {
            When::LongPress { ms } => Some(ms),
            _ => None,
        });
        let double_ms = binding.activators.iter().find_map(|a| match a.when {
            When::DoublePress { ms } => Some(ms),
            _ => None,
        });
        let deferred = long_ms.is_some() || double_ms.is_some();

        let track = self.tracks.entry(source).or_default();
        let rising = down && !track.down;
        let falling = !down && track.down;
        let mut tap = false;
        if rising {
            track.second = track.double_until.is_some_and(|until| now <= until);
            track.double_until = None;
            track.pressed_at = now;
        }
        if falling {
            let held_ms = (now - track.pressed_at) * 1000.0;
            let was_long = long_ms.is_some_and(|ms| held_ms >= ms as f64);
            if !track.second && !was_long {
                match double_ms {
                    Some(ms) => track.double_until = Some(now + ms as f64 / 1000.0),
                    None => tap = true,
                }
            }
            track.second = false;
        }
        if !down {
            if let Some(until) = track.double_until {
                if now > until {
                    track.double_until = None;
                    tap = true;
                }
            }
        }
        track.down = down;
        let held_ms = if down {
            (now - track.pressed_at) * 1000.0
        } else {
            0.0
        };
        let second = track.second && down;

        for (index, activator) in binding.activators.iter().enumerate() {
            let key = Key {
                source,
                activator: index as u8,
            };
            let mut level = match activator.when {
                When::Press => !deferred && down,
                When::LongPress { ms } => down && held_ms >= ms as f64,
                When::DoublePress { .. } => second,
                When::StartPress | When::ReleasePress => false,
                When::Chord { with } => down && buttons.is_down(with),
            };
            let pulse = match activator.when {
                When::Press => deferred && tap,
                When::StartPress => rising,
                When::ReleasePress => falling,
                _ => false,
            };
            if pulse {
                self.pulses.insert(key, now + PULSE_SECONDS);
            }
            level |= self.pulse_live(&key);

            let was = self.level_was.contains(&key);
            if level {
                self.level_was.insert(key);
            } else {
                self.level_was.remove(&key);
            }
            let mut on = level;
            if activator.toggle {
                let latch = self.latches.entry(key).or_insert(false);
                if level && !was {
                    *latch = !*latch;
                }
                on = *latch;
            }
            match activator.turbo_ms.filter(|ms| *ms > 0) {
                Some(ms) if on => {
                    let since = *self.turbo_since.entry(key).or_insert(now);
                    let period = ms as f64 / 1000.0;
                    on = (now - since).rem_euclid(period) < period * 0.5;
                }
                _ => {
                    self.turbo_since.remove(&key);
                }
            }
            if on && !activator.actions.is_empty() {
                actives.push((key, activator.actions.clone()));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn run_group(
        &mut self,
        group: Group,
        mode: &Mode,
        input: &Snapshot,
        buttons: &Buttons,
        dt: f64,
        frame: &mut Frame,
        motion: &mut (f64, f64),
        actives: &mut Vec<(Key, Vec<Action>)>,
    ) {
        let (x, y, live) = vector(group, input);
        let click = click_button(group).is_some_and(|b| buttons.is_down(b));
        let mut state = self.groups.remove(&group).unwrap_or_default();

        match mode {
            Mode::None => {}
            Mode::Joystick {
                output,
                sensitivity,
                deadzone,
                outer,
                curve,
                invert_x,
                invert_y,
            } => {
                if live {
                    let (mut sx, mut sy) = shape(x, y, *deadzone, *outer, *curve, *sensitivity);
                    if *invert_x {
                        sx = -sx;
                    }
                    if *invert_y {
                        sy = -sy;
                    }
                    add_stick(frame, *output, sx, sy);
                }
            }
            Mode::Dpad {
                up,
                down,
                left,
                right,
                deadzone,
                eight_way,
            } => {
                let engaged = live;
                let pressed = if engaged {
                    directions(x, y, *deadzone, *eight_way)
                } else {
                    [false; 4]
                };
                for (sub, binding) in [(SUB_UP, up), (SUB_DOWN, down), (SUB_LEFT, left), (SUB_RIGHT, right)] {
                    self.run_binding(
                        Source::Sub(group, sub),
                        pressed[sub as usize],
                        binding,
                        buttons,
                        actives,
                    );
                }
            }
            Mode::Mouse {
                sensitivity,
                invert_x,
                invert_y,
            } => {
                let (mut dx, mut dy) = (0.0, 0.0);
                if live {
                    let (sx, sy) = shape(x, y, 0.08, 0.95, Curve::Relaxed, 1.0);
                    dx = sx * STICK_MOUSE_PIXELS_PER_SECOND * sensitivity * dt;
                    dy = -sy * STICK_MOUSE_PIXELS_PER_SECOND * sensitivity * dt;
                }
                if *invert_x {
                    dx = -dx;
                }
                if *invert_y {
                    dy = -dy;
                }
                motion.0 += dx;
                motion.1 += dy;
            }
            Mode::ScrollWheel {
                degrees_per_notch,
                clockwise,
                counter_clockwise,
            } => {
                let radius = x.hypot(y);
                let (mut cw, mut ccw) = (false, false);
                if live && radius > ANGLE_RADIUS {
                    let angle = y.atan2(x).to_degrees();
                    if let Some(previous) = state.angle {
                        state.turned += wrap_degrees(angle - previous);
                    }
                    state.angle = Some(angle);
                    // A notch is a press, so two in a row need an up in between. Holding the
                    // rest of the turn for the next frame costs at most one frame of lag.
                    let resting = |s: u8| {
                        !self
                            .tracks
                            .get(&Source::Sub(group, s))
                            .is_some_and(|t| t.down)
                    };
                    let notch = degrees_per_notch.max(5.0);
                    // Angles grow anticlockwise, so a clockwise turn is a falling angle.
                    if state.turned <= -notch && resting(SUB_CLOCKWISE) {
                        state.turned += notch;
                        cw = true;
                    } else if state.turned >= notch && resting(SUB_COUNTER) {
                        state.turned -= notch;
                        ccw = true;
                    }
                } else {
                    state.angle = None;
                    state.turned = 0.0;
                }
                self.run_binding(Source::Sub(group, SUB_CLOCKWISE), cw, clockwise, buttons, actives);
                self.run_binding(
                    Source::Sub(group, SUB_COUNTER),
                    ccw,
                    counter_clockwise,
                    buttons,
                    actives,
                );
            }
            Mode::FlickStick { pixels_per_turn } => {
                let magnitude = x.hypot(y);
                let per_degree = pixels_per_turn / 360.0;
                let held = live
                    && (magnitude >= FLICK_ENGAGE || (state.flicking && magnitude >= FLICK_RELEASE));
                if held {
                    // Zero straight up, positive to the right: a flick right turns right.
                    let angle = x.atan2(y).to_degrees();
                    if state.flicking {
                        motion.0 += wrap_degrees(angle - state.flick_angle) * per_degree;
                    } else {
                        state.flicking = true;
                        state.flick_owed += angle * per_degree;
                    }
                    state.flick_angle = angle;
                } else {
                    state.flicking = false;
                }
                if state.flick_owed != 0.0 {
                    let share = if dt >= FLICK_SECONDS {
                        state.flick_owed
                    } else {
                        state.flick_owed * (dt / FLICK_SECONDS).max(0.34)
                    };
                    let share = if (state.flick_owed - share).abs() < 1.0 {
                        state.flick_owed
                    } else {
                        share
                    };
                    motion.0 += share;
                    state.flick_owed -= share;
                }
            }
            Mode::RadialMenu { items, on_release } => {
                let count = items.len();
                let engaged = count > 0
                    && live
                    && (x.hypot(y) >= RADIAL_ENGAGE
                        || (state.radial.is_some() && x.hypot(y) >= ANGLE_RADIUS));
                let selected = if engaged && x.hypot(y) > ANGLE_RADIUS {
                    Some(sector(x, y, count))
                } else if engaged {
                    state.radial
                } else {
                    None
                };
                let fired = if *on_release {
                    if !engaged {
                        state.radial
                    } else {
                        None
                    }
                } else if click && !state.click_was {
                    selected
                } else {
                    None
                };
                if let Some(index) = fired {
                    self.pulses.insert(
                        Key {
                            source: Source::Radial(group, index as u8),
                            activator: 0,
                        },
                        self.now + PULSE_SECONDS,
                    );
                }
                state.radial = if engaged { selected } else { None };
                state.click_was = click;
                if engaged {
                    frame.radial = Some(RadialView {
                        group,
                        labels: items.iter().map(|i| i.label.clone()).collect(),
                        selected,
                    });
                }
                for (index, item) in items.iter().enumerate() {
                    let key = Key {
                        source: Source::Radial(group, index as u8),
                        activator: 0,
                    };
                    if self.pulse_live(&key) && !item.actions.is_empty() {
                        actives.push((key, item.actions.clone()));
                    }
                }
            }
            Mode::PointerClick => {
                if group.kind() == GroupKind::Trigger {
                    frame.pointer_clicks[side_index(group.side())] = true;
                }
            }
            Mode::Trigger {
                output,
                soft_threshold,
                deadzone,
                soft,
                full,
            } => {
                let value = x;
                if let Some(side) = output {
                    let out = if value <= *deadzone {
                        0.0
                    } else {
                        ((value - deadzone) / (1.0 - deadzone).max(1e-3)).min(1.0)
                    } as f32;
                    match side {
                        Side::Left => frame.pad.left_trigger = frame.pad.left_trigger.max(out),
                        Side::Right => frame.pad.right_trigger = frame.pad.right_trigger.max(out),
                    }
                }
                let full_button = match group.side() {
                    Side::Left => Button::L2,
                    Side::Right => Button::R2,
                };
                self.run_binding(
                    Source::Sub(group, SUB_SOFT),
                    value >= *soft_threshold,
                    soft,
                    buttons,
                    actives,
                );
                self.run_binding(
                    Source::Sub(group, SUB_FULL),
                    buttons.is_down(full_button),
                    full,
                    buttons,
                    actives,
                );
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
                let rates = match group {
                    Group::Gyro => input.gyro,
                    Group::GlassesGyro => input.glasses_gyro,
                    _ => None,
                };
                let on = match *enable {
                    GyroEnable::Always => true,
                    GyroEnable::WhileHeld { button } => buttons.is_down(button),
                    GyroEnable::OffWhileHeld { button } => !buttons.is_down(button),
                    GyroEnable::Toggle { button } => {
                        let now_down = buttons.is_down(button);
                        if now_down && !state.gyro_button_was {
                            state.gyro_on = !state.gyro_on;
                        }
                        state.gyro_button_was = now_down;
                        state.gyro_on
                    }
                };
                match rates {
                    Some(r) if on => {
                        // Positive right and positive up, whichever rotation is doing it.
                        let mut h = match horizontal {
                            GyroAxis::Yaw => -(r.yaw as f64),
                            GyroAxis::Roll => r.roll as f64,
                        };
                        let mut v = r.pitch as f64;
                        if h.abs() < *deadzone {
                            h = 0.0;
                        }
                        if v.abs() < *deadzone {
                            v = 0.0;
                        }
                        if *invert_x {
                            h = -h;
                        }
                        if *invert_y {
                            v = -v;
                        }
                        match *output {
                            GyroOutput::Mouse => {
                                motion.0 += h * dt * GYRO_MOUSE_PIXELS_PER_DEGREE * sensitivity;
                                motion.1 -= v * dt * GYRO_MOUSE_PIXELS_PER_DEGREE * sensitivity;
                            }
                            GyroOutput::Camera { side } => add_stick(
                                frame,
                                side,
                                (h * sensitivity / GYRO_CAMERA_FULL_DPS).clamp(-1.0, 1.0),
                                (v * sensitivity / GYRO_CAMERA_FULL_DPS).clamp(-1.0, 1.0),
                            ),
                            GyroOutput::Tilt { side } => {
                                state.tilt.0 += h * dt;
                                state.tilt.1 += v * dt;
                                add_stick(
                                    frame,
                                    side,
                                    (state.tilt.0 * sensitivity / TILT_FULL_DEGREES)
                                        .clamp(-1.0, 1.0),
                                    (state.tilt.1 * sensitivity / TILT_FULL_DEGREES)
                                        .clamp(-1.0, 1.0),
                                );
                            }
                        }
                    }
                    // Tilt is measured from where the gyro came on, so it starts again each time.
                    _ => state.tilt = (0.0, 0.0),
                }
            }
        }
        self.groups.insert(group, state);
    }

    fn apply(&mut self, actives: Vec<(Key, Vec<Action>)>, frame: &mut Frame) {
        let mut active_now = HashSet::new();
        let mut keys = BTreeSet::new();
        let mut mouse = BTreeSet::new();
        let mut held_layers = BTreeSet::new();
        let mut switch_to = None;
        for (key, actions) in actives {
            for (index, action) in actions.iter().enumerate() {
                let id = (key, index as u8);
                let rising = !self.active_was.contains(&id);
                active_now.insert(id);
                match *action {
                    Action::Pad { button } => frame.pad.buttons |= button.bit(),
                    Action::Stick { side, direction } => {
                        let (dx, dy) = direction.vector();
                        add_stick(frame, side, dx as f64, dy as f64);
                    }
                    Action::Trigger { side } => match side {
                        Side::Left => frame.pad.left_trigger = 1.0,
                        Side::Right => frame.pad.right_trigger = 1.0,
                    },
                    Action::Key { code } => {
                        keys.insert(code);
                    }
                    Action::Mouse { button } => {
                        mouse.insert(button);
                    }
                    Action::Wheel { direction } => {
                        if rising {
                            let (dx, dy) = direction.vector();
                            frame.wheel.0 += dx as i32;
                            frame.wheel.1 += dy as i32;
                        }
                    }
                    Action::ActionSet { index } => {
                        if rising {
                            switch_to = Some(index);
                        }
                    }
                    Action::HoldLayer { index } => {
                        held_layers.insert(index);
                    }
                    Action::ToggleLayer { index } => {
                        if rising && !self.toggled_layers.remove(&index) {
                            self.toggled_layers.insert(index);
                        }
                    }
                    Action::Shell { command } => {
                        if rising {
                            frame.commands.push(command);
                        }
                    }
                }
            }
        }
        self.active_was = active_now;
        self.held_layers = held_layers;
        frame.keys = keys.into_iter().collect();
        frame.mouse_buttons = mouse.into_iter().collect();
        if let Some(index) = switch_to {
            if index < self.layout.sets.len() && index != self.set {
                self.set = index;
                self.toggled_layers.clear();
                self.held_layers.clear();
                self.forget();
            }
        }
    }
}

fn side_index(side: Side) -> usize {
    match side {
        Side::Left => 0,
        Side::Right => 1,
    }
}

/// A source's position, and whether it means anything right now: a trackpad nobody is touching
/// has a stale position that must not be read.
fn vector(group: Group, s: &Snapshot) -> (f64, f64, bool) {
    match group {
        Group::LeftStick => (s.left_stick.0 as f64, s.left_stick.1 as f64, true),
        Group::RightStick => (s.right_stick.0 as f64, s.right_stick.1 as f64, true),
        Group::LeftTrigger => (s.left_trigger as f64, 0.0, true),
        Group::RightTrigger => (s.right_trigger as f64, 0.0, true),
        Group::Gyro | Group::GlassesGyro => (0.0, 0.0, false),
    }
}

fn click_button(group: Group) -> Option<Button> {
    match group {
        Group::LeftStick => Some(Button::LStick),
        Group::RightStick => Some(Button::RStick),
        _ => None,
    }
}

fn add_stick(frame: &mut Frame, side: Side, x: f64, y: f64) {
    let stick = match side {
        Side::Left => &mut frame.pad.left,
        Side::Right => &mut frame.pad.right,
    };
    stick.0 += x as f32;
    stick.1 += y as f32;
}

fn clamp_stick((x, y): (f32, f32)) -> (f32, f32) {
    let magnitude = x.hypot(y);
    if magnitude > 1.0 {
        (x / magnitude, y / magnitude)
    } else {
        (x, y)
    }
}

fn shape(x: f64, y: f64, deadzone: f64, outer: f64, curve: Curve, sensitivity: f64) -> (f64, f64) {
    let magnitude = x.hypot(y);
    if magnitude <= deadzone || magnitude < 1e-6 {
        return (0.0, 0.0);
    }
    let span = (outer - deadzone).max(1e-3);
    let travel = ((magnitude - deadzone) / span).min(1.0).powf(curve.exponent());
    let scaled = (travel * sensitivity).min(1.0);
    (x / magnitude * scaled, y / magnitude * scaled)
}

fn wrap_degrees(mut d: f64) -> f64 {
    while d > 180.0 {
        d -= 360.0;
    }
    while d < -180.0 {
        d += 360.0;
    }
    d
}

/// `[up, down, left, right]`.
///
/// Four-way takes the dominant axis only. Eight-way gives each direction a 135 degree arc, so
/// the diagonals press two at once — what a keyboard's WASD needs to walk at an angle.
fn directions(x: f64, y: f64, deadzone: f64, eight_way: bool) -> [bool; 4] {
    if x.hypot(y) <= deadzone {
        return [false; 4];
    }
    if eight_way {
        let angle = y.atan2(x).to_degrees();
        let within = |centre: f64| wrap_degrees(angle - centre).abs() <= 67.5;
        [within(90.0), within(-90.0), within(180.0), within(0.0)]
    } else if x.abs() >= y.abs() {
        [false, false, x < 0.0, x > 0.0]
    } else {
        [y > 0.0, y < 0.0, false, false]
    }
}

/// Which of `count` items a direction points at, item 0 straight up and counting clockwise.
fn sector(x: f64, y: f64, count: usize) -> usize {
    let mut angle = x.atan2(y).to_degrees();
    if angle < 0.0 {
        angle += 360.0;
    }
    let width = 360.0 / count as f64;
    (((angle + width / 2.0) / width).floor() as usize) % count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Rates;
    use crate::layout::{ActionSet, Activator, GroupConfig, Layer, ModeShift, RadialItem};
    use crate::output::{Command, Direction, MouseButton, PadButton};
    use crate::templates;

    const FRAME: f64 = 1.0 / 72.0;

    #[test]
    fn a_gamepad_layout_plays_the_pad_and_the_desktop_does_not() {
        // The effective controls are worked out on a step, which the session takes every frame.
        let drives = |layout| {
            let mut engine = Engine::new(layout);
            engine.step(&Snapshot::default(), FRAME);
            engine.drives_pad()
        };
        assert!(drives(templates::gamepad()));
        assert!(!drives(templates::desktop()));
        assert!(!drives(templates::keyboard_and_mouse()));
    }

    fn layout_of(controls: Controls) -> Layout {
        Layout {
            name: "test".into(),
            description: String::new(),
            sets: vec![ActionSet {
                name: "one".into(),
                controls,
            }],
            layers: Vec::new(),
        }
    }

    fn pressing(buttons: &[Button]) -> Snapshot {
        let mut s = Snapshot::default();
        for b in buttons {
            s.buttons.set(*b, true);
        }
        s
    }

    /// Step `seconds` of frames with this input, returning the last frame.
    fn hold(engine: &mut Engine, input: &Snapshot, seconds: f64) -> Frame {
        let mut frame = Frame::default();
        let steps = (seconds / FRAME).round().max(1.0) as usize;
        for _ in 0..steps {
            frame = engine.step(input, FRAME);
        }
        frame
    }

    /// Every frame over `seconds`, for looking for something that happened at any point.
    fn frames(engine: &mut Engine, input: &Snapshot, seconds: f64) -> Vec<Frame> {
        let steps = (seconds / FRAME).round().max(1.0) as usize;
        (0..steps).map(|_| engine.step(input, FRAME)).collect()
    }

    fn binding(when: When, action: Action) -> Activator {
        Activator {
            when,
            actions: vec![action],
            ..Default::default()
        }
    }

    const W: Action = Action::Key { code: 17 };
    const E: Action = Action::Key { code: 18 };

    #[test]
    fn the_gamepad_template_passes_the_deck_straight_through() {
        let mut engine = Engine::new(templates::gamepad());
        let mut input = pressing(&[Button::A, Button::R1]);
        input.left_stick = (0.0, 1.0);
        input.right_trigger = 1.0;
        let frame = engine.step(&input, FRAME);
        assert!(frame.pad.is_down(PadButton::A));
        assert!(frame.pad.is_down(PadButton::RightBumper));
        assert!(!frame.pad.is_down(PadButton::B));
        assert!(frame.pad.left.1 > 0.99, "{:?}", frame.pad.left);
        assert_eq!(frame.pad.right_trigger, 1.0);
        assert!(frame.keys.is_empty());
    }

    #[test]
    fn a_resting_stick_inside_the_dead_zone_reads_centred() {
        let mut engine = Engine::new(templates::gamepad());
        let input = Snapshot {
            left_stick: (0.05, -0.03),
            ..Default::default()
        };
        assert_eq!(engine.step(&input, FRAME).pad.left, (0.0, 0.0));
    }

    #[test]
    fn a_long_press_fires_after_its_time_and_a_short_one_is_a_tap() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::X,
            Binding {
                activators: vec![
                    binding(When::Press, W),
                    binding(When::LongPress { ms: 400 }, E),
                ],
            },
        );
        let mut engine = Engine::new(layout_of(c.clone()));
        let held = pressing(&[Button::X]);
        let early = hold(&mut engine, &held, 0.2);
        assert!(early.keys.is_empty(), "nothing yet: it might still be long");
        let late = hold(&mut engine, &held, 0.3);
        assert_eq!(late.keys, vec![18], "held past 400 ms is the long press");
        let after = frames(&mut engine, &Snapshot::default(), 0.2);
        assert!(after.iter().all(|f| !f.keys.contains(&17)), "a long press is not also a tap");

        let mut engine = Engine::new(layout_of(c));
        hold(&mut engine, &held, 0.1);
        let released = frames(&mut engine, &Snapshot::default(), 0.1);
        assert!(released.iter().any(|f| f.keys == vec![17]), "a short press taps");
        assert!(released.iter().all(|f| !f.keys.contains(&18)));
    }

    #[test]
    fn a_double_press_fires_on_the_second_press_and_a_single_waits_it_out() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::Y,
            Binding {
                activators: vec![
                    binding(When::Press, W),
                    binding(When::DoublePress { ms: 250 }, E),
                ],
            },
        );
        let mut engine = Engine::new(layout_of(c.clone()));
        let held = pressing(&[Button::Y]);
        let idle = Snapshot::default();
        hold(&mut engine, &held, 0.05);
        hold(&mut engine, &idle, 0.05);
        let second = hold(&mut engine, &held, 0.05);
        assert_eq!(second.keys, vec![18]);
        let rest = frames(&mut engine, &idle, 0.5);
        assert!(rest.iter().all(|f| !f.keys.contains(&17)), "a double is not also a single");

        let mut engine = Engine::new(layout_of(c));
        hold(&mut engine, &held, 0.05);
        let waiting = frames(&mut engine, &idle, 0.2);
        assert!(waiting.iter().all(|f| f.keys.is_empty()), "still might be a double");
        let later = frames(&mut engine, &idle, 0.2);
        assert!(later.iter().any(|f| f.keys == vec![17]), "then it is a single");
    }

    #[test]
    fn a_toggle_latches_and_turbo_pulses() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::A,
            Binding {
                activators: vec![Activator {
                    actions: vec![Action::Pad {
                        button: PadButton::A,
                    }],
                    toggle: true,
                    ..Default::default()
                }],
            },
        );
        c.buttons.insert(
            Button::B,
            Binding {
                activators: vec![Activator {
                    actions: vec![Action::Pad {
                        button: PadButton::B,
                    }],
                    turbo_ms: Some(100),
                    ..Default::default()
                }],
            },
        );
        let mut engine = Engine::new(layout_of(c));
        hold(&mut engine, &pressing(&[Button::A]), 0.05);
        let latched = hold(&mut engine, &Snapshot::default(), 0.3);
        assert!(latched.pad.is_down(PadButton::A), "on after letting go");
        hold(&mut engine, &pressing(&[Button::A]), 0.05);
        let off = hold(&mut engine, &Snapshot::default(), 0.1);
        assert!(!off.pad.is_down(PadButton::A), "the second press turns it off");

        let turbo = frames(&mut engine, &pressing(&[Button::B]), 0.5);
        let on = turbo.iter().filter(|f| f.pad.is_down(PadButton::B)).count();
        assert!(on > 5 && on < turbo.len() - 5, "{on} of {} frames on", turbo.len());
    }

    #[test]
    fn a_chord_needs_both_buttons() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::A,
            Binding {
                activators: vec![binding(When::Chord { with: Button::L4 }, E)],
            },
        );
        let mut engine = Engine::new(layout_of(c));
        assert!(engine.step(&pressing(&[Button::A]), FRAME).keys.is_empty());
        assert_eq!(
            engine.step(&pressing(&[Button::A, Button::L4]), FRAME).keys,
            vec![18]
        );
    }

    #[test]
    fn a_mode_shift_swaps_the_mode_only_while_held() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::RightStick,
            GroupConfig {
                mode: crate::layout::ModeKind::Joystick.default_mode(Group::RightStick),
                shift: Some(ModeShift {
                    button: Button::L1,
                    mode: crate::layout::ModeKind::Mouse.default_mode(Group::RightStick),
                }),
            },
        );
        let mut engine = Engine::new(layout_of(c));
        let mut input = Snapshot {
            right_stick: (1.0, 0.0),
            ..Default::default()
        };
        let plain = hold(&mut engine, &input, 0.1);
        assert!(plain.pad.right.0 > 0.9 && plain.mouse_motion == (0, 0));
        input.buttons.set(Button::L1, true);
        let shifted = hold(&mut engine, &input, 0.1);
        assert_eq!(shifted.pad.right, (0.0, 0.0));
        assert!(shifted.mouse_motion.0 > 0);
    }

    #[test]
    fn switching_action_sets_does_not_fire_the_button_that_switched() {
        let mut first = Controls::default();
        first.buttons.insert(
            Button::View,
            Binding::press(Action::ActionSet { index: 1 }),
        );
        let mut second = Controls::default();
        second.buttons.insert(Button::View, Binding::key(1));
        second.buttons.insert(Button::A, Binding::key(57));
        let layout = Layout {
            name: "sets".into(),
            description: String::new(),
            sets: vec![
                ActionSet {
                    name: "walk".into(),
                    controls: first,
                },
                ActionSet {
                    name: "menu".into(),
                    controls: second,
                },
            ],
            layers: Vec::new(),
        };
        let mut engine = Engine::new(layout);
        engine.step(&pressing(&[Button::View]), FRAME);
        assert_eq!(engine.action_set(), 1);
        let still_held = hold(&mut engine, &pressing(&[Button::View]), 0.2);
        assert!(still_held.keys.is_empty(), "View is Esc now, but not until it is let go");
        hold(&mut engine, &Snapshot::default(), 0.05);
        assert_eq!(engine.step(&pressing(&[Button::View]), FRAME).keys, vec![1]);
        assert_eq!(engine.step(&pressing(&[Button::A]), FRAME).keys, vec![57]);
    }

    #[test]
    fn a_held_layer_overrides_while_its_button_is_down() {
        let mut base = Controls::default();
        base.buttons.insert(Button::A, Binding::key(57));
        base.buttons
            .insert(Button::L5, Binding::press(Action::HoldLayer { index: 0 }));
        let mut over = Controls::default();
        over.buttons.insert(Button::A, Binding::key(18));
        let mut layout = layout_of(base);
        layout.layers.push(Layer {
            name: "alt".into(),
            controls: over,
        });
        let mut engine = Engine::new(layout);
        assert_eq!(engine.step(&pressing(&[Button::A]), FRAME).keys, vec![57]);
        hold(&mut engine, &Snapshot::default(), 0.05);
        hold(&mut engine, &pressing(&[Button::L5]), 0.05);
        assert_eq!(
            hold(&mut engine, &pressing(&[Button::L5, Button::A]), 0.05).keys,
            vec![18]
        );
    }

    #[test]
    fn a_stick_as_a_mouse_moves_while_it_is_pushed_over() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::RightStick,
            GroupConfig::new(crate::layout::ModeKind::Mouse.default_mode(Group::RightStick)),
        );
        let mut engine = Engine::new(layout_of(c));
        let pushed = Snapshot {
            right_stick: (1.0, 0.0),
            ..Default::default()
        };
        let moved: i32 = frames(&mut engine, &pushed, 0.2)
            .iter()
            .map(|f| f.mouse_motion.0)
            .sum();
        assert!(moved > 100, "{moved} px in a fifth of a second");
        assert_eq!(engine.step(&Snapshot::default(), FRAME).mouse_motion, (0, 0));
    }

    #[test]
    fn the_gyro_aims_only_while_its_button_is_held() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::Gyro,
            GroupConfig::new(crate::layout::ModeKind::Gyro.default_mode(Group::Gyro)),
        );
        let mut engine = Engine::new(layout_of(c));
        let mut input = Snapshot {
            // Turning right at 90 degrees a second.
            gyro: Some(Rates {
                pitch: 0.0,
                yaw: -90.0,
                roll: 0.0,
            }),
            ..Default::default()
        };
        assert_eq!(hold(&mut engine, &input, 0.1).mouse_motion, (0, 0));
        input.buttons.set(Button::RPadTouch, true);
        assert!(hold(&mut engine, &input, 0.1).mouse_motion.0 > 0, "right turn, right motion");
    }

    #[test]
    fn a_radial_menu_fires_the_item_under_the_thumb_on_release() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::LeftStick,
            GroupConfig::new(Mode::RadialMenu {
                items: ["up", "right", "down", "left"]
                    .iter()
                    .enumerate()
                    .map(|(i, label)| RadialItem {
                        label: label.to_string(),
                        actions: vec![Action::Key {
                            code: 2 + i as u16,
                        }],
                    })
                    .collect(),
                on_release: true,
            }),
        );
        let mut engine = Engine::new(layout_of(c));
        let pushed_right = Snapshot {
            left_stick: (0.8, 0.0),
            ..Default::default()
        };
        let open = hold(&mut engine, &pushed_right, 0.1);
        let view = open.radial.expect("the menu is showing");
        assert_eq!(view.selected, Some(1));
        assert!(open.keys.is_empty(), "nothing fires while choosing");
        let released = frames(&mut engine, &Snapshot::default(), 0.1);
        assert!(released.iter().any(|f| f.keys == vec![3]), "the right-hand item");
    }

    #[test]
    fn circling_a_stick_scrolls_a_notch_at_a_time() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::LeftStick,
            GroupConfig::new(crate::layout::ModeKind::ScrollWheel.default_mode(Group::LeftStick)),
        );
        let mut engine = Engine::new(layout_of(c));
        let mut notches = 0;
        // Half a turn clockwise, starting at the top.
        for step in 0..=60 {
            let angle = std::f32::consts::FRAC_PI_2 - std::f32::consts::PI * step as f32 / 60.0;
            let input = Snapshot {
                left_stick: (angle.cos() * 0.8, angle.sin() * 0.8),
                ..Default::default()
            };
            notches += engine.step(&input, FRAME).wheel.1;
        }
        assert!(
            (-6..=-4).contains(&notches),
            "180 degrees at 30 a notch is about six down, got {notches}"
        );
    }

    #[test]
    fn a_flick_turns_to_face_the_stick_then_rotating_keeps_turning() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::RightStick,
            GroupConfig::new(Mode::FlickStick {
                pixels_per_turn: 3600.0,
            }),
        );
        let mut engine = Engine::new(layout_of(c));
        let right = Snapshot {
            right_stick: (1.0, 0.0),
            ..Default::default()
        };
        let total: i32 = frames(&mut engine, &right, 0.3)
            .iter()
            .map(|f| f.mouse_motion.0)
            .sum();
        assert!((880..=920).contains(&total), "a quarter turn is 900 px, got {total}");
    }

    #[test]
    fn the_desktop_template_hands_the_triggers_to_the_pointer() {
        let mut engine = Engine::new(templates::desktop());
        let frame = engine.step(&pressing(&[Button::A]), FRAME);
        assert_eq!(frame.pointer_clicks, [true, true]);
        assert_eq!(frame.keys, vec![28], "A is Enter, as it always was");
    }

    #[test]
    fn no_layout_can_take_a_trackpad() {
        // The pads are Spatiand's pointer in every layout, which is why they are not a source
        // at all. A game's layout that could take them would take the way of pointing at
        // anything -- a window, a menu, the game's own buttons -- with it.
        for group in Group::ALL {
            assert!(!group.label().contains("trackpad"), "{group:?}");
        }
    }

    #[test]
    fn a_shell_command_fires_once_per_press() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::R5,
            Binding::press(Action::Shell {
                command: Command::Screenshot,
            }),
        );
        let mut engine = Engine::new(layout_of(c));
        let held = frames(&mut engine, &pressing(&[Button::R5]), 0.5);
        let fired: usize = held.iter().map(|f| f.commands.len()).sum();
        assert_eq!(fired, 1);
    }

    #[test]
    fn reset_releases_everything_and_mutes_buttons_still_held() {
        let mut engine = Engine::new(templates::gamepad());
        let held = pressing(&[Button::A]);
        assert!(engine.step(&held, FRAME).pad.is_down(PadButton::A));
        engine.reset();
        assert!(!engine.step(&held, FRAME).pad.is_down(PadButton::A));
        engine.step(&Snapshot::default(), FRAME);
        assert!(engine.step(&held, FRAME).pad.is_down(PadButton::A));
    }

    #[test]
    fn a_trigger_is_analogue_and_its_soft_pull_is_a_button() {
        let mut c = Controls::default();
        c.groups.insert(
            Group::RightTrigger,
            GroupConfig::new(Mode::Trigger {
                output: Some(Side::Right),
                soft_threshold: 0.5,
                deadzone: 0.1,
                soft: Binding::press(Action::Mouse {
                    button: MouseButton::Left,
                }),
                full: Binding::default(),
            }),
        );
        let mut engine = Engine::new(layout_of(c));
        let light = Snapshot {
            right_trigger: 0.3,
            ..Default::default()
        };
        let frame = engine.step(&light, FRAME);
        assert!((frame.pad.right_trigger - 0.222).abs() < 0.01);
        assert!(frame.mouse_buttons.is_empty());
        let firm = Snapshot {
            right_trigger: 0.6,
            ..Default::default()
        };
        assert_eq!(engine.step(&firm, FRAME).mouse_buttons, vec![MouseButton::Left]);
    }

    #[test]
    fn eight_way_presses_two_directions_on_a_diagonal() {
        assert_eq!(directions(0.7, 0.7, 0.3, true), [true, false, false, true]);
        assert_eq!(directions(0.7, 0.7, 0.3, false), [false, false, false, true]);
        assert_eq!(directions(0.1, 0.1, 0.3, true), [false; 4]);
    }

    #[test]
    fn a_stick_standing_in_for_a_button_pushes_all_the_way() {
        let mut c = Controls::default();
        c.buttons.insert(
            Button::L4,
            Binding::press(Action::Stick {
                side: Side::Left,
                direction: Direction::Up,
            }),
        );
        let mut engine = Engine::new(layout_of(c));
        assert_eq!(engine.step(&pressing(&[Button::L4]), FRAME).pad.left, (0.0, 1.0));
    }
}
