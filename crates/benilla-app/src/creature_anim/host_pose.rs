//! **A host's hold on one unit's body animation** — an embedder's animation bench.
//!
//! [`super::PlayAnimation`] is the host's one-shot: it rides the road `/wave` takes, plays through,
//! and hands the unit back. A bench that inspects a model needs three things a one-shot cannot
//! carry: a clip **held on a loop**, a clip **held at a frame** and dragged through by hand, and a
//! read-back of **where in the clip** the body is. None of those is a server's to say — there is no
//! wire message for "freeze this orc at 40 % of its roar" — so this is a host seam, like
//! `PlayAnimation`, and never a `SessionEvent`.
//!
//! **While a [`HostPose`] is on a unit, the animation driver leaves that unit alone**
//! ([`super::driver::drive_animations`] filters it out): two writers on one `AnimationPlayer` is a
//! body that twitches between them every frame. Removing it hands the unit back, and the driver
//! re-selects from scratch — a fresh gait, as on the unit's first frame — with the sheath and
//! mount state it already had, so the hand-back is not mistaken for a draw or a mount.
//!
//! **What the hold costs**: for its duration the driver's per-clip laws do not run on the unit —
//! the sheath reconcile that stows a staff for a clip whose `WeaponFlags` say so, the wound
//! flinch, the combat fast path. The bench shows the model's clip, not the game's reaction to it.

use std::time::Duration;

use benilla_assets::ModelAnimations;
use bevy::animation::RepeatAnimation;
use bevy::prelude::*;

use super::{AnimData, AnimDriver};

/// A host's request: play `anim_id` on this unit, and keep playing or holding it until removed.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct HostPose {
    /// The `AnimationData.dbc` id asked for. The model's own `PlayableAnimationLookup` resolves it
    /// ([`ModelAnimations::resolve`]), as it does for every other request — a model that authors
    /// no such clip plays its own substitute rather than nothing.
    pub anim_id: u16,
    /// Loop it, or play it once and stop on its last frame.
    pub repeat: bool,
    /// `Some(fraction)` holds the clip at that point of its length, paused (a scrub); `None` lets
    /// it run.
    pub hold_at: Option<f32>,
    /// Which press this is. A change restarts the clip from its first frame, so pressing play on
    /// the clip already held plays it again rather than doing nothing.
    pub take: u32,
}

/// What the held clip is doing, written back every frame a [`HostPose`] is on the unit.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq)]
pub struct HostPoseNow {
    /// The id the model resolved the request to; `None` when it has no clip for it at all.
    pub resolved: Option<u16>,
    /// 0.0..=1.0 through the clip.
    pub fraction: f32,
    /// The clip's length in seconds; 0 for a sequence that keys no frames.
    pub length: f32,
    pub paused: bool,
    /// A clip played once has reached its end.
    pub finished: bool,
    /// The [`HostPose::take`] this reading is of.
    pub take: u32,
}

/// The cross-fade into a held clip: the client's own fade-to-rest window, so taking the body looks
/// like any other animation change rather than a cut.
const TAKE_BLEND: Duration = Duration::from_millis(150);

/// Play, hold and read back every unit a host is holding.
pub(crate) fn drive_host_pose(
    mut commands: Commands,
    mut units: Query<(
        Entity,
        Ref<HostPose>,
        &ModelAnimations,
        &mut AnimationPlayer,
        &mut AnimationTransitions,
        &mut AnimDriver,
        Option<&HostPoseNow>,
    )>,
    anim_data: Option<Res<AnimData>>,
) {
    for (entity, pose, anims, mut player, mut transitions, mut driver, now) in &mut units {
        let resolved = anim_data
            .as_deref()
            .map_or(pose.anim_id, |d| anims.resolve(pose.anim_id, &d.0).id);
        let Some(clip) = anims.pick_variation(resolved, 0) else {
            let none = HostPoseNow {
                take: pose.take,
                finished: true,
                ..default()
            };
            if now != Some(&none) {
                commands.entity(entity).insert(none);
            }
            continue;
        };
        let (node, length) = (clip.node, clip.duration);
        if pose.is_added() {
            // What the driver had layered over the base keeps no owner once it stands down, so it
            // goes with the take rather than freezing mid-fade on the held clip.
            for layered in [
                driver.overlay.take().map(|o| o.node),
                driver.overlay_fade.take().and_then(|f| f.out),
                driver.wound.take().map(|w| w.node),
            ]
            .into_iter()
            .flatten()
            {
                player.stop(layered);
            }
        }
        let restart = now.is_none_or(|n| n.take != pose.take);
        if restart || transitions.get_main_animation() != Some(node) {
            transitions.play(&mut player, node, TAKE_BLEND).replay();
        }
        if pose.is_changed() || restart {
            if let Some(active) = player.animation_mut(node) {
                active.set_repeat(if pose.repeat {
                    RepeatAnimation::Forever
                } else {
                    RepeatAnimation::Never
                });
                match pose.hold_at {
                    Some(fraction) => {
                        active.seek_to(fraction.clamp(0.0, 1.0) * length);
                        active.pause();
                    }
                    None => {
                        active.resume();
                    }
                }
            }
        }
        let fresh = player.animation(node).map_or(
            HostPoseNow {
                resolved: Some(resolved),
                length,
                finished: true,
                take: pose.take,
                ..default()
            },
            |a| HostPoseNow {
                resolved: Some(resolved),
                fraction: if length > 0.0 {
                    (a.seek_time() / length).clamp(0.0, 1.0)
                } else {
                    0.0
                },
                length,
                paused: a.is_paused(),
                finished: a.is_finished(),
                take: pose.take,
            },
        );
        if now != Some(&fresh) {
            commands.entity(entity).insert(fresh);
        }
    }
}

/// A removed [`HostPose`] hands the unit back to the driver, which re-selects from scratch.
///
/// Everything transient goes — the mode, the gait, anything layered — and the three facts about the
/// unit rather than about an animation stay: its sheath state and the sheath byte it was read
/// from, and its mount display, whose change is an edge the driver would otherwise answer with a
/// `Mount` flourish nobody asked for.
pub(crate) fn release_host_pose(
    mut commands: Commands,
    mut removed: RemovedComponents<HostPose>,
    mut drivers: Query<&mut AnimDriver>,
) {
    for entity in removed.read() {
        if let Ok(mut driver) = drivers.get_mut(entity) {
            let (sheath_cur, sheath_byte, mount_display) =
                (driver.sheath_cur, driver.sheath_byte, driver.mount_display);
            *driver = AnimDriver {
                sheath_cur,
                sheath_byte,
                mount_display,
                ..AnimDriver::default()
            };
        }
        if let Ok(mut unit) = commands.get_entity(entity) {
            unit.remove::<HostPoseNow>();
        }
    }
}
