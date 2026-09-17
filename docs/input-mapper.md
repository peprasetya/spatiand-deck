# Controller layouts

Spatiand's answer to Steam Input: every physical control is remappable, per application, and
what a game sees is one gamepad. Written because Game Mode already works this way and the
muscle memory should carry over — and because a spatial session has two inputs Game Mode has
never had, the glasses' gyro and their temple buttons.

Code: `spatiand-mapper` (all of the model and the engine, no hardware), `spatiand-input`
(`virtual_pad`, `gamepad`), `spatiand::controls` (what joins them to the session).

---

## 1. What a game sees

**One gamepad.** A uinput device wearing Steam's own virtual gamepad identity
(`28de:11ff`, "Steam Virtual Gamepad"). It is created when the session starts, so a game
launched later finds it, and it goes when the session ends.

The identity is the whole trick, and it was wrong for a while. A wired Xbox 360 pad
(`045e:028e`) is the obvious choice — every game knows it — and it is invisible: Steam hands
every game it launches a list of several hundred controller ids that Steam Input handles, with
`045e:028e` among them, and Proton honours that list. **[verified]** with Stumble Guys running,
by creating one pad of each identity at the same moment and watching which node `winedevice.exe`
opened: the Steam one within four seconds, the Xbox one never. The wire format either way is an
Xbox pad's, so this is what the device is *called*, not what it is.

**And nothing else.** Every application Spatiand launches gets
`SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT=0x28de/0x11ff` and
`SDL_GAMECONTROLLER_ALLOW_STEAM_VIRTUAL_GAMEPAD=1`. The second is not optional: SDL hides a
Steam virtual gamepad unless it is told not to, on the understanding that Steam will offer the
same controller through its own API — true for a game Steam launched, false for anything
Spatiand launched itself. **[verified]** with one pad up and one variable changed: one joystick
with it, none without. Steam passes its own environment to
every game it starts, so a game launched through Steam inherits it too. **[verified]** on the
Deck under Proton 11: with it set, the only XInput device Wine created was the virtual pad;
without it, Wine also read the Deck's controller straight from hidraw and the game saw two.

**The Deck's own gamepad disappears by itself.** While Spatiand holds the vendor hidraw
interface, `hid-steam` unregisters the `Steam Deck` and `Steam Deck Motion Sensors` devices.
**[verified]**

**And a game really does see it.** **[verified]** on the Deck by asking SDL, which is what a
game asks: with the pad held up by `cargo run -p spatiand-input --example virtual-pad`, an SDL
program listed `Steam Deck 28de/1205` and `Xbox 360 Controller 045e/028e`; with the environment
above, it listed only the second, as a game controller, and received every A press and the full
sweep of the left stick. The kernel half is a test of its own —
`the_kernel_publishes_it_as_a_gamepad`, ignored unless run on the Deck — which also checks that
an ordinary process can *open* the device node, since the permission to do so comes from a udev
rule and not from anything here. It arrives about 40 ms after the device does.

**Four axes are reserved.** The pad publishes twelve: the six a gamepad has, two for the
D-pad's hat, and four spares (`ABS_RUDDER`, `ABS_WHEEL` and the two tilts) that rest at centre
and that nothing writes yet. They exist from the beginning because the shape of a uinput device
is fixed when it is created and a game enumerates it once — an axis added later would be
invisible to everything already running. What they are for is a head: three of them can carry
yaw, pitch and roll to an application that can bind an axis but knows nothing about a headset,
which is the shortest path to driving something like Second Life from where the wearer is
looking. **[verified]** that adding them changes nothing a game sees today: SDL still reports
`Xbox 360 Controller 045e/028e` as a game controller, and `jstest` lists the twelve axes.

**A game that does not believe it is focused ignores all of this.** Wine decides whether its
window is in the foreground from `WM_TAKE_FOCUS` and `_NET_WM_STATE_FOCUSED`, and a game that
thinks it is in the background reads no gamepad at all. Both are set by giving the seat the X11
window rather than its Wayland surface — see [x11.md](x11.md), "Keys went nowhere".

---

## 2. Steam has to be told to keep its hands off

Steam takes the controller for Steam Input and zeroes every field Spatiand reads — see
`steam-deck-controller.md` §3. `steam -nojoy` ("Disable controller support", from
steamclient.so's own switch list) is the way out: **[verified]** Steam then holds no hidraw
node, creates no virtual pad of its own, logs no controller activity, and still launches games.

So `crate::controls::without_steam_input` puts `-nojoy` into any launch command whose program is
`steam` — the desktop entries Steam writes for games are `steam steam://rungameid/<appid>`.

---

## 3. The model

Steam Input's, with its names:

| | |
|---|---|
| **Action set** | a whole layout; an action can switch between them |
| **Layer** | overrides some controls while held or toggled on |
| **Binding** | what one button does: a list of activators |
| **Activator** | when it fires — regular, long press, double press, start press, release press, chorded — with optional toggle and turbo |
| **Action** | what it does: a gamepad button, stick direction or trigger, a key, a mouse button or wheel, an action set or layer, or a Spatiand command |
| **Mode** | what an analogue source does: joystick, directional pad, mouse, scroll wheel, flick stick, radial menu, trigger, or gyro |
| **Mode shift** | that source runs in another mode while a button is held |

Sources are both sticks, both triggers, the Deck's gyro and the glasses' gyro. Buttons are the
Deck's, plus the glasses' two temple buttons.

**The trackpads are not sources, and never will be.** They are Spatiand's pointer in every
application, and their clicks are its mouse buttons. This was learned the short way: for one
build a layout could take them, a Steam game's layout did, and opening OpenTTD left the wearer
with no way to point at anything — including the menu that would have undone it. A game that
wants a pointer gets the one the session already has, which is what almost every game with
mouse support actually wants. Resting a thumb on a pad is still bindable, as the natural switch
for gyro aiming, and the face buttons still click *unless* the layout has claimed them.

**A plain press waits when it has to.** If a button also has a long press or a double press, its
regular press fires as a short tap on release. Firing it immediately — the other reading — makes
every long press also a short one.

**STEAM and `⋯` are not bindable.** They open Spatiand's menus in every layout, as they open
Steam's in Game Mode, so no layout can shut the wearer inside a game.

### Templates

*Gamepad*, *Gamepad with gyro aiming*, *Keyboard and mouse*, *Spatial desktop*. A Steam game
starts on *Gamepad*; anything else starts on *Spatial desktop*, which is how Spatiand's desktop
has always behaved — the triggers click the pointer, the buttons type — expressed as a layout. Layouts are stored one file per application in `~/.config/spatiand/layouts`.

---

## 4. The editor

Settings → **Controller layout**, for whatever is in front of the wearer. Laid out as Steam's
configurator is: the controller drawn with every binding labelled around it, and a list —
buttons, then each analogue source with its mode and that mode's settings, then action sets and
layers. D-pad to move, left and right to change a value, A to open, B to go back. Changes apply
at once, so a sensitivity can be felt while it is being set, and are saved when the editor
closes.

The picture is **above** the card rather than beside it: a headset is short of vertical field,
but a card wide enough to read leaves no room beside it at all.

---

## 5. Where the sources come from

**The Deck** — as always, raw hidraw (`steam-deck-controller.md`). Its trackpads go to the
pointer rather than to the layout, and a trigger the layout uses reads as unpulled to the
pointer, so one control never does two things.

**A Bluetooth or USB pad** — evdev, found by rescanning `/proc/bus/input/devices` every two
seconds, so a pad paired mid-session joins by itself. Each is grabbed (`EVIOCGRAB`) while open,
so its presses reach only the layout. Its buttons arrive as the Deck button in the same place,
which is what makes one layout cover either device. Microsoft pads are read by label because
xpad reports X and Y under the codes the kernel's own layout document assigns to the top and
left buttons the other way round.

**The glasses** — the tracker's own gyro samples, bias removed, in the head frame. Their temple
buttons only ever report a press, never a release, so a press is held for 120 ms.

**Rumble** goes the other way: a game uploads a force-feedback effect to the virtual pad and the
kernel hands it to us on the same descriptor. `poll_rumble` answers those requests — uinput
blocks the game until it is answered — and the strengths are sent to the Deck's body motors with
`ID_TRIGGER_RUMBLE_CMD`. That report's layout is from `hid-steam.c` and has **not** been
confirmed by feel. **[inferred]**

---

## 6. Gyro conventions

`Rates` is in the holder's terms: pitch positive aiming up, yaw positive turning left, roll
positive with the right side going down. Each source converts into that.

The Deck's IMU has Z out of the screen (face up reads +1 g), X right and Y towards the top edge,
so pitch is X, yaw is Z and roll is Y — SDL's Steam Deck driver maps it the same way. The signs
come from that driver and have not been checked by turning a Deck here, which is why every gyro
mode has invert switches. **[inferred]**

---

## 7. Looking at it without hardware

```text
SPATIAND_BACKEND=snapshot SPATIAND_SNAPSHOT=/tmp/editor.png SPATIAND_VIEW=controller spatiand
```

`SPATIAND_VIEW_PAGE=<row label>` opens that row first — `Buttons`, `Right trackpad`, `Gyro`.

The card shows six rows at a time and the Deck has twenty-four buttons, so most of the Buttons
page is below the fold — including the four back paddles, which are bindable like everything
else and were taken for missing because nothing said the list went on. Every page longer than
the card now says where you are in it: "7 of 24" in the footer, beside the button hints.

The pad itself can be held up without a session at all, which is how anything outside Spatiand
gets to be asked whether it can see it:

```text
cargo run --release -p spatiand-input --example virtual-pad        # 30s, pressing A once a second
cargo run --release -p spatiand-input --example virtual-pad 120 quiet
```

Then open a game's controller-binding screen, or run `jstest /dev/input/jsN`, and watch.
