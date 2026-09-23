//! **A host's ground pick** — the targeting reticle, asked for by an embedder rather than a spell.
//!
//! An embedder placing creatures into a world (trt's placement mode) needs exactly the thing a
//! ground-targeted spell already has: a mark that follows the cursor across the terrain, and a
//! click that says *there*. Drawing a second mark would re-derive the terrain hit, the draping and
//! the fade the [`super::reticle`] already transcribes; arming a spell to get one would send a
//! `CMSG_CAST_SPELL`, start a GCD and put a cast on the wire that nobody made.
//!
//! So this is its own small mode beside [`crate::ui_action::SpellTargeting`], sharing its surface
//! and none of its consequences:
//!
//! - [`HostGroundPick`] present = the mode is up: the reticle draws at its radius, always in range
//!   (a host's pick has no spell to be out of range of), and a world click does not select.
//! - A left click on the terrain writes [`HostGroundPicked`] with the point, in WoW coordinates,
//!   and takes the mode down — one pick per arming. A click on the sky does nothing and keeps it.
//! - A right press takes it down with no pick, as it does a spell's.

use bevy::prelude::*;

use benilla_assets::coords::bevy_to_wow;
use benilla_world::interact::{WorldClick, WorldRightPress};

/// The host's ground pick is up, with the reticle drawn at `radius` yards.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct HostGroundPick {
    pub radius: f32,
}

/// Where the host's pick landed, in WoW coordinates — the terrain point under the press.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct HostGroundPicked {
    pub at: [f32; 3],
}

/// A left click while the host's pick is up: the point, and the mode down.
///
/// The point is the press ray's, not this frame's — the same [`super::PressPick`] the spell's
/// ground commit reads, so a drag between press and release does not move the pick.
pub(super) fn commit_host_pick_on_click(
    mut commands: Commands,
    mut clicks: MessageReader<WorldClick>,
    press: Res<super::PressPick>,
    host: Option<Res<HostGroundPick>>,
    mut picked: MessageWriter<HostGroundPicked>,
) {
    if host.is_none() {
        clicks.clear();
        return;
    }
    if clicks.read().last().is_none() {
        return;
    }
    let Some(point) = press.occlusion.point else {
        return;
    };
    let at = bevy_to_wow(point);
    debug!(
        "host pick at wow ({:.2}, {:.2}, {:.2})",
        at[0], at[1], at[2]
    );
    picked.write(HostGroundPicked { at });
    commands.remove_resource::<HostGroundPick>();
}

/// A right press while the host's pick is up takes it down, picking nothing.
pub(super) fn cancel_host_pick_on_right_press(
    mut commands: Commands,
    mut presses: MessageReader<WorldRightPress>,
    host: Option<Res<HostGroundPick>>,
) {
    if host.is_none() {
        presses.clear();
        return;
    }
    if presses.read().last().is_some() {
        commands.remove_resource::<HostGroundPick>();
    }
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::SystemId;

    use super::*;

    fn world_with(point: Option<Vec3>) -> World {
        let mut world = World::new();
        world.init_resource::<Messages<WorldClick>>();
        world.init_resource::<Messages<HostGroundPicked>>();
        world.init_resource::<super::super::PressPick>();
        world.resource_mut::<super::super::PressPick>().occlusion = super::super::PickOcclusion {
            distance: 5.0,
            point,
        };
        world
    }

    fn click(world: &mut World, id: SystemId) {
        world.resource_mut::<Messages<WorldClick>>().write(WorldClick);
        world.run_system(id).expect("the host pick runs");
    }

    fn picked(world: &mut World) -> Vec<HostGroundPicked> {
        world
            .resource_mut::<Messages<HostGroundPicked>>()
            .drain()
            .collect()
    }

    /// **The click is the pick, in WoW coordinates, and one pick per arming.**
    #[test]
    fn a_click_while_armed_picks_the_ground_and_disarms() {
        let mut world = world_with(Some(Vec3::new(1.0, 2.0, 3.0)));
        let id = world.register_system(commit_host_pick_on_click);
        world.insert_resource(HostGroundPick { radius: 2.0 });
        click(&mut world, id);
        assert_eq!(
            picked(&mut world),
            [HostGroundPicked {
                at: bevy_to_wow(Vec3::new(1.0, 2.0, 3.0))
            }]
        );
        assert!(world.get_resource::<HostGroundPick>().is_none(), "disarmed");

        // Disarmed, a click picks nothing -- it is an ordinary click again.
        click(&mut world, id);
        assert!(picked(&mut world).is_empty());
    }

    /// **The sky is not a place**: a click with no ground under it keeps the mode up.
    #[test]
    fn a_click_on_the_sky_keeps_the_pick_armed() {
        let mut world = world_with(None);
        let id = world.register_system(commit_host_pick_on_click);
        world.insert_resource(HostGroundPick { radius: 2.0 });
        click(&mut world, id);
        assert!(picked(&mut world).is_empty());
        assert!(world.get_resource::<HostGroundPick>().is_some());
    }
}
