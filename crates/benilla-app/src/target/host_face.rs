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
//! facing, a little downward, at [`FRAME_DISTANCE`] -- [`FlyCam::park`] and
//! [`CameraControl::park_distance`], the scripted camera park's levers.
//!
//! Unlike a click or a use, the request never waits: there is a selection now or there is not, so
//! it is consumed on the first in-world frame and answered with [`HostFacedTarget`] either way.

use bevy::prelude::*;

use super::Selection;
use crate::net::SelfPlayer;
use crate::player::camera::FlyCam;
use crate::player::{CameraControl, Player};

/// The camera's pitch after the turn (radians, `+` = up): a little downward, so a target in melee
/// range stands in the frame whole, not just its feet -- world entry's `-0.45` looks at the dirt.
const FRAME_PITCH: f32 = -0.2;

/// The camera's distance behind the character after the turn (yards): the character and a
/// target in melee range in one frame.
const FRAME_DISTANCE: f32 = 9.0;

/// The host's standing request: turn to the current target. Removed by [`face_target`] on the
/// first frame the character is in the world, whatever the answer.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostFaceTarget;

/// What a [`HostFaceTarget`] did.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub enum HostFacedTarget {
    /// The character turned to the selected unit: `degrees` is the turn made (counter-clockwise
    /// seen from above is positive, `0.0` when it already faced it), `distance` the yards on the
    /// ground to the target.
    Turned { guid: u64, degrees: f32, distance: f32 },
    /// Nothing is selected (or the selection is not streamed): no turn, the camera untouched.
    NoTarget,
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
    transforms: Query<&Transform>,
    mut player: ResMut<Player>,
    mut rig: ResMut<CameraControl>,
    mut cam: Query<&mut FlyCam, With<benilla_world::view::WorldCamera>>,
    self_player: Query<(), With<SelfPlayer>>,
    mut faced: MessageWriter<HostFacedTarget>,
) {
    if request.is_none() || self_player.is_empty() {
        return; // nothing asked, or not in the world yet -- pending, silent
    }
    commands.remove_resource::<HostFaceTarget>();
    let target = selection
        .target
        .zip(selection.guid)
        .and_then(|(e, g)| transforms.get(e).ok().map(|t| (g, t.translation)));
    let Some((guid, at)) = target else {
        debug!("host face: no target");
        faced.write(HostFacedTarget::NoTarget);
        return;
    };
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
    if let Ok(mut cam) = cam.single_mut() {
        cam.park(facing, FRAME_PITCH);
    }
    rig.park_distance(FRAME_DISTANCE);
    debug!(
        "host face: {guid:#x} -> turned {:.1} deg ({distance:.1} yd)",
        turn.to_degrees()
    );
    faced.write(HostFacedTarget::Turned {
        guid,
        degrees: turn.to_degrees(),
        distance,
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

        fn select(&mut self, at: Vec3) {
            let e = self.world.spawn(Transform::from_translation(at)).id();
            let mut selection = self.world.resource_mut::<Selection>();
            selection.target = Some(e);
            selection.guid = Some(GUID);
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

        fn pending(&self) -> bool {
            self.world.contains_resource::<HostFaceTarget>()
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    /// A target due east (+X, a quarter turn clockwise from -Z): One turns -90°, the camera goes
    /// behind it on the same yaw and a little down, and the turn and the distance are told.
    #[test]
    fn the_character_turns_to_the_target_and_the_camera_goes_behind_it() {
        let mut rig = Rig::new();
        rig.select(Vec3::new(3.0, 0.5, 0.0));
        rig.world.insert_resource(HostFaceTarget);
        rig.frame();
        assert!(close(rig.facing(), -FRAC_PI_2), "facing {}", rig.facing());
        let (yaw, pitch) = rig.camera();
        assert!(close(yaw, -FRAC_PI_2) && close(pitch, FRAME_PITCH), "camera {yaw} {pitch}");
        let told = rig.told();
        assert!(
            matches!(told.as_slice(), [HostFacedTarget::Turned { guid: GUID, degrees, distance }]
                if close(*degrees, -90.0) && close(*distance, 3.0)),
            "{told:?}"
        );
        assert!(!rig.pending(), "the request is consumed");
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
}
