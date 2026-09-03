Spatiand — installing
=====================

A 3D spatial desktop for a Steam Deck driving XREAL Air glasses.

What you need
-------------

This has only ever been run on one combination of hardware. It may well work on
others; nobody has tried.

  * Steam Deck (LCD or OLED — tested on an LCD, AMD Custom APU 0405)
  * XREAL Air, first generation (USB 3318:0424). Air 2 uses the same protocol
    on the same interfaces and is likely to work; it has not been tested.
  * SteamOS 3.8.16 or thereabouts
  * A USB-C cable that carries video. Many do not.

Installing
----------

  1. Unpack the folder anywhere.
  2. Double-click "Install Spatiand".
     KDE will ask whether you trust it the first time — say yes.
  3. Type your password when asked. It is needed once, to add "Spatial Mode"
     to the list of sessions you can log into.

Nothing is installed outside your home folder except that one session entry.

Running it
----------

Plug the glasses in *before* starting, then either double-click "Go To
Spatiand" on the desktop, or log out and choose "Spatial Mode".

To leave: hold any button on the panel's exit row. From a terminal:

    steamosctl switch-to-desktop-mode plasma.desktop

If something goes wrong
-----------------------

The session writes everything it did to:

    ~/.local/share/spatiand-session.log        this session
    ~/.local/share/spatiand-session.log.1      the one before

The second one is where a crash will be, because a session that dies is
restarted within seconds and the new one opens the first file fresh.

**A black screen in the glasses** with everything else working is usually one of
three things, in the order worth checking:

  * The battery is low. Below about 15% the Deck stops giving the glasses
    enough power for their panel while USB and video keep working.
  * The cable. Reseat it, then flip the plug, then try another cable.
  * The port has stopped offering video. `cat /sys/class/drm/card0-DP-1/status`
    says `disconnected` and `.../modes` is empty. Only a full shutdown clears
    this — not a restart.

**Nothing starts at all.** SteamOS updates replace `/usr` and take the session
registration with them. Run the installer again; everything else survives.

Turning things off
------------------

`~/.config/spatiand/prefs.toml`, written on first run:

    keyboard_click            the on-screen keyboard's click
    spatial_audio             whether each window's sound is placed
    audio_directness_centred  how much unplaced sound to keep, 0 to 1
    audio_directness_off_axis the same, for windows off to one side

`~/.config/spatiand/session.env` is read into the session's environment if it
exists. `RUST_LOG=debug` there is the way to get more out of the log on a
machine whose only screen is the thing being debugged.
