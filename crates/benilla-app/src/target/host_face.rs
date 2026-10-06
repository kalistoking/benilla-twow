//! **A host's turn to the target** -- the player's own turn toward what is selected, for a test
//! lab with no hand on the mouse.
//!
//! A lab that fights an NPC has the character standing wherever `.go` put it, facing whatever way
//! it was facing; the server refuses every melee swing that is not made toward the victim
//! (`SMSG_ATTACKSWING_BADFACING`, "You are facing the wrong way!"), so the swings are lost. A
//! player turns. This does what the turn does and nothing more: it writes the facing
//! ([`Player::turn_aim`], the scripted mouse-turn's lever), and from there it is the identical
//! path a mouse-turn takes -- `stream_self_movement` sees the facing differ and sends
//! `MSG_MOVE_SET_FACING`, standing or moving, and the body turns to it. No teleport, no GM
//! command, no orientation the server did not hear from the client.
//!
//! The camera is put behind the character as a player would put it to see the fight: on the new
//! facing, framing **the whole target, feet to head, by its size** ([`framing`]) -- a target of
//! ordinary size at [`FRAME_DISTANCE`] and [`FRAME_PITCH`] as it always was, a giant from further
//! back and tilted up toward level, so its head is in the shot and not only its legs. The levers
//! are [`FlyCam::park`] and [`CameraControl::park_framed`], the scripted camera park's, the second
//! holding past the zoom slider's ceiling until a hand moves the wheel.
//!
//! What the camera does after the park: the auto-follow ([`crate::player::camera`]'s
//! `FollowRig`) only ever returns the yaw to directly behind the character -- where the park put
//! it -- and never touches the distance or the pitch; the zoom's per-frame re-clamp to the slider's
//! ceiling (15 yd at rest), which would have glided a giant's 30 yd back to 15 within two seconds,
//! respects the framed park ([`CameraControl::park_framed`]). What still can pull it in is the
//! boom's collision sweep: a wall or a slope behind the character shortens the arm, as it must.
//!
//! Unlike a click or a use, the request never waits: there is a selection now or there is not, so
//! it is consumed on the first in-world frame and answered with [`HostFacedTarget`] either way.

use benilla_world::view::CAM_FOVY;
use bevy::prelude::*;

use super::Selection;
use crate::entities::StandBoxHeight;
use crate::net::{NetEntity, ObjectStore, SelfPlayer};
use crate::player::camera::FlyCam;
use crate::player::{head_height, CameraControl, CameraPivot, Player};

/// The camera's pitch after the turn for a target of ordinary size (radians, `+` = up): a little
/// downward, so a target in melee range stands in the frame whole, not just its feet -- world
/// entry's `-0.45` looks at the dirt. The lowest pitch [`framing`] picks.
const FRAME_PITCH: f32 = -0.2;

/// The highest pitch [`framing`] tilts to for a tall target: **level**. Above it the camera would
/// sit below the character's neck (`seat = pivot - forward * distance`), 30 yd back and yards
/// down -- in the ground he stands on, where the boom's collision sweep pulls the camera in and
/// the framing is lost. Level keeps the camera at neck height and lets the distance do the rest.
const FRAME_PITCH_LEVEL: f32 = 0.0;

/// The pitch search's step (radians, ~0.3 deg): forty-one candidates between the two above.
const FRAME_PITCH_STEP: f32 = 0.005;

/// The camera's distance behind the character after the turn (yards) for a target of ordinary
/// size: the character and a target in melee range in one frame. The **minimum** -- [`framing`]
/// only ever pulls back from it.
const FRAME_DISTANCE: f32 = 9.0;

/// The furthest [`framing`] pulls back (yards): the reference's own hard cap of 50 on
/// `cameraDistanceMax`, so the host's camera never stands where a player's could not.
///
/// It is sized by the biggest bodies the game draws, not by taste. An Anubisath Guardian (Ruins of
/// Ahn'Qiraj, display 15347, `Creature\Anubisath\Anubisath.m2` at scale 2.5) is **19.5 yd** to the
/// top of its Stand box (7.80 model-local; every other sequence box 7.6-8.6, the bind-pose mesh
/// 8.07 -- the box is the body, not inflated), and at melee range, 3.8 yd ahead, it needs 50.9 yd
/// with the full [`FRAME_MARGIN`]. At 50 it is framed level, feet to head, its head ~4.3 deg under
/// the frame's top edge (~150 px of a 1440-line shot) -- the margin spent down from 4.6 deg, not
/// the head. The proof run that had this at 40 cut the Guardian's head and ears off (the box top
/// then sat 0.5 deg inside the edge, the ears outside it). At 50 One (~2 yd) still stands ~1/20 of
/// the frame's height beside the target.
const FRAME_DISTANCE_CAP: f32 = 50.0;

/// The frame's margin, top and bottom (radians, ~4.6 deg, about a tenth of the 45 deg frame
/// height): the action bars cover the bottom of a shot and the unit frames its top, so a head or a
/// pair of feet exactly on the edge would be under the interface.
const FRAME_MARGIN: f32 = 0.08;

/// A combat reach read as a height: the client's own two defaults side by side -- its empty-world
/// collision height ([`crate::player::DEFAULT_COLLISION_HEIGHT`], a human male's 2.03 yd) over the
/// descriptor's default reach (1.5 yd, `unit_combat_reach`'s fallback and every player's) -- so a
/// reach of 1.5 is a body of 2.03 yd and a reach of 6 a body four times that. One ratio, no table.
/// Only the fallback for a target whose model has not loaded ([`TargetSize::CombatReach`]).
const REACH_TO_HEIGHT: f32 = crate::player::DEFAULT_COLLISION_HEIGHT / 1.5;

/// The host's standing request: turn to the current target. Removed by [`face_target`] on the
/// first frame the character is in the world, whatever the answer.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostFaceTarget;

/// What a [`HostFaceTarget`] did.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub enum HostFacedTarget {
    /// The character turned to the selected unit: `degrees` is the turn made (counter-clockwise
    /// seen from above is positive, `0.0` when it already faced it), `distance` the yards on the
    /// ground to the target. The camera was parked `camera_distance` yards behind the character at
    /// `camera_pitch` radians (`+` = up), framing a target of `target_size` ([`framing`]).
    Turned {
        guid: u64,
        degrees: f32,
        distance: f32,
        camera_distance: f32,
        camera_pitch: f32,
        target_size: TargetSize,
    },
    /// Nothing is selected (or the selection is not streamed): no turn, the camera untouched.
    NoTarget,
}

/// How tall the target is, and what the client read it from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TargetSize {
    /// The model the client drew: its **Stand** animation box's Z extent (the M2 `CAaBox`
    /// benilla parsed for the render, [`StandBoxHeight`], model-local) times `OBJECT_FIELD_SCALE_X`
    /// ([`NetEntity::scale`]). Yards, feet to head.
    Model { height: f32 },
    /// The model has not loaded (or has no box): `UNIT_FIELD_COMBATREACH` read as a body of the
    /// default proportion ([`REACH_TO_HEIGHT`]). The server sends the reach already scaled by the
    /// unit's `OBJECT_FIELD_SCALE_X` (vmangos `Unit::UpdateModelData`: `scale * combat_reach`), so
    /// it is not multiplied by the scale a second time. Yards.
    CombatReach { height: f32 },
    /// Neither: the camera takes the ordinary framing.
    Unknown,
}

impl TargetSize {
    /// The height in yards, when one is known.
    pub fn height(&self) -> Option<f32> {
        match *self {
            Self::Model { height } | Self::CombatReach { height } => Some(height),
            Self::Unknown => None,
        }
    }
}

/// The target's size from what the client holds for it: the drawn model's Stand box (model-local
/// yards) times the object scale, else the combat reach, else unknown. A zero box is a model not
/// loaded yet (or a bounds-less display), not a unit of no height.
fn target_size(stand_box: Option<f32>, scale: f32, reach: Option<f32>) -> TargetSize {
    let usable = |v: f32| v.is_finite() && v > 0.0;
    if let Some(h) = stand_box.map(|b| b * scale).filter(|h| usable(*h)) {
        return TargetSize::Model { height: h };
    }
    match reach.filter(|r| usable(*r)) {
        Some(r) => TargetSize::CombatReach {
            height: r * REACH_TO_HEIGHT,
        },
        None => TargetSize::Unknown,
    }
}

/// What the camera has to frame, every height measured from the character's feet (yards).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Scene {
    /// The target's height, feet to head.
    height: f32,
    /// The target's ground distance in front of the character (he faces it).
    ahead: f32,
    /// The target's feet above the character's (negative below).
    rise: f32,
    /// The camera's pivot above the character's feet -- what the camera looks at, his neck.
    pivot: f32,
}

/// **The framing**: the camera's `(distance, pitch)` behind the character so that the whole
/// target, feet to head, and the character himself stand inside the vertical field of view
/// ([`CAM_FOVY`]), each edge [`FRAME_MARGIN`] in.
///
/// The camera looks at the pivot `P` from `C = P - f * d`, `f` the unit forward at pitch `p`, so it
/// stands `d cos p` behind the character and `d sin p` below the pivot (above it, for `p < 0`). A
/// point `y` high (from the feet) and `x` ahead of the character is inside the top edge when
/// `y <= C_y + (d cos p + x) tan(p + h)`, `h` the half field of view less the margin; solved for
/// `d` (with `cos p tan(p + h) - sin p = sin h / cos(p + h)`):
///
/// - the target's head, top edge: `d >= ((top - pivot) cos(p + h) - ahead sin(p + h)) / sin h`
/// - the target's feet, bottom edge: `d >= ((pivot - rise) cos(p - h) + ahead sin(p - h)) / sin h`
/// - the character's own feet, bottom edge: the same with `rise = 0`, `ahead = 0`
///
/// The distance a pitch needs is the largest of the three and [`FRAME_DISTANCE`]; the pitch is the
/// one from [`FRAME_PITCH`] up to [`FRAME_PITCH_LEVEL`] that needs the least distance, the lowest
/// winning a tie -- so a target that fits from 9 yd keeps today's pitch, and a tall one tilts up
/// only as far as tilting shortens the pull-back. The distance is then capped at
/// [`FRAME_DISTANCE_CAP`]. No scene (no size known) is the ordinary framing.
fn framing(scene: Option<Scene>) -> (f32, f32) {
    let Some(s) = scene.filter(|s| {
        [s.height, s.ahead, s.rise, s.pivot].iter().all(|v| v.is_finite()) && s.height > 0.0
    }) else {
        return (FRAME_DISTANCE, FRAME_PITCH);
    };
    let half = CAM_FOVY * 0.5 - FRAME_MARGIN;
    let sin_half = half.sin();
    let top = s.rise + s.height;
    let need = |p: f32| {
        let (up, down) = (p + half, p - half);
        let head = ((top - s.pivot) * up.cos() - s.ahead * up.sin()) / sin_half;
        let feet = ((s.pivot - s.rise) * down.cos() + s.ahead * down.sin()) / sin_half;
        let own_feet = s.pivot * down.cos() / sin_half;
        FRAME_DISTANCE.max(head).max(feet).max(own_feet)
    };
    let steps = ((FRAME_PITCH_LEVEL - FRAME_PITCH) / FRAME_PITCH_STEP).round() as i32;
    let mut best = (need(FRAME_PITCH), FRAME_PITCH);
    for i in 1..=steps {
        // As a fraction of the span, so the last candidate is level exactly, not level + 1 ulp.
        let p = FRAME_PITCH + (FRAME_PITCH_LEVEL - FRAME_PITCH) * (i as f32 / steps as f32);
        let d = need(p);
        if d < best.0 - 1e-4 {
            best = (d, p);
        }
    }
    (best.0.min(FRAME_DISTANCE_CAP), best.1)
}

/// Wrap an angle into `(-π, π]`.
fn wrap_pi(a: f32) -> f32 {
    let t = std::f32::consts::TAU;
    let x = (a + std::f32::consts::PI).rem_euclid(t);
    x - std::f32::consts::PI
}

/// The facing that looks along a horizontal Bevy-space delta: the controller walks along
/// `Quat::from_rotation_y(face_yaw) * NEG_Z = (-sin y, 0, -cos y)` (the follow's `bearing_to`).
fn bearing_to(delta: Vec3) -> f32 {
    (-delta.x).atan2(-delta.z)
}

/// Resolve the pending request: turn, frame, answer.
pub(super) fn face_target(
    mut commands: Commands,
    request: Option<Res<HostFaceTarget>>,
    selection: Res<Selection>,
    targets: Query<(
        &Transform,
        Option<&NetEntity>,
        Option<&StandBoxHeight>,
        Option<&ObjectStore>,
    )>,
    mut player: ResMut<Player>,
    mut rig: ResMut<CameraControl>,
    mut cam: Query<&mut FlyCam, With<benilla_world::view::WorldCamera>>,
    self_player: Query<(Option<&CameraPivot>, Option<&NetEntity>), With<SelfPlayer>>,
    mut faced: MessageWriter<HostFacedTarget>,
) {
    if request.is_none() {
        return; // nothing asked
    }
    let Ok((my_pivot, me)) = self_player.single() else {
        return; // not in the world yet -- pending, silent
    };
    commands.remove_resource::<HostFaceTarget>();
    let target = selection
        .target
        .zip(selection.guid)
        .and_then(|(e, g)| targets.get(e).ok().map(|q| (g, q)));
    let Some((guid, (t, net, stand_box, store))) = target else {
        debug!("host face: no target");
        faced.write(HostFacedTarget::NoTarget);
        return;
    };
    let at = t.translation;
    let flat = Vec3::new(at.x - player.pos.x, 0.0, at.z - player.pos.z);
    let distance = flat.length();
    // On top of it there is no direction: the facing stays, and the turn is none.
    let turn = if distance > f32::EPSILON {
        wrap_pi(bearing_to(flat) - player.facing())
    } else {
        0.0
    };
    player.turn_aim(turn);
    let facing = wrap_pi(player.facing());
    // The raw `OBJECT_FIELD_SCALE_X` (the pivot's own choice, `head_height`'s doc), the drawn
    // transform's only where the entity carries no descriptor.
    let scale = net.map_or(t.scale.y, |n| n.scale);
    let target_size = target_size(
        stand_box.map(|b| b.0),
        scale,
        store.map(|s| s.0.unit_combat_reach()),
    );
    let scene = target_size.height().map(|height| Scene {
        height,
        ahead: distance,
        rise: at.y - player.pos.y,
        pivot: head_height(my_pivot, me.map_or(1.0, |n| n.scale)),
    });
    let (camera_distance, camera_pitch) = framing(scene);
    if let Ok(mut cam) = cam.single_mut() {
        cam.park(facing, camera_pitch);
    }
    rig.park_framed(camera_distance);
    debug!(
        "host face: {guid:#x} -> turned {:.1} deg ({distance:.1} yd), camera {camera_distance:.1} yd \
         pitch {camera_pitch:.3} ({target_size:?})",
        turn.to_degrees()
    );
    faced.write(HostFacedTarget::Turned {
        guid,
        degrees: turn.to_degrees(),
        distance,
        camera_distance,
        camera_pitch,
        target_size,
    });
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;
    use std::f32::consts::FRAC_PI_2;

    use super::*;

    const GUID: u64 = 0xF130_0000_9500_0007;

    struct Rig {
        world: World,
        camera: Entity,
    }

    impl Rig {
        /// One in the world at the origin, facing yaw 0 (Bevy -Z), the camera off to the side.
        fn new() -> Self {
            let mut world = World::new();
            world.init_resource::<Selection>();
            world.init_resource::<Player>();
            world.init_resource::<CameraControl>();
            world.init_resource::<Messages<HostFacedTarget>>();
            world.spawn(SelfPlayer);
            let camera = world
                .spawn((benilla_world::view::WorldCamera, FlyCam::at(2.0, -0.45)))
                .id();
            Rig { world, camera }
        }

        fn select(&mut self, at: Vec3) -> Entity {
            let e = self.world.spawn(Transform::from_translation(at)).id();
            let mut selection = self.world.resource_mut::<Selection>();
            selection.target = Some(e);
            selection.guid = Some(GUID);
            e
        }

        fn frame(&mut self) {
            self.world.run_system_once(face_target).unwrap();
        }

        fn told(&mut self) -> Vec<HostFacedTarget> {
            let mut messages = self.world.resource_mut::<Messages<HostFacedTarget>>();
            messages.drain().collect()
        }

        fn facing(&self) -> f32 {
            self.world.resource::<Player>().facing()
        }

        fn camera(&self) -> (f32, f32) {
            self.world.get::<FlyCam>(self.camera).unwrap().aim()
        }

        fn parked(&self) -> (f32, f32) {
            self.world.resource::<CameraControl>().parked()
        }

        fn pending(&self) -> bool {
            self.world.contains_resource::<HostFaceTarget>()
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// A target due east (+X, a quarter turn clockwise from -Z): One turns -90°, the camera goes
    /// behind it on the same yaw and a little down at the ordinary 9 yd (no size known), and the
    /// turn and the distance are told.
    #[test]
    fn the_character_turns_to_the_target_and_the_camera_goes_behind_it() {
        let mut rig = Rig::new();
        rig.select(Vec3::new(3.0, 0.5, 0.0));
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        assert!(close(rig.facing(), -FRAC_PI_2), "facing {}", rig.facing());
        let (yaw, pitch) = rig.camera();
        assert!(close(yaw, -FRAC_PI_2) && close(pitch, FRAME_PITCH), "camera {yaw} {pitch}");
        assert_eq!(rig.parked(), (FRAME_DISTANCE, FRAME_DISTANCE));
        let told = rig.told();
        assert!(
            matches!(told.as_slice(), [HostFacedTarget::Turned {
                guid: GUID, degrees, distance, camera_distance, camera_pitch,
                target_size: TargetSize::Unknown,
            }] if close(*degrees, -90.0) && close(*distance, 3.0)
                && *camera_distance == FRAME_DISTANCE && *camera_pitch == FRAME_PITCH),
            "{told:?}"
        );
        assert!(!rig.pending(), "the request is consumed");
    }

    /// A giant whose model the client drew (a 4.7 yd Stand box at scale 3, so 14.1 yd tall) is
    /// framed by its model: the camera pulls back past the zoom slider's ceiling, tilts up, and
    /// the host is told the numbers it framed with.
    #[test]
    fn a_giant_is_framed_by_its_drawn_model() {
        let mut rig = Rig::new();
        let giant = rig.select(Vec3::new(0.0, 0.0, -7.0));
        rig.world.entity_mut(giant).insert((
            StandBoxHeight(4.7),
            Transform::from_translation(Vec3::new(0.0, 0.0, -7.0)).with_scale(Vec3::splat(3.0)),
        ));
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        let told = rig.told();
        let [HostFacedTarget::Turned {
            camera_distance,
            camera_pitch,
            target_size: TargetSize::Model { height },
            ..
        }] = told.as_slice()
        else {
            panic!("{told:?}");
        };
        assert!((height - 14.1).abs() < 1e-3, "height {height}");
        assert!(*camera_distance > 15.0 && *camera_distance < FRAME_DISTANCE_CAP, "{told:?}");
        assert!(*camera_pitch > FRAME_PITCH && *camera_pitch <= FRAME_PITCH_LEVEL, "{told:?}");
        assert_eq!(rig.parked(), (*camera_distance, *camera_distance));
        assert!(close(rig.camera().1, *camera_pitch));
    }

    /// The facing that goes on the wire is the one the controller walks along: after the turn,
    /// the forward vector points at the target.
    #[test]
    fn the_new_facing_walks_toward_the_target() {
        let mut rig = Rig::new();
        let at = Vec3::new(-4.0, 0.0, 7.0);
        rig.select(at);
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        let forward = Quat::from_rotation_y(rig.facing()) * Vec3::NEG_Z;
        let want = Vec3::new(at.x, 0.0, at.z).normalize();
        assert!(forward.distance(want) < 1e-4, "{forward} vs {want}");
    }

    /// The short way round: from yaw 3.0 to a target at yaw -3.0 is a turn of 2π - 6, not -6.
    #[test]
    fn the_turn_goes_the_short_way_round() {
        let mut rig = Rig::new();
        rig.world.resource_mut::<Player>().turn_aim(3.0);
        let bearing = -3.0f32;
        rig.select(Vec3::new(-bearing.sin(), 0.0, -bearing.cos()) * 5.0);
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        let told = rig.told();
        let want = (std::f32::consts::TAU - 6.0).to_degrees();
        assert!(
            matches!(told.as_slice(), [HostFacedTarget::Turned { degrees, .. }] if close(*degrees, want)),
            "{told:?}"
        );
    }

    /// Nothing selected: no turn, the camera untouched, and the host is told so -- once.
    #[test]
    fn no_target_is_told_and_nothing_moves() {
        let mut rig = Rig::new();
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        assert_eq!(rig.told(), vec![HostFacedTarget::NoTarget]);
        assert!(close(rig.facing(), 0.0));
        assert_eq!(rig.camera(), (2.0, -0.45));
        assert!(!rig.pending());
        rig.frame();
        assert!(rig.told().is_empty());
    }

    /// Before the world: the request waits, silently.
    #[test]
    fn out_of_the_world_the_request_waits() {
        let mut rig = Rig::new();
        let me = rig
            .world
            .query_filtered::<Entity, With<SelfPlayer>>()
            .single(&rig.world)
            .unwrap();
        rig.world.despawn(me);
        rig.select(Vec3::new(3.0, 0.0, 0.0));
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        assert!(rig.pending());
        assert!(rig.told().is_empty());
    }

    // ---- the framing's geometry ----

    /// A human's neck, the camera's fallback pivot.
    const PIVOT: f32 = 1.8;

    fn scene(height: f32, ahead: f32) -> Option<Scene> {
        Some(Scene {
            height,
            ahead,
            rise: 0.0,
            pivot: PIVOT,
        })
    }

    /// Where a point `y` high and `x` ahead of the character sits in the frame of a camera at
    /// `(d, p)`: its angle off the view axis (radians, `+` = up).
    fn off_axis(d: f32, p: f32, s: Scene, y: f32, x: f32) -> f32 {
        let cam_y = s.pivot - d * p.sin();
        let back = d * p.cos();
        (y - cam_y).atan2(back + x) - p
    }

    /// The target feet to head and the character's feet all inside the frame, margin kept.
    fn fits(d: f32, p: f32, s: Scene) -> bool {
        let edge = CAM_FOVY * 0.5 - FRAME_MARGIN + 1e-4;
        [
            off_axis(d, p, s, s.rise + s.height, s.ahead),
            off_axis(d, p, s, s.rise, s.ahead),
            off_axis(d, p, s, 0.0, 0.0),
        ]
        .iter()
        .all(|a| a.abs() <= edge)
    }

    /// A normal humanoid (2 yd) in melee range, or a few yards off: exactly today's camera,
    /// 9 yd and the ordinary pitch.
    #[test]
    fn a_humanoid_is_framed_as_before() {
        for ahead in [0.5, 3.0, 5.0, 12.0] {
            assert_eq!(framing(scene(2.0, ahead)), (FRAME_DISTANCE, FRAME_PITCH), "{ahead} yd");
            let (d, p) = framing(scene(2.0, ahead));
            assert!(fits(d, p, scene(2.0, ahead).unwrap()), "{ahead} yd");
        }
    }

    /// A giant of 12-15 yd at melee range: further back than 9 yd, tilted up no further than
    /// level, under the cap -- and it fits, feet to head, with One's feet in the frame too.
    #[test]
    fn a_giant_is_framed_whole() {
        for (height, ahead) in [(12.0, 5.0), (14.0, 8.0), (15.0, 5.0), (15.0, 10.0)] {
            let s = scene(height, ahead).unwrap();
            let (d, p) = framing(Some(s));
            assert!(d > FRAME_DISTANCE && d < FRAME_DISTANCE_CAP, "{height} yd at {ahead}: {d}");
            assert!(p > FRAME_PITCH && p <= FRAME_PITCH_LEVEL, "{height} yd at {ahead}: {p}");
            assert!(fits(d, p, s), "{height} yd at {ahead}: {d} yd {p} rad");
            // Today's camera did not: the head was out of the frame.
            assert!(!fits(FRAME_DISTANCE, FRAME_PITCH, s), "{height} yd at {ahead}");
        }
    }

    /// The tilt pays: the pitch picked needs less distance than today's pitch would.
    #[test]
    fn the_tilt_shortens_the_pull_back() {
        let s = scene(14.0, 8.0).unwrap();
        let (d, _) = framing(Some(s));
        let mut untilted = FRAME_DISTANCE;
        while !fits(untilted, FRAME_PITCH, s) {
            untilted += 0.1;
        }
        assert!(d < untilted - 1.0, "{d} vs {untilted}");
    }

    /// A target on a ledge or in a pit is framed by where its feet really are.
    #[test]
    fn the_rise_of_the_ground_is_framed_too() {
        for rise in [-4.0, 4.0] {
            let s = Scene {
                rise,
                ..scene(6.0, 6.0).unwrap()
            };
            let (d, p) = framing(Some(s));
            assert!(fits(d, p, s), "rise {rise}: {d} yd {p} rad");
        }
    }

    /// The Anubisath Guardian of the proof run: 19.5 yd tall (its Stand box x 2.5), at melee range.
    /// The cap binds, the pitch is level, and the Guardian fits feet to head with most of the
    /// margin left -- the proof run's 40 yd cap cut its head off.
    #[test]
    fn a_boss_of_twenty_yards_fits_at_the_cap() {
        for ahead in [0.0, 3.8, 5.7] {
            let s = scene(19.5, ahead).unwrap();
            let (d, p) = framing(Some(s));
            assert!(d <= FRAME_DISTANCE_CAP && d > 45.0, "{ahead} yd: {d}");
            assert_eq!(p, FRAME_PITCH_LEVEL, "{ahead} yd");
            // Inside the frame with at least 0.05 rad (2.9 deg) to spare at the edge, feet to head.
            let edge = CAM_FOVY * 0.5 - 0.05;
            for (y, x) in [(s.height, ahead), (0.0, ahead), (0.0, 0.0)] {
                let a = off_axis(d, p, s, y, x);
                assert!(a.abs() <= edge, "{ahead} yd: point ({y}, {x}) at {a} rad");
            }
            // The tallest the Guardian's mesh ever reaches (8.6 model-local, x 2.5) still fits.
            assert!(off_axis(d, p, s, 8.6 * 2.5, ahead) < CAM_FOVY * 0.5, "{ahead} yd");
        }
        assert_eq!(framing(scene(19.5, 3.8)).0, FRAME_DISTANCE_CAP);
        // At the old 40 yd its head was out of the margin.
        assert!(!fits(40.0, 0.0, scene(19.5, 3.8).unwrap()));
    }

    /// Past what any boss needs, the cap holds -- the distance never goes further.
    /// No size known (or a size that is no size): today's camera.
    #[test]
    fn no_size_is_the_ordinary_framing() {
        assert_eq!(framing(None), (FRAME_DISTANCE, FRAME_PITCH));
        assert_eq!(framing(scene(0.0, 3.0)), (FRAME_DISTANCE, FRAME_PITCH));
        assert_eq!(framing(scene(f32::NAN, 3.0)), (FRAME_DISTANCE, FRAME_PITCH));
    }

    /// The model the client drew comes first, times the object scale; a box of zero (a model not
    /// loaded yet) falls to the combat reach, not scaled a second time; neither is unknown.
    #[test]
    fn the_size_is_read_from_the_model_then_the_reach() {
        assert_eq!(
            target_size(Some(2.0), 1.5, Some(9.0)),
            TargetSize::Model { height: 3.0 }
        );
        let TargetSize::CombatReach { height } = target_size(Some(0.0), 3.0, Some(1.5)) else {
            panic!();
        };
        assert!((height - crate::player::DEFAULT_COLLISION_HEIGHT).abs() < 1e-5, "{height}");
        assert!(matches!(target_size(None, 1.0, Some(6.0)),
            TargetSize::CombatReach { height } if (height - 4.0 * crate::player::DEFAULT_COLLISION_HEIGHT).abs() < 1e-4));
        assert_eq!(target_size(None, 1.0, None), TargetSize::Unknown);
        assert_eq!(target_size(Some(0.0), 1.0, Some(0.0)), TargetSize::Unknown);
    }
}
