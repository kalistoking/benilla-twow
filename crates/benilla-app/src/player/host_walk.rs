//! **A host's walk** -- the player's own forward key held for a host, straight ahead or toward a
//! point, with no target needed (the step back's generalisation, [`super::host_step`]).
//!
//! A test lab that wants to know whether the world's collision stops a body (a closed door, a
//! field of force) has to walk into it the way a player does: benilla has no movement Lua and no
//! click-to-move, and a GM `.go` is a teleport that no collider sees. This does what the player's
//! hands do and nothing more:
//!
//! 1. **turn** -- toward a point ([`WalkAim::Point`]: [`Player::turn_aim`] by the angle to the
//!    bearing, [`super::follow::bearing_to`]), or not at all ([`WalkAim::Facing`]: the way he
//!    looks); `stream_self_movement` sends it as `MSG_MOVE_SET_FACING`;
//! 2. **walk** -- [`Player::follow_forward`], the flag `/follow` holds the forward key with: the
//!    server hears `MSG_MOVE_START_FORWARD`, the heartbeats and `MSG_MOVE_STOP`, at the speed it
//!    granted, through the world's collision. It is released when the ground walked would reach the
//!    asked yards by the next frame, or when the body stops making headway (a wall, a closed door,
//!    a root) for [`STALL_AFTER`], or when the walk runs out its time;
//! 3. **answer** -- one frame after the release (the stop is on the wire), [`HostWalked`] says the
//!    ground really walked and where the body ended, in the server's own coordinates (what `.gps`
//!    prints), so a log line can be set beside the GM's reading.
//!
//! Speed and stall are measured as the step back measures them ([`forward_speed`]): against the
//! speed the controller really moves the body at. A [`WalkAim::Point`] walk asks for the distance
//! to the point, so it ends on the point.
//!
//! **Reached means got there.** The walk is judged on where the body ended ([`Goal`]): within a yard
//! of the point, or having made the asked yards (less a yard) along the heading it started on -- a
//! body that slid along a wall has covered the ground and is answered blocked. A teleport mid-walk
//! (one frame's move beyond what the speed allows, or the player gone) ends the walk blocked, with
//! the ground covered before it, and does not resume. A point already within a yard is answered at
//! once as reached with nothing walked; a point farther than [`MAX_WALK`] is refused at once
//! (`asked` is its distance, blocked, nothing walked).
//!
//! A request that cannot be walked (yards not a positive number, a point that is not finite, a
//! speed of nothing) is answered on its first frame as blocked with nothing walked: no turn, no
//! key. The host is never left waiting.
//!
//! Unlike the step back there is no face to the target afterwards and nothing selected is read. The
//! two requests share the one forward flag: a host asks for one at a time.

use benilla_assets::coords::{bevy_to_wow, wow_to_bevy};
use bevy::prelude::*;

use crate::net::{Embodied, SelfPlayer, UnitSpeeds};

use super::camera::FlyCam;
use super::follow::bearing_to;
use super::host_step::{
    forward_speed, ground, walked_enough, HEADWAY_SHARE, STALL_AFTER, TIME_SLACK,
};
use super::state::{MoveSpeed, Player};

/// The longest walk (yards): a longer ask is walked this far.
pub const MAX_WALK: f32 = 150.0;

/// Where a walk goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WalkAim {
    /// Straight ahead, on the facing the body has: no turn.
    Facing,
    /// Toward the point `(x, y)` in the server's coordinates (what `.gps` prints): a turn to it
    /// first, then at most the ground distance to it.
    Point { x: f32, y: f32 },
}

/// The host's standing request: walk `yards` along `aim`. Removed on the frame the walk ends,
/// whatever the answer.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct HostWalk {
    pub yards: f32,
    pub aim: WalkAim,
}

/// What a [`HostWalk`] did.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct HostWalked {
    /// The yards the walk aimed at: those asked (at most [`MAX_WALK`]), for a point the ground
    /// distance to it (over [`MAX_WALK`]: refused, `asked` says how far it was). The yards as asked
    /// when the request could not be walked.
    pub asked: f32,
    /// The ground covered from where he stood (yards).
    pub walked: f32,
    /// The walk did not get there: given up short of `asked` (no headway for [`STALL_AFTER`], out
    /// of time, a speed of nothing, a teleport), ended away from the point or heading, or could not
    /// be walked at all.
    pub blocked: bool,
    /// Where the body ended, in the server's coordinates `[x, y, z]`.
    pub end: [f32; 3],
}

/// What counts as having got there: the walk is judged on where the body ended, not on the ground
/// it covered (a body that slides along a wall covers the ground and gets nowhere).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Goal {
    /// A point (Bevy space, height ignored): reached within [`REACH`] of it.
    Point(Vec3),
    /// A heading (horizontal, unit): reached when the headway made along it is within [`REACH`] of
    /// the asked yards.
    Axis(Vec3),
}

impl Goal {
    fn reached(self, from: Vec3, end: Vec3, asked: f32) -> bool {
        match self {
            Goal::Point(p) => ground(end, p) <= REACH,
            Goal::Axis(a) => Vec3::new(end.x - from.x, 0.0, end.z - from.z).dot(a) >= asked - REACH,
        }
    }
}

/// How near the end must be to the point (or to the asked yards along the heading) to count as
/// reached, and how near a point may already be for a walk to it to need no step at all (yards).
const REACH: f32 = 1.0;

/// Where the walk is. Between requests, [`Run::Idle`].
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub(super) enum Run {
    #[default]
    Idle,
    /// Turned on the last frame (or not at all); the key goes down on this one, so the server hears
    /// the turn before the first step.
    Turned { from: Vec3, asked: f32, goal: Goal },
    /// The forward key is held.
    Walking {
        from: Vec3,
        asked: f32,
        goal: Goal,
        /// Seconds since the key went down.
        elapsed: f32,
        /// Seconds without headway.
        stalled: f32,
        /// Where the body stood on the last frame.
        last: Vec3,
        /// The yards the body could have covered so far: the sum of each frame's speed times its
        /// dt. The time-out is measured on this (see below), not on the clock, so a speed that
        /// changes mid-walk (a snare ending, walk mode toggled) cannot shrink the bound.
        potential: f32,
        /// The fastest speed seen so far; the time-out's slack is [`TIME_SLACK`] seconds of it.
        top_speed: f32,
    },
    /// The key was released on the last frame (that frame's controller sent the stop); this one
    /// measures and answers.
    Settling {
        from: Vec3,
        asked: f32,
        goal: Goal,
        blocked: bool,
        /// Where to measure and report the end when the body is not to be read where it stands now
        /// (it was carried off: a teleport mid-walk). `None`: where it stands.
        end: Option<Vec3>,
    },
}

/// The yards a request may be walked: `None` for yards that are not finite or not above 0, else at
/// most [`MAX_WALK`].
fn walk_yards(yards: f32) -> Option<f32> {
    (yards.is_finite() && yards > 0.0).then(|| yards.min(MAX_WALK))
}

/// The horizontal delta (Bevy space) from `pos` to the server-coordinate point `(x, y)`; heights
/// ignored. `None` for a point that is not finite.
fn delta_to_point(pos: Vec3, x: f32, y: f32) -> Option<Vec3> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let at = wow_to_bevy([x, y, 0.0]);
    Some(Vec3::new(at.x - pos.x, 0.0, at.z - pos.z))
}

/// Answer a request on the spot: the request is withdrawn and `HostWalked` written, nothing walked.
fn answer_now(
    commands: &mut Commands,
    told: &mut MessageWriter<HostWalked>,
    asked: f32,
    blocked: bool,
    pos: Vec3,
) {
    commands.remove_resource::<HostWalk>();
    told.write(HostWalked {
        asked,
        walked: 0.0,
        blocked,
        end: bevy_to_wow(pos),
    });
}

/// Resolve the pending request: turn, walk, release, answer.
///
/// note: the end's height is wherever the body is when the key is released (it may be mid-air on a
/// slope or a jump); a host reads X and Y. note: a host asks for one walk (or step back) at a time
/// -- the two requests share the one forward flag, and nothing here enforces it.
#[allow(clippy::too_many_arguments)]
pub(super) fn walk(
    mut commands: Commands,
    request: Option<Res<HostWalk>>,
    mut run: ResMut<Run>,
    mut player: ResMut<Player>,
    move_speed: Res<MoveSpeed>,
    body: Query<Option<&UnitSpeeds>, (With<Embodied>, Without<FlyCam>)>,
    time: Res<Time>,
    self_player: Query<(), With<SelfPlayer>>,
    mut told: MessageWriter<HostWalked>,
) {
    let Some(request) = request else {
        *run = Run::Idle;
        return; // nothing asked
    };
    if self_player.single().is_err() {
        if *run != Run::Idle {
            // The body is gone mid-walk (a map change, a drop): the walk is over, not reached, and
            // must not pick up again when a body comes back.
            info!("host walk: the player is gone, ended");
            player.follow_forward = false;
            let (from, asked) = match *run {
                Run::Turned { from, asked, .. }
                | Run::Walking { from, asked, .. }
                | Run::Settling { from, asked, .. } => (from, asked),
                Run::Idle => unreachable!(),
            };
            *run = Run::Idle;
            commands.remove_resource::<HostWalk>();
            told.write(HostWalked {
                asked,
                walked: ground(player.pos, from),
                blocked: true,
                end: bevy_to_wow(player.pos),
            });
        }
        return; // not in the world yet -- pending, silent
    }
    let dt = time.delta_secs();
    let pos = player.pos;
    let granted = body.single().ok().flatten().map(|s| s.0);
    let speed = forward_speed(granted, &move_speed, player.walking, player.swimming);
    match *run {
        Run::Idle => {
            // The aim, resolved against where he stands now: the yards to walk, the turn to take and
            // what counts as being there.
            let Some(yards) = walk_yards(request.yards) else {
                debug!("host walk: not walked ({} yd asked, {:?})", request.yards, request.aim);
                answer_now(&mut commands, &mut told, request.yards, true, pos);
                return;
            };
            let (asked, turn, goal) = match request.aim {
                WalkAim::Facing => {
                    let forward = Quat::from_rotation_y(player.facing()) * Vec3::NEG_Z;
                    (yards, 0.0, Goal::Axis(Vec3::new(forward.x, 0.0, forward.z)))
                }
                WalkAim::Point { x, y } => {
                    let Some(d) = delta_to_point(pos, x, y) else {
                        debug!("host walk: not walked (point {x}, {y} is not finite)");
                        answer_now(&mut commands, &mut told, request.yards, true, pos);
                        return;
                    };
                    let to_point = d.length();
                    if to_point > MAX_WALK {
                        // Walking the cap would end short of the point and could not say "reached".
                        info!(
                            "host walk: point {to_point:.0} yd away, over the {MAX_WALK:.0} yd cap, not walked"
                        );
                        answer_now(&mut commands, &mut told, to_point, true, pos);
                        return;
                    }
                    if to_point <= REACH {
                        // Already there: nothing to walk, and nothing in the way.
                        debug!("host walk: already at the point ({to_point:.1} yd)");
                        answer_now(&mut commands, &mut told, to_point, false, pos);
                        return;
                    }
                    let turn = super::wrap_pi(bearing_to(d) - player.facing());
                    if yards >= to_point {
                        (to_point, turn, Goal::Point(pos + d))
                    } else {
                        (yards, turn, Goal::Axis(d / to_point))
                    }
                }
            };
            if speed.is_none() {
                debug!("host walk: not walked ({} yd asked, {:?}, no speed)", request.yards, request.aim);
                answer_now(&mut commands, &mut told, request.yards, true, pos);
                return;
            }
            if turn != 0.0 {
                player.turn_aim(turn);
            }
            *run = Run::Turned { from: pos, asked, goal };
        }
        Run::Turned { from, asked, goal } => {
            player.follow_forward = true;
            *run = Run::Walking {
                from,
                asked,
                goal,
                elapsed: 0.0,
                stalled: 0.0,
                last: pos,
                potential: 0.0,
                top_speed: 0.0,
            };
        }
        Run::Walking {
            from,
            asked,
            goal,
            elapsed,
            mut stalled,
            last,
            mut potential,
            mut top_speed,
        } => {
            let elapsed = elapsed + dt;
            let walked = ground(pos, from);
            // The speed fell to nothing mid-walk: the key comes up on this frame, blocked -- no
            // headway can be measured against nothing.
            let (done, given_up, jumped) = match speed {
                Some(speed) => {
                    // A frame's move far beyond what the body can walk in it is a teleport (a
                    // `.go`, a map change, a snap back): the walk is over, not reached, and the
                    // ground it covered is what it covered before the jump.
                    let jumped = ground(pos, last) > 2.0 * speed * dt + 1.0;
                    if ground(pos, last) < HEADWAY_SHARE * speed * dt && elapsed > 0.25 {
                        stalled += dt;
                    } else {
                        stalled = 0.0;
                    }
                    // The time-out: the walk has had the yards it could cover at its speeds so far
                    // (summed per frame) plus `TIME_SLACK` seconds of the fastest speed seen. The
                    // sum only grows and the slack only grows, so a speed that rises mid-walk (a
                    // snare ends) cannot cut a slow start short; at one steady speed it is the
                    // plain `asked / speed + TIME_SLACK` seconds.
                    potential += speed * dt;
                    top_speed = top_speed.max(speed);
                    let timed_out = potential > asked + TIME_SLACK * top_speed;
                    (
                        !jumped && walked_enough(walked, asked, speed, dt),
                        stalled >= STALL_AFTER || timed_out || jumped,
                        jumped,
                    )
                }
                None => (false, true, false),
            };
            if done || given_up {
                if jumped {
                    info!(
                        "host walk: position jumped {:.0} yd in one frame, ended",
                        ground(pos, last)
                    );
                }
                // The key is released here: this frame's controller is the one that stops.
                *run = Run::Settling {
                    from,
                    asked,
                    goal,
                    blocked: !done,
                    end: jumped.then_some(last),
                };
            } else {
                player.follow_forward = true;
                *run = Run::Walking {
                    from,
                    asked,
                    goal,
                    elapsed,
                    stalled,
                    last: pos,
                    potential,
                    top_speed,
                };
            }
        }
        Run::Settling { from, asked, goal, blocked, end } => {
            let at = end.unwrap_or(pos);
            let walked = ground(at, from);
            // Sliding along a wall covers the ground without getting there: reached is judged on
            // the end, against the point or the heading.
            let blocked = blocked || !goal.reached(from, at, asked);
            let end = bevy_to_wow(at);
            debug!(
                "host walk: walked {walked:.1} yd of {asked:.1}, at X {:.1} Y {:.1}{}",
                end[0],
                end[1],
                if blocked { ", blocked" } else { "" },
            );
            *run = Run::Idle;
            commands.remove_resource::<HostWalk>();
            told.write(HostWalked {
                asked,
                walked,
                blocked,
                end,
            });
        }
    }
}

/// Register the walk. After the follow's steer (which rewrites the flag every frame) and before
/// the controller that reads it, like the step back.
pub(super) fn plugin(app: &mut App) {
    app.init_resource::<Run>()
        .add_message::<HostWalked>()
        .add_systems(
            Update,
            walk.in_set(benilla_world::schedule::WorldStage::Input)
                .after(super::follow::steer_follow)
                .before(super::control)
                .in_set(crate::char_select::InWorldGated),
        );
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;
    use std::f32::consts::FRAC_PI_2;

    use benilla_protocol::MoveSpeeds;

    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

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

    struct Rig {
        app: App,
        keyed: bool,
        /// Frames in which the forward key was down.
        key_frames: u32,
        /// How fast the "controller" moves the body (yd/s), as the speed set granted.
        speed: f32,
        /// The angle the body drifts off its facing as it moves (a slide along a wall).
        skew: f32,
    }

    impl Rig {
        /// One in the world at the origin facing yaw 0 (north: server +X), the vanilla speed set
        /// granted and moved by (run 7), and nothing selected.
        fn new() -> Self {
            let mut app = App::new();
            app.add_plugins(MinimalPlugins);
            app.init_resource::<Player>();
            app.init_resource::<Run>();
            app.insert_resource(MoveSpeed {
                value: 7.0,
                env_override: false,
            });
            app.add_message::<HostWalked>();
            app.world_mut()
                .spawn((SelfPlayer, Embodied, UnitSpeeds(vanilla())));
            Rig {
                app,
                keyed: false,
                key_frames: 0,
                speed: 7.0,
                skew: 0.0,
            }
        }

        fn ask(&mut self, yards: f32, aim: WalkAim) {
            self.app.world_mut().insert_resource(HostWalk { yards, aim });
        }

        /// One frame: the system, then the "controller" -- the body runs along its facing for the
        /// frame when the key is down, at 7 yd/s, until its server-X passes `wall_x` (a wall).
        fn frame(&mut self, dt: f32, wall_x: Option<f32>) {
            self.app
                .world_mut()
                .resource_mut::<Time>()
                .advance_by(std::time::Duration::from_secs_f32(dt));
            self.app.world_mut().run_system_once(walk).unwrap();
            self.app.world_mut().flush();
            let mut player = self.app.world_mut().resource_mut::<Player>();
            self.keyed |= player.follow_forward;
            if player.follow_forward {
                self.key_frames += 1;
                let forward = Quat::from_rotation_y(player.facing() + self.skew) * Vec3::NEG_Z;
                let next = player.pos + forward * self.speed * dt;
                if wall_x.is_none_or(|w| bevy_to_wow(next)[0] <= w) {
                    player.pos = next;
                }
            }
            player.follow_forward = false; // `steer_follow`'s rewrite, the next frame's start
        }

        fn walk_out(&mut self, seconds: f32, wall_x: Option<f32>) -> u32 {
            let mut frames = 0;
            while self.pending() && (frames as f32) < seconds * 60.0 {
                self.frame(1.0 / 60.0, wall_x);
                frames += 1;
            }
            frames
        }

        fn told(&mut self) -> Vec<HostWalked> {
            let mut messages = self.app.world_mut().resource_mut::<Messages<HostWalked>>();
            messages.drain().collect()
        }

        fn pending(&self) -> bool {
            self.app.world().contains_resource::<HostWalk>()
        }

        fn pos(&self) -> Vec3 {
            self.app.world().resource::<Player>().pos
        }
    }

    /// No target needed: 20 yd straight ahead is walked on the facing he has, with no turn, and
    /// the end is told in the server's coordinates (yaw 0 is server +X).
    #[test]
    fn a_walk_ahead_needs_no_target_and_turns_nowhere() {
        let mut rig = Rig::new();
        rig.ask(20.0, WalkAim::Facing);
        let frames = rig.walk_out(10.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?} after {frames} frames");
        };
        assert!(!w.blocked && w.asked == 20.0, "{w:?}");
        assert!((w.walked - 20.0).abs() < 0.07, "{w:?}");
        assert!((w.end[0] - 20.0).abs() < 0.07 && w.end[1].abs() < 1e-3, "{w:?}");
        assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "no turn");
        // 20 yd at 7 yd/s is 172 frames; plus the start and the settle.
        assert!((172..=180).contains(&frames), "{frames} frames");
    }

    /// A wall gives no headway: the walk is given up after the stall time and says blocked, with the
    /// ground it did cover and where it ended.
    #[test]
    fn a_wall_ends_the_walk_blocked_with_the_ground_covered() {
        let mut rig = Rig::new();
        rig.ask(60.0, WalkAim::Facing);
        rig.walk_out(20.0, Some(41.8));
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(w.blocked && w.asked == 60.0, "{w:?}");
        assert!(w.walked > 41.0 && w.walked <= 41.8, "{w:?}");
        assert!(close(w.end[0], w.walked) && w.end[1].abs() < 1e-3, "{w:?}");
        assert!(!rig.pending());
    }

    /// A point is walked to: turned to first (a frame before any step), asked at most the distance
    /// to it, and the end lands on it. The point is in server coordinates: Y 30 is west of him
    /// (server +Y), a quarter turn counter-clockwise.
    #[test]
    fn a_walk_to_a_point_turns_to_it_and_ends_on_it() {
        let mut rig = Rig::new();
        rig.ask(MAX_WALK, WalkAim::Point { x: 0.0, y: 30.0 });
        rig.frame(1.0 / 60.0, None);
        let facing = rig.app.world().resource::<Player>().facing();
        assert!(close(facing, FRAC_PI_2), "turned first: {facing}");
        assert_eq!(rig.pos(), Vec3::ZERO, "no step yet");
        rig.walk_out(20.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(!w.blocked && close(w.asked, 30.0), "{w:?}");
        assert!((w.walked - 30.0).abs() < 0.07, "{w:?}");
        assert!(w.end[0].abs() < 1e-2 && (w.end[1] - 30.0).abs() < 0.07, "{w:?}");
    }

    /// A point farther than the yards asked is walked only the yards asked.
    #[test]
    fn a_point_is_walked_no_farther_than_the_yards_asked() {
        let mut rig = Rig::new();
        rig.ask(10.0, WalkAim::Point { x: 100.0, y: 0.0 });
        rig.walk_out(10.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(!w.blocked && w.asked == 10.0, "{w:?}");
        assert!((w.end[0] - 10.0).abs() < 0.07, "{w:?}");
    }

    /// A walk past [`MAX_WALK`] is walked that far, and says so.
    #[test]
    fn a_walk_past_the_longest_is_walked_the_longest() {
        let mut rig = Rig::new();
        rig.ask(1000.0, WalkAim::Facing);
        rig.walk_out(60.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert_eq!(w.asked, MAX_WALK);
        assert!(!w.blocked && (w.walked - MAX_WALK).abs() < 0.1, "{w:?}");
    }

    /// Yards that are not a positive number, a point that is not a number:
    /// answered on the first frame as blocked with nothing walked -- no turn, no key.
    #[test]
    fn what_cannot_be_walked_is_answered_at_once() {
        let asks = [
            (f32::NAN, WalkAim::Facing),
            (0.0, WalkAim::Facing),
            (-3.0, WalkAim::Facing),
            (f32::INFINITY, WalkAim::Facing),
            (10.0, WalkAim::Point { x: f32::NAN, y: 0.0 }),
            (10.0, WalkAim::Point { x: 0.0, y: f32::INFINITY }),
        ];
        for (yards, aim) in asks {
            let mut rig = Rig::new();
            rig.ask(yards, aim);
            rig.frame(1.0 / 60.0, None);
            let told = rig.told();
            let [w] = told.as_slice() else {
                panic!("{yards} {aim:?}: {told:?}");
            };
            assert!(w.blocked && w.walked == 0.0, "{yards} {aim:?}: {w:?}");
            assert_eq!(w.asked.to_bits(), yards.to_bits(), "{yards} {aim:?}");
            assert!(!rig.pending() && !rig.keyed, "{yards} {aim:?}");
            assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "no turn");
        }
    }

    /// A speed of nothing is answered on the first frame as blocked.
    #[test]
    fn a_speed_of_nothing_ends_the_walk_at_once() {
        let mut rig = Rig::new();
        *rig.app.world_mut().resource_mut::<MoveSpeed>() = MoveSpeed {
            value: 0.0,
            env_override: true,
        };
        rig.ask(10.0, WalkAim::Facing);
        rig.frame(1.0 / 60.0, None);
        let told = rig.told();
        assert!(
            matches!(told.as_slice(), [w] if w.blocked && w.walked == 0.0),
            "{told:?}"
        );
        assert!(!rig.pending() && !rig.keyed);
    }

    /// A teleport mid-walk (here 50 yd in one frame, then back) ends the walk as not reached, with
    /// the ground covered before the jump, the key up -- and it does not pick up when the body is
    /// back where it was.
    #[test]
    fn a_teleport_mid_walk_ends_it_blocked_and_it_does_not_resume() {
        let mut rig = Rig::new();
        rig.ask(60.0, WalkAim::Facing);
        for _ in 0..60 {
            rig.frame(1.0 / 60.0, None);
        }
        let before = rig.pos();
        rig.app.world_mut().resource_mut::<Player>().pos = before + Vec3::NEG_Z * 50.0;
        rig.frame(1.0 / 60.0, None); // the jump is seen: the key comes up
        rig.app.world_mut().resource_mut::<Player>().pos = before; // and he is carried back
        let keyed = rig.key_frames;
        rig.walk_out(5.0, None);
        assert_eq!(rig.key_frames, keyed, "the key stayed up");
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(w.blocked && w.asked == 60.0, "{w:?}");
        assert!(w.walked > 6.0 && w.walked < 7.5, "the ground before the jump: {w:?}");
        assert!(!rig.pending());
        rig.walk_out(2.0, None);
        assert!(rig.told().is_empty(), "no second answer");
    }

    /// The player gone mid-walk (no `SelfPlayer`) answers it at once as blocked.
    #[test]
    fn the_player_gone_mid_walk_ends_it_blocked() {
        let mut rig = Rig::new();
        rig.ask(60.0, WalkAim::Facing);
        for _ in 0..30 {
            rig.frame(1.0 / 60.0, None);
        }
        let body = rig
            .app
            .world_mut()
            .query_filtered::<Entity, With<SelfPlayer>>()
            .single(rig.app.world())
            .unwrap();
        rig.app.world_mut().despawn(body);
        rig.frame(1.0 / 60.0, None);
        let told = rig.told();
        assert!(matches!(told.as_slice(), [w] if w.blocked && w.walked > 1.0), "{told:?}");
        assert!(!rig.pending());
    }

    /// A point farther than the cap is refused at once -- never answered reached: `asked` is its
    /// distance, nothing walked, no turn, no key. One at the cap is walked.
    #[test]
    fn a_point_over_the_cap_is_refused_at_once() {
        let mut rig = Rig::new();
        rig.ask(MAX_WALK, WalkAim::Point { x: 200.0, y: 0.0 });
        rig.frame(1.0 / 60.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(w.blocked && w.walked == 0.0 && close(w.asked, 200.0), "{w:?}");
        assert!(!rig.pending() && !rig.keyed);
        assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "no turn");

        let mut rig = Rig::new();
        rig.ask(MAX_WALK, WalkAim::Point { x: 149.0, y: 0.0 });
        rig.walk_out(60.0, None);
        let told = rig.told();
        assert!(matches!(told.as_slice(), [w] if !w.blocked && close(w.asked, 149.0)), "{told:?}");
    }

    /// Sliding along a wall covers the ground without getting there: a body that drifts 35 degrees
    /// off its facing walks the 20 yd asked but makes only 16 yd along it -- blocked.
    #[test]
    fn sliding_off_the_heading_is_not_reached() {
        let mut rig = Rig::new();
        rig.skew = 0.6;
        rig.ask(20.0, WalkAim::Facing);
        rig.walk_out(10.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(w.blocked && (w.walked - 20.0).abs() < 0.1, "{w:?}");
        assert!(w.end[0] < 19.0, "{w:?}");
    }

    /// The same drift on a walk to a point ends well away from it: blocked. A body a little off
    /// (under a yard at the end) still got there.
    #[test]
    fn missing_the_point_is_not_reached() {
        let mut rig = Rig::new();
        rig.skew = 0.6;
        rig.ask(MAX_WALK, WalkAim::Point { x: 30.0, y: 0.0 });
        rig.walk_out(10.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(w.blocked && close(w.asked, 30.0), "{w:?}");

        let mut rig = Rig::new();
        rig.skew = 0.02; // 0.6 yd off at 30 yd
        rig.ask(MAX_WALK, WalkAim::Point { x: 30.0, y: 0.0 });
        rig.walk_out(10.0, None);
        let told = rig.told();
        assert!(matches!(told.as_slice(), [w] if !w.blocked), "{told:?}");
    }

    /// A speed that rises mid-walk does not shrink the time-out: 8 s in walk mode (2.5 yd/s, 20
    /// yd), then run (7 yd/s) for the last 10 yd of 30 -- 9.4 s in all, past the 7.3 s the run
    /// speed alone would allow, and still reached.
    #[test]
    fn a_speed_that_rises_does_not_time_the_walk_out() {
        let mut rig = Rig::new();
        rig.app.world_mut().resource_mut::<Player>().walking = true;
        rig.speed = 2.5;
        rig.ask(30.0, WalkAim::Facing);
        for _ in 0..(8 * 60) {
            rig.frame(1.0 / 60.0, None);
        }
        assert!(rig.pending());
        rig.app.world_mut().resource_mut::<Player>().walking = false;
        rig.speed = 7.0;
        rig.walk_out(10.0, None);
        let told = rig.told();
        let [w] = told.as_slice() else {
            panic!("{told:?}");
        };
        assert!(!w.blocked && (w.walked - 30.0).abs() < 0.2, "{w:?}");
    }

    /// A point he already stands within a yard of is reached at once: nothing walked, not blocked,
    /// no turn, no key.
    #[test]
    fn a_point_already_there_is_reached_at_once() {
        for (x, y) in [(0.0, 0.0), (0.6, 0.0), (0.0, -0.9)] {
            let mut rig = Rig::new();
            rig.ask(10.0, WalkAim::Point { x, y });
            rig.frame(1.0 / 60.0, None);
            let told = rig.told();
            let [w] = told.as_slice() else {
                panic!("{x} {y}: {told:?}");
            };
            assert!(!w.blocked && w.walked == 0.0, "{x} {y}: {w:?}");
            assert!(!rig.pending() && !rig.keyed, "{x} {y}");
            assert_eq!(rig.app.world().resource::<Player>().facing(), 0.0, "no turn");
        }
    }
}
