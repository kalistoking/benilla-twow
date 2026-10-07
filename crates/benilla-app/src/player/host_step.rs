//! **A host's step back from the target** -- the player's own turn away and short walk, for a test
//! lab with no hand on the keyboard.
//!
//! `.cometome` stands the mob on the character's own spot (0.0 yd), so a shot meant to show both
//! of them side by side shows one body in the other. A player steps back; benilla has no movement
//! Lua (`MoveBackwardStart` and its kin) and a GM `.go` is a teleport, so this does what the
//! player's hands do and nothing more, through the one lever a synthesized input already has:
//!
//! 1. **turn away** -- [`Player::turn_aim`], the host's turn to the target ([`crate::target::HostFaceTarget`]), to the bearing that
//!    points directly away from the target (the facing it already has when there is no bearing, the
//!    mob being on his spot); `stream_self_movement` sends it as `MSG_MOVE_SET_FACING`;
//! 2. **walk** -- [`Player::follow_forward`], the flag `/follow` holds the forward key with
//!    ([`super::follow`]): the controller folds it into its forward axis exactly like a held W, so
//!    the server hears `MSG_MOVE_START_FORWARD`, the heartbeats, and `MSG_MOVE_STOP` -- the run
//!    speed the server granted, the world's collision, no teleport. It is released when the ground
//!    walked would reach the asked yards by the next frame (the nearest frame, not the first one
//!    past), or when the body stops making headway (a wall) or the walk runs out its time;
//! 3. **face again** -- one frame after the key is released (the stop is on the wire), the host's
//!    turn to the target ([`crate::target::HostFaceTarget`], camera and all) is asked, so the shot
//!    that follows is framed as every other.
//!
//! The answer, [`HostSteppedBack`], is given when the walk ends and tells the ground really walked
//! and the distance to the target it ended at -- the server's word is the position, not this
//! module's.

use bevy::prelude::*;

use crate::net::SelfPlayer;
use crate::target::{HostFaceTarget, Selection};

use super::follow::bearing_to;
use super::state::{MoveSpeed, Player};

/// How long the body may fail to make headway before the walk is given up (seconds): a wall, a
/// root, a stun. A step of a few yards takes half a second at run speed.
const STALL_AFTER: f32 = 0.6;

/// A frame's ground covered under this share of what the run speed would give is no headway.
const HEADWAY_SHARE: f32 = 0.2;

/// The walk's time beyond the one its yards need at the granted speed (seconds): the start-up of
/// the first frames, a slope, the speed being cut mid-step -- and the end of a walk that never got
/// anywhere without being plainly stalled.
const TIME_SLACK: f32 = 3.0;

/// The host's standing request: step `yards` straight away from the current target, then face it
/// again. Removed on the frame the walk ends, whatever the answer.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct HostStepBack {
    pub yards: f32,
}

/// What a [`HostStepBack`] did.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub enum HostSteppedBack {
    /// The character walked: `walked` is the ground covered from where he stood (yards), `distance`
    /// the ground distance to the target at the end -- the number a shot's reader wants -- and
    /// `blocked` says the walk was given up short of `asked` (no headway for [`STALL_AFTER`], or
    /// out of time). The host's turn to the target is asked on the same frame; its own answer,
    /// [`crate::target::HostFacedTarget`], follows.
    Stepped {
        guid: u64,
        asked: f32,
        walked: f32,
        distance: f32,
        blocked: bool,
    },
    /// Nothing is selected (or the selection is not streamed): the character did not move.
    NoTarget,
}

/// Where the walk is. Between requests, [`Walk::Idle`].
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Walk {
    #[default]
    Idle,
    /// Turned away on the last frame; the key goes down on this one, so the server hears the turn
    /// before the first step.
    Turned { from: Vec3, guid: u64 },
    /// The forward key is held.
    Walking {
        from: Vec3,
        guid: u64,
        /// Seconds since the key went down.
        elapsed: f32,
        /// Seconds without headway.
        stalled: f32,
        /// Where the body stood on the last frame.
        last: Vec3,
    },
    /// The key was released on the last frame (that frame's controller sent the stop); this one
    /// measures and answers.
    Settling { from: Vec3, guid: u64, blocked: bool },
}

/// The facing that points directly away from `target` as seen from `from` (a horizontal bearing in
/// Bevy space, [`bearing_to`]); `facing` when the two stand on the same spot and there is no away.
pub(super) fn away_facing(from: Vec3, target: Vec3, facing: f32) -> f32 {
    let delta = Vec3::new(from.x - target.x, 0.0, from.z - target.z);
    if delta.length() > f32::EPSILON {
        bearing_to(delta)
    } else {
        facing
    }
}

/// Is the walk done: would the ground walked reach `asked` by the next frame? Half a frame's
/// travel is the rounding, so the walk ends on the frame nearest the asked yards, not always the
/// one past them.
fn walked_enough(walked: f32, asked: f32, speed: f32, dt: f32) -> bool {
    walked + 0.5 * speed * dt >= asked
}

/// The ground distance between two points (yards), heights ignored.
fn ground(a: Vec3, b: Vec3) -> f32 {
    Vec3::new(a.x - b.x, 0.0, a.z - b.z).length()
}

/// Resolve the pending request: turn away, walk, release, answer.
#[allow(clippy::too_many_arguments)]
pub(super) fn step_back(
    mut commands: Commands,
    request: Option<Res<HostStepBack>>,
    mut walk: ResMut<Walk>,
    selection: Res<Selection>,
    targets: Query<&Transform>,
    mut player: ResMut<Player>,
    speed: Res<MoveSpeed>,
    time: Res<Time>,
    self_player: Query<(), With<SelfPlayer>>,
    mut told: MessageWriter<HostSteppedBack>,
) {
    let Some(request) = request else {
        *walk = Walk::Idle;
        return; // nothing asked
    };
    if self_player.single().is_err() {
        return; // not in the world yet -- pending, silent
    }
    let target = selection
        .target
        .zip(selection.guid)
        .and_then(|(e, g)| targets.get(e).ok().map(|t| (g, t.translation)));
    let dt = time.delta_secs();
    let pos = player.pos;
    match *walk {
        Walk::Idle => {
            let Some((guid, at)) = target else {
                debug!("host step back: no target");
                commands.remove_resource::<HostStepBack>();
                told.write(HostSteppedBack::NoTarget);
                return;
            };
            let turn = super::wrap_pi(away_facing(pos, at, player.facing()) - player.facing());
            player.turn_aim(turn);
            *walk = Walk::Turned { from: pos, guid };
        }
        Walk::Turned { from, guid } => {
            player.follow_forward = true;
            *walk = Walk::Walking {
                from,
                guid,
                elapsed: 0.0,
                stalled: 0.0,
                last: pos,
            };
        }
        Walk::Walking {
            from,
            guid,
            elapsed,
            mut stalled,
            last,
        } => {
            let elapsed = elapsed + dt;
            let walked = ground(pos, from);
            if ground(pos, last) < HEADWAY_SHARE * speed.value * dt && elapsed > 0.25 {
                stalled += dt;
            } else {
                stalled = 0.0;
            }
            let timed_out = elapsed > request.yards / speed.value.max(f32::EPSILON) + TIME_SLACK;
            let done = walked_enough(walked, request.yards, speed.value, dt);
            if done || stalled >= STALL_AFTER || timed_out {
                // The key is released here: this frame's controller is the one that stops.
                *walk = Walk::Settling {
                    from,
                    guid,
                    blocked: !done,
                };
            } else {
                player.follow_forward = true;
                *walk = Walk::Walking {
                    from,
                    guid,
                    elapsed,
                    stalled,
                    last: pos,
                };
            }
        }
        Walk::Settling { from, guid, blocked } => {
            let walked = ground(pos, from);
            let distance = target.map_or(walked, |(_, at)| ground(pos, at));
            debug!(
                "host step back: {guid:#x} -> walked {walked:.1} yd of {:.1}, {distance:.1} yd from it{}",
                request.yards,
                if blocked { ", blocked" } else { "" },
            );
            *walk = Walk::Idle;
            commands.remove_resource::<HostStepBack>();
            told.write(HostSteppedBack::Stepped {
                guid,
                asked: request.yards,
                walked,
                distance,
                blocked,
            });
            if target.is_some() {
                commands.insert_resource(HostFaceTarget);
            }
        }
    }
}

/// Register the step back. After the follow's steer (which rewrites the flag every frame) and
/// before the controller that reads it, like the follow itself.
pub(super) fn plugin(app: &mut App) {
    app.init_resource::<Walk>()
        .add_message::<HostSteppedBack>()
        .add_systems(
            Update,
            step_back
                .in_set(benilla_world::schedule::WorldStage::Input)
                .after(super::follow::steer_follow)
                .before(super::control)
                .in_set(crate::char_select::InWorldGated),
        );
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;
    use std::f32::consts::FRAC_PI_2;

    use super::*;

    const GUID: u64 = 0xF130_0000_9500_0007;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// Away from a target due south (+Z in Bevy) is north (-Z), facing yaw 0; away from one due
    /// west (-X) is east, a quarter turn clockwise (yaw -90 deg), as the face tests have it.
    #[test]
    fn away_points_along_the_target_to_the_character() {
        let at = Vec3::new(2.0, 1.0, 5.0);
        assert!(close(away_facing(at + Vec3::new(0.0, 0.0, -4.0), at, 1.0), 0.0));
        assert!(close(away_facing(at + Vec3::new(4.0, 3.0, 0.0), at, 1.0), -FRAC_PI_2));
        // Walking that way moves the body straight away: the controller's forward at that yaw.
        let yaw = away_facing(at + Vec3::new(3.0, 0.0, -4.0), at, 0.0);
        let forward = Quat::from_rotation_y(yaw) * Vec3::NEG_Z;
        assert!(close(forward.x, 0.6) && close(forward.z, -0.8), "{forward:?}");
    }

    /// On the target's own spot (`.cometome`) there is no away: the facing stays and he walks the
    /// way he looks.
    #[test]
    fn on_the_targets_spot_the_facing_stays() {
        let at = Vec3::new(2.0, 1.0, 5.0);
        assert_eq!(away_facing(at, at, 0.7), 0.7);
        assert_eq!(away_facing(at + Vec3::new(0.0, 4.0, 0.0), at, -1.2), -1.2);
    }

    /// The walk ends on the frame nearest the asked yards: half a frame's travel is the rounding.
    #[test]
    fn the_walk_ends_on_the_frame_nearest_the_asked_yards() {
        // 7 yd/s at 60 Hz is 0.117 yd a frame.
        let (speed, dt) = (7.0, 1.0 / 60.0);
        assert!(!walked_enough(2.9, 3.0, speed, dt));
        assert!(walked_enough(2.95, 3.0, speed, dt));
        assert!(walked_enough(3.2, 3.0, speed, dt));
    }

    struct Rig {
        app: App,
        target: Entity,
    }

    impl Rig {
        /// One in the world at the origin facing yaw 0, the target `delta` from him, run speed 7.
        fn new(delta: Vec3) -> Self {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins);
            app.init_resource::<Selection>();
            app.init_resource::<Player>();
            app.init_resource::<Walk>();
            app.insert_resource(MoveSpeed {
                value: 7.0,
                env_override: false,
            });
            app.add_message::<HostSteppedBack>();
            app.world_mut().spawn(SelfPlayer);
            let target = app.world_mut().spawn(Transform::from_translation(delta)).id();
            let mut selection = app.world_mut().resource_mut::<Selection>();
            selection.target = Some(target);
            selection.guid = Some(GUID);
            Rig { app, target }
        }

        fn ask(&mut self, yards: f32) {
            self.app.world_mut().insert_resource(HostStepBack { yards });
        }

        /// One frame of `dt` seconds: the system, then the "controller" -- the body runs along its
        /// facing for the frame when the key is down, at `speed` yd/s, unless `wall` stops it.
        fn frame(&mut self, dt: f32, wall: bool) {
            self.app
                .world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_secs_f32(dt));
            self.app.world_mut().run_system_once(step_back).unwrap();
            self.app.world_mut().flush();
            let mut player = self.app.world_mut().resource_mut::<Player>();
            if player.follow_forward && !wall {
                let forward = Quat::from_rotation_y(player.facing()) * Vec3::NEG_Z;
                player.pos += forward * 7.0 * dt;
            }
            player.follow_forward = false; // `steer_follow`'s rewrite, the next frame's start
        }

        /// Whether the key was down at the end of the last frame's system, read before the
        /// controller's rewrite: run one frame and say it.
        fn told(&mut self) -> Vec<HostSteppedBack> {
            let mut messages = self.app.world_mut().resource_mut::<Messages<HostSteppedBack>>();
            messages.drain().collect()
        }

        fn pending(&self) -> bool {
            self.app.world().contains_resource::<HostStepBack>()
        }

        fn face_asked(&self) -> bool {
            self.app.world().contains_resource::<HostFaceTarget>()
        }
    }

    /// The mob stands on his spot (`.cometome`): he turns nowhere, walks 3 yd the way he looks,
    /// stops within half a frame of it, and is told 3 yd from the target -- then faces it again.
    #[test]
    fn a_step_back_walks_the_asked_yards_and_asks_the_face() {
        let mut rig = Rig::new(Vec3::ZERO);
        rig.ask(3.0);
        let dt = 1.0 / 60.0;
        let mut frames = 0;
        while rig.pending() && frames < 200 {
            rig.frame(dt, false);
            frames += 1;
        }
        let told = rig.told();
        let [HostSteppedBack::Stepped { guid: GUID, asked, walked, distance, blocked: false }] =
            told.as_slice()
        else {
            panic!("{told:?} after {frames} frames");
        };
        assert_eq!(*asked, 3.0);
        assert!((walked - 3.0).abs() < 0.07, "walked {walked}");
        assert!((distance - 3.0).abs() < 0.07, "distance {distance}");
        assert!(rig.face_asked(), "faces the target again");
        assert!(!rig.pending());
        // 3 yd at 7 yd/s is 26 frames; plus the turn, the start and the settle.
        assert!((27..=35).contains(&frames), "{frames} frames");
    }

    /// Standing 2 yd south of the target, away is north: he turns to it first (a frame before any
    /// step), and ends 2 + 3 yd from the target on the line through them.
    #[test]
    fn a_step_back_goes_straight_away_from_where_the_target_stands() {
        let mut rig = Rig::new(Vec3::new(2.0, 0.0, 0.0)); // due east of him: away is west
        rig.ask(3.0);
        rig.frame(1.0 / 60.0, false);
        let facing = rig.app.world().resource::<Player>().facing();
        assert!(close(facing, FRAC_PI_2), "turned away first: {facing}");
        assert_eq!(rig.app.world().resource::<Player>().pos, Vec3::ZERO, "no step yet");
        let mut frames = 0;
        while rig.pending() && frames < 200 {
            rig.frame(1.0 / 60.0, false);
            frames += 1;
        }
        let pos = rig.app.world().resource::<Player>().pos;
        assert!(pos.x < -2.9 && pos.z.abs() < 1e-3, "{pos:?}");
        let told = rig.told();
        let [HostSteppedBack::Stepped { distance, .. }] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!((distance - 5.0).abs() < 0.07, "{distance}");
    }

    /// A wall gives no headway: the walk is given up after the stall time and says blocked, with
    /// the ground it did cover.
    #[test]
    fn a_wall_ends_the_walk_blocked() {
        let mut rig = Rig::new(Vec3::ZERO);
        rig.ask(3.0);
        let mut seconds = 0.0;
        while rig.pending() && seconds < 5.0 {
            rig.frame(1.0 / 60.0, true);
            seconds += 1.0 / 60.0;
        }
        let told = rig.told();
        let [HostSteppedBack::Stepped { walked, blocked: true, .. }] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(*walked < 0.01, "{walked}");
        assert!(seconds > STALL_AFTER && seconds < 1.5, "{seconds}");
        assert!(rig.face_asked(), "he faces the target all the same");
    }

    /// Nothing selected: no turn, no walk, no face; answered at once.
    #[test]
    fn nothing_selected_is_answered_and_no_one_moves() {
        let mut rig = Rig::new(Vec3::ZERO);
        let _ = rig.target;
        rig.app.world_mut().resource_mut::<Selection>().target = None;
        rig.ask(3.0);
        rig.frame(1.0 / 60.0, false);
        assert_eq!(rig.told(), [HostSteppedBack::NoTarget]);
        assert!(!rig.pending() && !rig.face_asked());
        assert_eq!(rig.app.world().resource::<Player>().pos, Vec3::ZERO);
    }
}
