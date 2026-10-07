//! **A host's step back from the target** -- the player's own turn away and short walk, for a test
//! lab with no hand on the keyboard.
//!
//! `.cometome` stands the mob on the character's own spot (0.0 yd), so a shot meant to show both
//! of them side by side shows one body in the other. A player steps back; benilla has no movement
//! Lua (`MoveBackwardStart` and its kin) and a GM `.go` is a teleport, so this does what the
//! player's hands do and nothing more, through the one lever a synthesized input already has:
//!
//! 1. **turn away** -- [`Player::turn_aim`], the scripted mouse-turn's lever, by the angle to the
//!    bearing that points directly away from the target (the facing it already has when there is
//!    no bearing, the mob being on his spot); `stream_self_movement` sends it as
//!    `MSG_MOVE_SET_FACING`;
//! 2. **walk** -- [`Player::follow_forward`], the flag `/follow` holds the forward key with
//!    ([`super::follow`]): the controller folds it into its forward axis exactly like a held W, so
//!    the server hears `MSG_MOVE_START_FORWARD`, the heartbeats, and `MSG_MOVE_STOP` -- the speed
//!    the server granted, the world's collision, no teleport. It is released when the ground
//!    walked would reach the asked yards by the next frame (the nearest frame, not the first one
//!    past), or when the body stops making headway (a wall) or the walk runs out its time;
//! 3. **face again** -- one frame after the key is released (the stop is on the wire), the host's
//!    turn to the target ([`crate::target::HostFaceTarget`], camera and all) is asked, so the shot
//!    that follows is framed as every other.
//!
//! Every yard and second of the walk is measured against the speed the controller really moves
//! the body at ([`forward_speed`]: the driven body's granted [`UnitSpeeds`] through the
//! controller's own cascade, walk mode included) -- not the run-speed fallback, which a snare, a
//! walk or a mount leaves behind.
//!
//! The answer, [`HostSteppedBack`], is given when the walk ends and tells the ground really walked
//! and the distance to the target it ended at -- the server's word is the position, not this
//! module's. A request that cannot be walked (yards not a positive number, a speed of nothing) is
//! answered at once as blocked, with no turn and no key, so the host is never left waiting.

use benilla_protocol::MoveSpeeds;
use bevy::prelude::*;

use crate::creature_anim::move_flags;
use crate::net::{Embodied, SelfPlayer, UnitSpeeds};
use crate::target::{HostFaceTarget, Selection};

use super::camera::FlyCam;
use super::follow::bearing_to;
use super::state::{MoveSpeed, Player};

/// How long the body may fail to make headway before the walk is given up (seconds): a wall, a
/// root, a stun. A step of a few yards takes half a second at run speed.
const STALL_AFTER: f32 = 0.6;

/// A frame's ground covered under this share of what the body's speed would give is no headway.
const HEADWAY_SHARE: f32 = 0.2;

/// The walk's time beyond the one its yards need at the body's speed (seconds): the start-up of
/// the first frames, a slope, the speed being cut mid-step -- and the end of a walk that never got
/// anywhere without being plainly stalled.
const TIME_SLACK: f32 = 3.0;

/// The longest step walked (yards): a longer ask is walked this far. The test lab's card refuses
/// more than 20; this is the client's own bound on a key held for a host.
const MAX_STEP: f32 = 40.0;

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
    /// the ground distance to the target at the end -- the number a shot's reader wants, `None`
    /// when the target walked from is no longer the one selected (despawned, deselected, another
    /// selected) -- and `blocked` says the walk was given up short of `asked` (no headway for
    /// [`STALL_AFTER`], out of time, or the body's speed fell to nothing). `asked` is the yards the
    /// walk aimed at (at most [`MAX_STEP`]). The host's turn to the target is asked on the same
    /// frame; its own answer, [`crate::target::HostFacedTarget`], follows (`NoTarget` when the
    /// target is gone).
    ///
    /// A request that cannot be walked -- `yards` not finite or not above 0, or a body whose speed
    /// is nothing (`$WOW_MOVE_SPEED=0`, a server speed of 0) -- is answered on its first frame as
    /// `blocked` with `walked` 0 and `asked` the yards as asked: no turn, no key.
    Stepped {
        guid: u64,
        asked: f32,
        walked: f32,
        distance: Option<f32>,
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

/// The yards a request may be walked: `None` for yards that are not finite or not above 0 (no
/// walk ends on them), else at most [`MAX_STEP`].
fn step_yards(yards: f32) -> Option<f32> {
    (yards.is_finite() && yards > 0.0).then(|| yards.min(MAX_STEP))
}

/// The yards a second the controller moves the body forward on the held key this frame -- the
/// speed set it moves by ([`MoveSpeed::speeds`]: the driven body's granted [`UnitSpeeds`], or the
/// fallback and `$WOW_MOVE_SPEED`'s synthetic set) through the same cascade
/// ([`crate::net::current_speed`]), so a walk-mode body walks at the walk speed and a snare is
/// the snared run; in water, the swim arm's own pair ([`super::swim`]). `None` when it is not a
/// positive, finite number: a body that moves by nothing.
fn forward_speed(
    granted: Option<MoveSpeeds>,
    fallback: &MoveSpeed,
    walking: bool,
    swimming: bool,
) -> Option<f32> {
    let speed = if swimming {
        match granted {
            Some(s) if !fallback.env_override => s.swim,
            _ => super::swim::SWIM_SPEED,
        }
    } else {
        let gait = if walking { move_flags::WALK_MODE } else { 0 };
        crate::net::current_speed(&fallback.speeds(granted), move_flags::FORWARD | gait)
    };
    (speed.is_finite() && speed > 0.0).then_some(speed)
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
    move_speed: Res<MoveSpeed>,
    body: Query<Option<&UnitSpeeds>, (With<Embodied>, Without<FlyCam>)>,
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
    let granted = body.single().ok().flatten().map(|s| s.0);
    let speed = forward_speed(granted, &move_speed, player.walking, player.swimming);
    let yards = step_yards(request.yards);
    match *walk {
        Walk::Idle => {
            let Some((guid, at)) = target else {
                debug!("host step back: no target");
                commands.remove_resource::<HostStepBack>();
                told.write(HostSteppedBack::NoTarget);
                return;
            };
            if yards.is_none() || speed.is_none() {
                // Nothing to walk, or nothing to walk it with: answered now, no turn, no key.
                debug!(
                    "host step back: {guid:#x} -> not walked ({} yd asked, speed {speed:?})",
                    request.yards,
                );
                commands.remove_resource::<HostStepBack>();
                told.write(HostSteppedBack::Stepped {
                    guid,
                    asked: request.yards,
                    walked: 0.0,
                    distance: Some(ground(pos, at)),
                    blocked: true,
                });
                commands.insert_resource(HostFaceTarget);
                return;
            }
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
            let asked = yards.unwrap_or(0.0);
            // The speed fell to nothing mid-walk (or the ask became unwalkable): the key comes up
            // on this frame, blocked -- no headway can be measured against nothing.
            let (done, given_up) = match speed {
                Some(speed) if yards.is_some() => {
                    if ground(pos, last) < HEADWAY_SHARE * speed * dt && elapsed > 0.25 {
                        stalled += dt;
                    } else {
                        stalled = 0.0;
                    }
                    let timed_out = elapsed > asked / speed + TIME_SLACK;
                    (
                        walked_enough(walked, asked, speed, dt),
                        stalled >= STALL_AFTER || timed_out,
                    )
                }
                _ => (false, true),
            };
            if done || given_up {
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
            let asked = yards.unwrap_or(request.yards);
            // The target walked from, and only it: a despawn or another selection is no distance.
            let distance = target
                .filter(|(g, _)| *g == guid)
                .map(|(_, at)| ground(pos, at));
            debug!(
                "host step back: {guid:#x} -> walked {walked:.1} yd of {asked:.1}, {}{}",
                distance.map_or_else(
                    || "the target gone".to_owned(),
                    |d| format!("{d:.1} yd from it")
                ),
                if blocked { ", blocked" } else { "" },
            );
            *walk = Walk::Idle;
            commands.remove_resource::<HostStepBack>();
            told.write(HostSteppedBack::Stepped {
                guid,
                asked,
                walked,
                distance,
                blocked,
            });
            // Asked whatever the selection: the host waits for the face's answer after a step, and
            // with the target gone that answer is its `NoTarget`.
            commands.insert_resource(HostFaceTarget);
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

    /// The vanilla speed set (vmangos `baseMoveSpeed`): walk 2.5, run 7.0, run-back 4.5.
    fn vanilla() -> MoveSpeeds {
        MoveSpeeds {
            walk: 2.5,
            run: 7.0,
            run_back: 4.5,
            swim: 4.722,
            swim_back: 2.5,
            turn_rate: std::f32::consts::PI,
        }
    }

    /// The speed a step is measured against is the body's own: the granted set through the
    /// controller's cascade, the walk speed in walk mode, the swim speed in water -- the fallback
    /// only before the create lands or under `$WOW_MOVE_SPEED`, and nothing is no speed.
    #[test]
    fn the_step_reads_the_speed_the_controller_moves_by() {
        let fallback = |value, env_override| MoveSpeed {
            value,
            env_override,
        };
        let snared = MoveSpeeds {
            run: 1.0,
            ..vanilla()
        };
        let speed = |granted, fallback: MoveSpeed, walking, swimming| {
            forward_speed(granted, &fallback, walking, swimming)
        };
        assert_eq!(speed(Some(snared), fallback(7.0, false), false, false), Some(1.0));
        assert_eq!(speed(Some(vanilla()), fallback(7.0, false), true, false), Some(2.5));
        assert_eq!(speed(Some(vanilla()), fallback(7.0, false), false, true), Some(4.722));
        assert_eq!(speed(None, fallback(7.0, false), false, false), Some(7.0));
        // `$WOW_MOVE_SPEED=14`: its synthetic set, walk mode at its walk ratio.
        assert_eq!(speed(Some(vanilla()), fallback(14.0, true), false, false), Some(14.0));
        assert_eq!(speed(Some(vanilla()), fallback(14.0, true), true, false), Some(5.0));
        // `$WOW_MOVE_SPEED=0`, a granted run of 0, a NaN: no speed.
        assert_eq!(speed(Some(vanilla()), fallback(0.0, true), false, false), None);
        let stopped = MoveSpeeds {
            run: 0.0,
            ..vanilla()
        };
        assert_eq!(speed(Some(stopped), fallback(7.0, false), false, false), None);
        let broken = MoveSpeeds {
            run: f32::NAN,
            ..vanilla()
        };
        assert_eq!(speed(Some(broken), fallback(7.0, false), false, false), None);
    }

    /// Yards that are not a positive number are no walk; past [`MAX_STEP`] they are walked that
    /// far.
    #[test]
    fn the_yards_must_be_positive_and_are_held_to_the_longest_step() {
        assert_eq!(step_yards(3.0), Some(3.0));
        assert_eq!(step_yards(100.0), Some(MAX_STEP));
        for yards in [0.0, -3.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(step_yards(yards), None, "{yards}");
        }
    }

    struct Rig {
        app: App,
        target: Entity,
        /// The yards a second the rig's "controller" moves the body on the held key: the speed
        /// the test says the real controller would give.
        pace: f32,
        /// Was the key ever down at the end of a frame's system?
        keyed: bool,
    }

    impl Rig {
        /// One in the world at the origin facing yaw 0, the target `delta` from him, the vanilla
        /// speed set granted (run 7).
        fn new(delta: Vec3) -> Self {
            Self::granted(delta, vanilla(), 7.0)
        }

        /// As [`Self::new`], with `speeds` granted on the driven body and the body moving at
        /// `pace`; the run-speed fallback stays the vanilla 7.0, so a step that read it would walk
        /// by the wrong number.
        fn granted(delta: Vec3, speeds: MoveSpeeds, pace: f32) -> Self {
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
            app.world_mut()
                .spawn((SelfPlayer, Embodied, UnitSpeeds(speeds)));
            let target = app.world_mut().spawn(Transform::from_translation(delta)).id();
            let mut selection = app.world_mut().resource_mut::<Selection>();
            selection.target = Some(target);
            selection.guid = Some(GUID);
            Rig {
                app,
                target,
                pace,
                keyed: false,
            }
        }

        fn ask(&mut self, yards: f32) {
            self.app.world_mut().insert_resource(HostStepBack { yards });
        }

        /// One frame of `dt` seconds: the system, then the "controller" -- the body runs along its
        /// facing for the frame when the key is down, at [`Self::pace`], unless `wall` stops it.
        fn frame(&mut self, dt: f32, wall: bool) {
            self.app
                .world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_secs_f32(dt));
            self.app.world_mut().run_system_once(step_back).unwrap();
            self.app.world_mut().flush();
            let pace = self.pace;
            let mut player = self.app.world_mut().resource_mut::<Player>();
            self.keyed |= player.follow_forward;
            if player.follow_forward && !wall {
                let forward = Quat::from_rotation_y(player.facing()) * Vec3::NEG_Z;
                player.pos += forward * pace * dt;
            }
            player.follow_forward = false; // `steer_follow`'s rewrite, the next frame's start
        }

        /// Frames of 1/60 s until the request is answered, at most `seconds` of them: how many
        /// ran.
        fn walk_out(&mut self, seconds: f32) -> u32 {
            let mut frames = 0;
            while self.pending() && (frames as f32) < seconds * 60.0 {
                self.frame(1.0 / 60.0, false);
                frames += 1;
            }
            frames
        }

        /// Grant `speeds` on the driven body from the next frame on (a snare landing, a server
        /// speed of 0).
        fn grant(&mut self, speeds: MoveSpeeds) {
            let world = self.app.world_mut();
            let body = world
                .query_filtered::<Entity, With<Embodied>>()
                .single(world)
                .unwrap();
            world.entity_mut(body).insert(UnitSpeeds(speeds));
        }

        /// The answers written since the last call (drained).
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
        assert!(matches!(distance, Some(d) if (d - 3.0).abs() < 0.07), "distance {distance:?}");
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
        let [HostSteppedBack::Stepped { distance: Some(distance), .. }] = told.as_slice() else {
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
        rig.app.world_mut().resource_mut::<Selection>().target = None;
        rig.ask(3.0);
        rig.frame(1.0 / 60.0, false);
        assert_eq!(rig.told(), [HostSteppedBack::NoTarget]);
        assert!(!rig.pending() && !rig.face_asked());
        assert_eq!(rig.app.world().resource::<Player>().pos, Vec3::ZERO);
    }

    /// A heavy snare (run 1.0 yd/s, granted) makes headway at its own pace: no stall, no time-out,
    /// the 3 yd walked in about three seconds -- the stall measured against the 7.0 fallback gave
    /// it up after 0.85 s.
    #[test]
    fn a_snared_step_arrives_and_is_not_stalled() {
        let snared = MoveSpeeds {
            run: 1.0,
            ..vanilla()
        };
        let mut rig = Rig::granted(Vec3::ZERO, snared, 1.0);
        rig.ask(3.0);
        let frames = rig.walk_out(10.0);
        let told = rig.told();
        let [HostSteppedBack::Stepped { walked, blocked: false, .. }] = told.as_slice() else {
            panic!("{told:?} after {frames} frames");
        };
        assert!((walked - 3.0).abs() < 0.01, "walked {walked}");
        assert!((180..=185).contains(&frames), "{frames} frames");
    }

    /// In walk mode the body walks at the walk speed (2.5 yd/s), and a 20 yd step takes its eight
    /// seconds without running out of time -- the time measured from the 7.0 fallback allowed 5.9.
    #[test]
    fn a_long_step_in_walk_mode_does_not_run_out_of_time() {
        let mut rig = Rig::granted(Vec3::ZERO, vanilla(), 2.5);
        rig.app.world_mut().resource_mut::<Player>().walking = true;
        rig.ask(20.0);
        let frames = rig.walk_out(15.0);
        let told = rig.told();
        let [HostSteppedBack::Stepped { walked, distance: Some(distance), blocked: false, .. }] =
            told.as_slice()
        else {
            panic!("{told:?} after {frames} frames");
        };
        assert!((walked - 20.0).abs() < 0.03, "walked {walked}");
        assert!((distance - 20.0).abs() < 0.03, "distance {distance}");
        assert!(frames > 8 * 60, "{frames} frames");
    }

    /// A speed of nothing (`$WOW_MOVE_SPEED=0`) is answered on the first frame as blocked: no
    /// turn, no key, and the face asked so the host's wait for it ends.
    #[test]
    fn a_speed_of_nothing_ends_the_step_at_once() {
        let mut rig = Rig::new(Vec3::new(2.0, 0.0, 0.0));
        *rig.app.world_mut().resource_mut::<MoveSpeed>() = MoveSpeed {
            value: 0.0,
            env_override: true,
        };
        rig.ask(3.0);
        rig.frame(1.0 / 60.0, false);
        let told = rig.told();
        let [HostSteppedBack::Stepped {
            asked,
            walked,
            distance: Some(distance),
            blocked: true,
            ..
        }] = told.as_slice()
        else {
            panic!("{told:?}");
        };
        assert_eq!((*asked, *walked), (3.0, 0.0));
        assert!(close(*distance, 2.0), "{distance}");
        assert!(!rig.pending() && rig.face_asked() && !rig.keyed);
        assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "no turn");
    }

    /// A speed that falls to nothing mid-walk (a server speed of 0) releases the key on the next
    /// frame and answers blocked on the one after.
    #[test]
    fn a_speed_gone_mid_walk_releases_the_key() {
        let mut rig = Rig::new(Vec3::ZERO);
        rig.ask(3.0);
        for _ in 0..5 {
            rig.frame(1.0 / 60.0, false);
        }
        assert!(rig.keyed && rig.pending());
        rig.grant(MoveSpeeds {
            run: 0.0,
            ..vanilla()
        });
        rig.pace = 0.0;
        rig.keyed = false;
        rig.frame(1.0 / 60.0, false);
        assert!(!rig.keyed, "the key is up on the frame the speed is gone");
        rig.frame(1.0 / 60.0, false);
        let told = rig.told();
        assert!(
            matches!(told.as_slice(), [HostSteppedBack::Stepped { blocked: true, .. }]),
            "{told:?}"
        );
        assert!(!rig.pending() && rig.face_asked());
    }

    /// Yards that are not a positive number are refused on the first frame: answered blocked with
    /// the yards as asked, the face asked, no turn and no key -- never a key held to a wall.
    #[test]
    fn yards_not_a_positive_number_are_refused_and_answered() {
        for yards in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -3.0] {
            let mut rig = Rig::new(Vec3::new(2.0, 0.0, 0.0));
            rig.ask(yards);
            rig.frame(1.0 / 60.0, false);
            let told = rig.told();
            let [HostSteppedBack::Stepped {
                asked,
                walked,
                blocked: true,
                ..
            }] = told.as_slice()
            else {
                panic!("{yards}: {told:?}");
            };
            assert_eq!(asked.to_bits(), yards.to_bits(), "{yards}: asked {asked}");
            assert_eq!(*walked, 0.0, "{yards}");
            assert!(!rig.pending() && rig.face_asked() && !rig.keyed, "{yards}");
            assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "{yards}: no turn");
        }
    }

    /// A step past [`MAX_STEP`] is walked that far, and says so.
    #[test]
    fn a_step_past_the_longest_is_walked_the_longest() {
        let mut rig = Rig::new(Vec3::ZERO);
        rig.ask(100.0);
        let frames = rig.walk_out(20.0);
        let told = rig.told();
        let [HostSteppedBack::Stepped { asked, walked, blocked: false, .. }] = told.as_slice()
        else {
            panic!("{told:?} after {frames} frames");
        };
        assert_eq!(*asked, MAX_STEP);
        assert!((walked - MAX_STEP).abs() < 0.07, "walked {walked}");
    }

    /// The target despawned mid-walk: the step is told with no distance (not the ground walked
    /// passed off as one), and the face is asked all the same -- its own answer says no target.
    #[test]
    fn a_target_gone_mid_walk_is_told_as_no_distance() {
        let mut rig = Rig::new(Vec3::ZERO);
        rig.ask(3.0);
        for _ in 0..5 {
            rig.frame(1.0 / 60.0, false);
        }
        let target = rig.target;
        rig.app.world_mut().despawn(target);
        rig.walk_out(5.0);
        let told = rig.told();
        let [HostSteppedBack::Stepped {
            walked,
            distance: None,
            blocked: false,
            ..
        }] = told.as_slice()
        else {
            panic!("{told:?}");
        };
        assert!((walked - 3.0).abs() < 0.07, "walked {walked}");
        assert!(rig.face_asked());
    }
}
