//! Where each of a stream's channels is coming from.
//!
//! ## The model: the room turns with the window
//!
//! An app that sends 5.1 is describing a room — a screen in front of you and speakers around
//! you. It does not know where it is, and it should not have to: it says "front left", and
//! something else decides where front is.
//!
//! Here, **the window decides**. The whole speaker ring stays centred on the listener and is
//! rotated bodily so that its front points at the window. Put the window on your right and
//! the front channels arrive from the right, while the surrounds that were behind you swing
//! round to your left — because that is what would happen if you turned a chair to face a
//! screen beside you. The listener never leaves the middle of the ring; only the ring's
//! heading changes.
//!
//! That single rule covers every case the same way. Stereo is a ring of two. Mono is a ring
//! of one. 7.1.4 is a ring of twelve with four of them overhead. Nothing special-cases the
//! common layout, which is why an unusual one is not a new feature.
//!
//! ## The front stage is as wide as the window
//!
//! A standard layout puts the main pair at ±30°, because that is where a listener puts real
//! speakers. Here there are no real speakers — there is a window, with two edges — so the
//! front stage is **stretched or squeezed to match the window's own angular width**, and the
//! main pair lands on its edges. A large window near your face has a wide image; a small one
//! across the room has a narrow one; and the sound is the size of the thing making it.
//!
//! Only the front stage moves. The surrounds keep their nominal angles, because they describe
//! the room around you rather than anything on screen, and pulling them in with the picture
//! would collapse the room every time a window was made smaller.
//!
//! ## What this module does not decide
//!
//! Directions, and nothing else. How a direction becomes something two ears hear — plain
//! panning, or a head-related transfer function that can put a sound behind you — is a
//! separate question with a separate answer, and it is downstream of every line here. So is
//! whether to bother: a window dead ahead at its natural width produces very nearly the
//! layout the app already assumed, and the closer it is to that the less there is to do.

use glam::{DQuat, DVec3};

/// One channel of a stream, named as the audio server names it.
///
/// The set is what a PipeWire channel map can hold that we can place. Anything outside it
/// (a second LFE, ambisonic components) is not a channel with a direction, and is handled by
/// not being in this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Mono,
    FrontLeft,
    FrontRight,
    FrontCentre,
    /// Low frequency effects. Has no direction, and is the only member that does not.
    Lfe,
    SideLeft,
    SideRight,
    RearLeft,
    RearRight,
    TopFrontLeft,
    TopFrontRight,
    TopRearLeft,
    TopRearRight,
}

/// The channel layouts a stream can arrive in.
///
/// Ordered exactly as the channels appear in the stream, because that is the order the samples
/// are interleaved in and getting it wrong swaps somebody's surrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Mono,
    Stereo,
    /// 3 front, 2 rear, LFE.
    Surround51,
    /// 3 front, 2 side, 2 rear, LFE.
    Surround71,
    /// 5.1 with four height channels. What a Dolby Atmos or DTS:X track most often becomes on
    /// a machine whose renderer is asked for heights but not for side speakers -- and what a
    /// media player is most likely to send here, since the alternative asks the wearer's
    /// headphones to be a twelve-speaker room.
    Surround514,
    /// 7.1 with four height channels — what a decoded Atmos track becomes once something has
    /// turned its objects into speakers.
    Surround714,
}

impl Layout {
    /// The stream's channels, in wire order.
    pub fn channels(self) -> &'static [Channel] {
        use Channel::*;
        match self {
            Layout::Mono => &[Mono],
            Layout::Stereo => &[FrontLeft, FrontRight],
            Layout::Surround51 => &[FrontLeft, FrontRight, FrontCentre, Lfe, RearLeft, RearRight],
            Layout::Surround71 => &[
                FrontLeft,
                FrontRight,
                FrontCentre,
                Lfe,
                RearLeft,
                RearRight,
                SideLeft,
                SideRight,
            ],
            Layout::Surround514 => &[
                FrontLeft,
                FrontRight,
                FrontCentre,
                Lfe,
                RearLeft,
                RearRight,
                TopFrontLeft,
                TopFrontRight,
                TopRearLeft,
                TopRearRight,
            ],
            Layout::Surround714 => &[
                FrontLeft,
                FrontRight,
                FrontCentre,
                Lfe,
                RearLeft,
                RearRight,
                SideLeft,
                SideRight,
                TopFrontLeft,
                TopFrontRight,
                TopRearLeft,
                TopRearRight,
            ],
        }
    }

    /// How many channels a stream in this layout carries.
    pub fn count(self) -> usize {
        self.channels().len()
    }

    /// Read a layout back from the channel count, for a stream that only says how many.
    ///
    /// A guess, and it is the guess the whole industry makes — six channels are 5.1 and eight
    /// are 7.1 often enough that assuming otherwise would be perverse. Where the real channel
    /// map is available it should be used instead; this is for when it is not.
    pub fn from_count(channels: usize) -> Option<Layout> {
        match channels {
            1 => Some(Layout::Mono),
            2 => Some(Layout::Stereo),
            6 => Some(Layout::Surround51),
            8 => Some(Layout::Surround71),
            10 => Some(Layout::Surround514),
            12 => Some(Layout::Surround714),
            _ => None,
        }
    }
}

impl Channel {
    /// Whether this channel belongs to the picture rather than to the room.
    ///
    /// The front stage follows the window's width; everything else keeps the angle a listening
    /// room would have given it. The height fronts count as front — they are above the screen,
    /// not above the sofa — so that a stretched stage stays coherent from top to bottom.
    fn is_front_stage(self) -> bool {
        matches!(
            self,
            Channel::Mono
                | Channel::FrontLeft
                | Channel::FrontRight
                | Channel::FrontCentre
                | Channel::TopFrontLeft
                | Channel::TopFrontRight
        )
    }

    /// Where a listening room would put this speaker, as (azimuth, elevation) in radians.
    ///
    /// Azimuth is measured from straight ahead and is **positive to the left**, matching the
    /// tracker's frame. `None` for the LFE, which is placed by not being placed.
    ///
    /// The angles are the standard ones — ITU-R BS.775 for the horizontal ring, Dolby's
    /// height layout for the top four. `nominal_layout` decides the surrounds: 5.1 puts its
    /// only surround pair at ±110°, while 7.1 has a side pair there and pushes its rears back
    /// to ±150°. The same channel name means a different angle in the two layouts, which is
    /// why this needs to know which layout it is being asked about.
    fn nominal(self, nominal_layout: Layout) -> Option<(f64, f64)> {
        let has_sides = matches!(nominal_layout, Layout::Surround71 | Layout::Surround714);
        let deg = |az: f64, el: f64| Some((az.to_radians(), el.to_radians()));
        match self {
            Channel::Lfe => None,
            Channel::Mono | Channel::FrontCentre => deg(0.0, 0.0),
            Channel::FrontLeft => deg(30.0, 0.0),
            Channel::FrontRight => deg(-30.0, 0.0),
            Channel::SideLeft => deg(90.0, 0.0),
            Channel::SideRight => deg(-90.0, 0.0),
            Channel::RearLeft => deg(if has_sides { 150.0 } else { 110.0 }, 0.0),
            Channel::RearRight => deg(if has_sides { -150.0 } else { -110.0 }, 0.0),
            Channel::TopFrontLeft => deg(45.0, 45.0),
            Channel::TopFrontRight => deg(-45.0, 45.0),
            Channel::TopRearLeft => deg(135.0, 45.0),
            Channel::TopRearRight => deg(-135.0, 45.0),
        }
    }
}

/// The angle a standard front stage puts its main pair at, radians.
///
/// The reference the window's own width is measured against: a window subtending exactly this
/// much either side of its centre gets the layout the app assumed, untouched.
const NOMINAL_HALF_STAGE: f64 = 30.0 * std::f64::consts::PI / 180.0;

/// Where the sound's front is, and how wide.
///
/// Taken from the window rather than being a thing of its own, and deliberately only these
/// three numbers: a window's distance changes how wide it looks, and how wide it looks is
/// already [`Stage::half_width`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stage {
    /// Azimuth of the window's centre in the world frame, radians, positive to the left.
    pub yaw: f64,
    /// Elevation of the window's centre above the horizon, radians.
    pub pitch: f64,
    /// Half the window's angular width seen from the listener, radians.
    ///
    /// From the placement: `atan2(width / 2, radius)`. This is what the front stage is
    /// stretched to, so a window's stereo image is exactly as wide as the window looks.
    pub half_width: f64,
}

impl Stage {
    /// How much the front stage is stretched from its nominal ±30°.
    fn stage_scale(&self) -> f64 {
        self.half_width / NOMINAL_HALF_STAGE
    }

    /// The rotation that swings the whole ring round to face the window.
    ///
    /// The same yaw-then-pitch order the window itself is placed with, so the sound's front
    /// and the window's face point the same way by construction rather than by agreement.
    fn heading(&self) -> DQuat {
        DQuat::from_axis_angle(DVec3::Z, self.yaw) * DQuat::from_axis_angle(DVec3::Y, -self.pitch)
    }
}

/// One channel, and where it is coming from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Speaker {
    /// Which channel of the stream this is. Its index in [`Layout::channels`] is the index of
    /// the samples it belongs to.
    pub channel: Channel,
    /// Unit vector from the listener towards the sound, **in the listener's own frame**:
    /// +X ahead, +Y to the left, +Z up. Turning the head turns these.
    ///
    /// `None` for the LFE. It is not a direction the ear can find, and pretending otherwise
    /// would spend a convolution on making bass arrive from a corner it cannot be heard in.
    pub direction: Option<DVec3>,
}

/// Place a stream's channels around the listener.
///
/// `head` is the head's orientation in the world — the same quaternion the renderer builds its
/// view from. Everything comes back relative to the head, so a window that has not moved but
/// is now behind you reports itself as behind you.
pub fn place(layout: Layout, stage: &Stage, head: DQuat) -> Vec<Speaker> {
    let scale = stage.stage_scale();
    let heading = stage.heading();
    // Undoing the head's own rotation is what makes the world hold still: the ring is built in
    // world terms, and the ears are then asked what that looks like from where they are.
    let into_head = head.inverse();

    layout
        .channels()
        .iter()
        .map(|&channel| {
            let direction = channel.nominal(layout).map(|(az, el)| {
                // The picture's channels are as wide as the picture; the room's stay put.
                let az = if channel.is_front_stage() {
                    az * scale
                } else {
                    az
                };
                let (sa, ca) = az.sin_cos();
                let (se, ce) = el.sin_cos();
                // +X ahead, +Y left, +Z up, matching `Placement::position`.
                let nominal = DVec3::new(ce * ca, ce * sa, se);
                (into_head * (heading * nominal)).normalize()
            });
            Speaker { channel, direction }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window at its default placement: 1.1 m wide at 2.2 m, which is about ±14°.
    fn default_stage() -> Stage {
        Stage {
            yaw: 0.0,
            pitch: 0.0,
            half_width: (1.1f64 / 2.0).atan2(2.2),
        }
    }

    fn looking_ahead() -> DQuat {
        DQuat::IDENTITY
    }

    /// Turn the head to the left by this many degrees.
    fn head_turned(deg: f64) -> DQuat {
        DQuat::from_axis_angle(DVec3::Z, deg.to_radians())
    }

    fn find(speakers: &[Speaker], channel: Channel) -> DVec3 {
        speakers
            .iter()
            .find(|s| s.channel == channel)
            .unwrap_or_else(|| panic!("no {channel:?} in the layout"))
            .direction
            .unwrap_or_else(|| panic!("{channel:?} has no direction"))
    }

    /// Azimuth of a direction in degrees, positive to the left.
    fn azimuth_deg(v: DVec3) -> f64 {
        v.y.atan2(v.x).to_degrees()
    }

    #[test]
    fn the_channel_order_is_the_order_the_samples_arrive_in() {
        // The index into this list is the index into an interleaved frame. Swapping two
        // entries is inaudible on a test tone and unmistakable on a film.
        assert_eq!(
            Layout::Surround51.channels(),
            &[
                Channel::FrontLeft,
                Channel::FrontRight,
                Channel::FrontCentre,
                Channel::Lfe,
                Channel::RearLeft,
                Channel::RearRight
            ]
        );
        for layout in [
            Layout::Mono,
            Layout::Stereo,
            Layout::Surround51,
            Layout::Surround71,
            Layout::Surround514,
            Layout::Surround714,
        ] {
            assert_eq!(layout.count(), layout.channels().len());
            assert_eq!(Layout::from_count(layout.count()), Some(layout));
        }
    }

    #[test]
    fn a_window_in_front_puts_its_speakers_on_its_own_edges() {
        // The promise that keeps ordinary stereo sounding ordinary: the left channel comes
        // from the left side of the picture, not from a notional speaker beside the sofa.
        let stage = default_stage();
        let out = place(Layout::Stereo, &stage, looking_ahead());
        let edge = stage.half_width.to_degrees();
        assert!((azimuth_deg(find(&out, Channel::FrontLeft)) - edge).abs() < 1e-6);
        assert!((azimuth_deg(find(&out, Channel::FrontRight)) + edge).abs() < 1e-6);
    }

    #[test]
    fn a_wider_window_has_a_wider_image() {
        // And a smaller one a narrower image. The sound is the size of the thing making it.
        let narrow = Stage {
            half_width: 8f64.to_radians(),
            ..default_stage()
        };
        let wide = Stage {
            half_width: 40f64.to_radians(),
            ..default_stage()
        };
        let n = azimuth_deg(find(
            &place(Layout::Stereo, &narrow, looking_ahead()),
            Channel::FrontLeft,
        ));
        let w = azimuth_deg(find(
            &place(Layout::Stereo, &wide, looking_ahead()),
            Channel::FrontLeft,
        ));
        assert!((n - 8.0).abs() < 1e-6, "narrow image was {n}");
        assert!((w - 40.0).abs() < 1e-6, "wide image was {w}");
    }

    #[test]
    fn a_window_at_the_nominal_width_is_left_exactly_as_the_app_meant_it() {
        // A stage that happens to be 30 degrees wide is the layout the app already assumed, so
        // nothing here should touch it. This is what makes "do nothing" a reachable state
        // rather than a special case bolted on.
        let stage = Stage {
            half_width: NOMINAL_HALF_STAGE,
            ..default_stage()
        };
        let out = place(Layout::Surround51, &stage, looking_ahead());
        for (channel, expected) in [
            (Channel::FrontLeft, 30.0),
            (Channel::FrontRight, -30.0),
            (Channel::FrontCentre, 0.0),
            (Channel::RearLeft, 110.0),
            (Channel::RearRight, -110.0),
        ] {
            let got = azimuth_deg(find(&out, channel));
            assert!(
                (got - expected).abs() < 1e-6,
                "{channel:?} at {got}, wanted {expected}"
            );
        }
    }

    #[test]
    fn a_window_on_the_right_sends_its_front_right_and_its_rear_left() {
        // The case that says the whole model is right, and the one worth reading the test
        // names for: turn to face a screen beside you and the room turns with you, so what
        // was behind is now to the other side.
        let stage = Stage {
            yaw: -90f64.to_radians(),
            ..default_stage()
        };
        let out = place(Layout::Surround51, &stage, looking_ahead());
        for front in [
            Channel::FrontLeft,
            Channel::FrontRight,
            Channel::FrontCentre,
        ] {
            assert!(
                find(&out, front).y < 0.0,
                "{front:?} did not come from the right"
            );
        }
        for rear in [Channel::RearLeft, Channel::RearRight] {
            assert!(
                find(&out, rear).y > 0.0,
                "{rear:?} did not come from the left"
            );
        }
    }

    #[test]
    fn the_surrounds_stay_around_the_listener_when_the_picture_shrinks() {
        // Only the front stage follows the window. If the surrounds came in with it, making a
        // window small would collapse the room to a point, and a film would lose its room
        // every time you pushed the picture away.
        let small = Stage {
            half_width: 5f64.to_radians(),
            ..default_stage()
        };
        let out = place(Layout::Surround51, &small, looking_ahead());
        assert!((azimuth_deg(find(&out, Channel::RearLeft)) - 110.0).abs() < 1e-6);
        assert!((azimuth_deg(find(&out, Channel::FrontLeft)) - 5.0).abs() < 1e-6);
    }

    #[test]
    fn turning_your_head_leaves_the_sound_where_the_window_is() {
        // Head 30 degrees to the left, window straight ahead in the world: the window is now
        // 30 degrees to your right, and so is its sound. This is the difference between sound
        // that is in the room and sound that is stuck to your face.
        let out = place(Layout::Mono, &default_stage(), head_turned(30.0));
        let az = azimuth_deg(find(&out, Channel::Mono));
        assert!(
            (az + 30.0).abs() < 1e-6,
            "the sound followed the head to {az}"
        );
    }

    #[test]
    fn a_window_you_have_turned_your_back_on_is_behind_you() {
        let out = place(Layout::Mono, &default_stage(), head_turned(180.0));
        assert!(find(&out, Channel::Mono).x < -0.999);
    }

    #[test]
    fn the_ring_turns_without_changing_shape() {
        // A rigid rotation, which is the claim the model rests on: the angles between one
        // channel and the next are a property of the app's mix, and nothing here may alter
        // them just because the window moved.
        let ahead = place(Layout::Surround71, &default_stage(), looking_ahead());
        let aside = place(
            Layout::Surround71,
            &Stage {
                yaw: -1.1,
                pitch: 0.4,
                ..default_stage()
            },
            head_turned(20.0),
        );
        for (a, b) in ahead
            .iter()
            .zip(aside.iter())
            .filter(|(a, _)| a.direction.is_some())
        {
            for (c, d) in ahead
                .iter()
                .zip(aside.iter())
                .filter(|(c, _)| c.direction.is_some())
            {
                let before = a.direction.unwrap().dot(c.direction.unwrap());
                let after = b.direction.unwrap().dot(d.direction.unwrap());
                assert!(
                    (before - after).abs() < 1e-9,
                    "{:?}/{:?} changed angle: {before} -> {after}",
                    a.channel,
                    c.channel
                );
            }
        }
    }

    #[test]
    fn the_bass_is_not_given_somewhere_to_come_from() {
        // The LFE is the one channel with no direction to have. Placing it would cost a
        // convolution to produce an effect the ear cannot hear anyway.
        let out = place(Layout::Surround51, &default_stage(), looking_ahead());
        let lfe = out
            .iter()
            .find(|s| s.channel == Channel::Lfe)
            .expect("5.1 has an LFE");
        assert!(lfe.direction.is_none());
        assert_eq!(out.iter().filter(|s| s.direction.is_none()).count(), 1);
    }

    #[test]
    fn the_height_channels_are_overhead_and_stay_overhead() {
        let out = place(Layout::Surround714, &default_stage(), looking_ahead());
        for top in [
            Channel::TopFrontLeft,
            Channel::TopFrontRight,
            Channel::TopRearLeft,
            Channel::TopRearRight,
        ] {
            assert!(
                find(&out, top).z > 0.5,
                "{top:?} was not above the listener"
            );
        }
        // ...and the rears above are still behind.
        assert!(find(&out, Channel::TopRearLeft).x < 0.0);
        assert!(find(&out, Channel::TopFrontLeft).x > 0.0);
    }

    #[test]
    fn seven_one_pushes_its_rears_back_to_make_room_for_the_sides() {
        // The same channel name means a different angle in 5.1 and 7.1. Reusing the 5.1 angle
        // would sit the rear pair on top of the side pair and waste both.
        let stage = default_stage();
        let five = place(Layout::Surround51, &stage, looking_ahead());
        let seven = place(Layout::Surround71, &stage, looking_ahead());
        assert!((azimuth_deg(find(&five, Channel::RearLeft)) - 110.0).abs() < 1e-6);
        assert!((azimuth_deg(find(&seven, Channel::RearLeft)) - 150.0).abs() < 1e-6);
        assert!((azimuth_deg(find(&seven, Channel::SideLeft)) - 90.0).abs() < 1e-6);
    }

    #[test]
    fn every_direction_is_a_unit_vector() {
        // Downstream, a direction is looked up in a table of measurements taken on a sphere.
        // One that is not on the sphere is a silent lookup of the wrong thing.
        for layout in [
            Layout::Mono,
            Layout::Stereo,
            Layout::Surround51,
            Layout::Surround714,
        ] {
            let stage = Stage {
                yaw: 0.7,
                pitch: -0.3,
                half_width: 0.9,
            };
            for speaker in place(layout, &stage, head_turned(-40.0)) {
                if let Some(d) = speaker.direction {
                    assert!(
                        (d.length() - 1.0).abs() < 1e-9,
                        "{:?} was {}",
                        speaker.channel,
                        d.length()
                    );
                }
            }
        }
    }
}
