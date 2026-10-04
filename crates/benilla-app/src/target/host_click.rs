//! **A host's right-click** -- the click a player would make on a creature or gameobject, asked
//! for by an embedder that has no mouse.
//!
//! An embedder driving a scripted run (trt's test mode) needs to say "right-click the innkeeper"
//! and watch what the client does with it. The client's right-click is not one function: the
//! cursor classifier reads the hovered pick and decides the kind and the range gray, and
//! [`super::click::act_on_right_click`] runs the ladder off the latched [`PressPick`]. Re-deriving
//! either from outside would test a guess, so this feeds the real pair the one thing a mouse
//! feeds them -- a pick -- and lets everything downstream run untouched:
//!
//! - [`aim`], right after the object pick: resolve the pending [`HostRightClick`] to an entity
//!   and publish it as the frame's [`Hovered`] / [`HoveredObject`], as if the cursor were over it.
//! - the classifier runs on that pick, exactly as it would for a mouse;
//! - [`press`], right after the classifier: latch the pick and the cursor as [`PressPick`] (what
//!   the down edge does for a real press), send the [`WorldRightClick`] the release would, and
//!   tell the host what the cursor said ([`HostRightClicked`]).
//!
//! **A target that is not streamed yet stays pending, silently.** `.go creature N` teleports and
//! the object arrives frames or seconds later (the same lag [`super::by_name::EmbedderSelection`]
//! is a standing want for), so "not found" is not a verdict this side can give: the request waits
//! and the host, which owns the clock, withdraws it by removing the resource if it gives up.
//! Nothing is emitted until the target resolves, and the request is consumed the frame it does.
//!
//! The click acts like the player's: it selects, and on a hostile it attacks. A service NPC out of
//! range is the player's out-of-range gray ([`HostClickOutcome::OutOfRange`]) -- no packet, no
//! auto-approach.

use bevy::prelude::*;

use benilla_protocol::guid;
use benilla_world::interact::WorldRightClick;

use super::by_name::ByNameScan;
use super::cursor_mode::{CursorKind, WorldCursor};
use super::{Hovered, HoveredObject, PickOcclusion, PressPick};
use crate::net::GuidIndex;

/// Which object a host's right-click lands on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostClickTarget {
    /// A creature by its **database** guid (`creature.guid`, the spawn row's id). The streamed
    /// guid carries the template entry in the middle 24 bits, which a spawn that picked among
    /// several `ChooseCreatureId` entries need not match, so only the low 24 bits are compared.
    Creature(u32),
    /// A gameobject by its database guid (`gameobject.guid`); the same low-24-bit match.
    Object(u32),
    /// A unit by exact name (case-insensitive); the nearest of several wins.
    Name(String),
    /// A streamed object by its full server guid.
    Guid(u64),
}

/// The host's standing request: right-click this. Present until the target is streamed and the
/// click is made, then removed by [`aim`]; a host that gives up removes it itself.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct HostRightClick(pub HostClickTarget);

/// What the cursor said about the thing that was clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostClickOutcome {
    /// The cursor was the plain pointer: there is no service, attack or use on this object.
    NoAction,
    /// The cursor was grayed -- a service or use out of reach. Nothing was sent.
    OutOfRange,
    /// The click ran its ladder; the cursor kind it ran on (`Speak`, `Attack`, `Interact`, ...).
    Acted(&'static str),
}

/// A [`HostRightClick`] was made: on which object, and what came of it.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostRightClicked {
    pub guid: Option<u64>,
    pub outcome: HostClickOutcome,
}

/// The object [`aim`] put under the "cursor" this frame, handed to [`press`] -- the two run in the
/// same frame with the classifier between them. Internal.
#[derive(Resource, Default)]
pub(super) struct HostClickAim(Option<u64>);

/// The creature (`HIGH_UNIT`) or gameobject (`HIGH_GAMEOBJECT`) in the index whose database guid
/// -- the low 24 bits -- is `db`, whatever entry its guid carries.
fn find_by_db_guid(index: &GuidIndex, high: u16, db: u32) -> Option<(Entity, u64)> {
    index
        .0
        .iter()
        .filter(|(&g, _)| guid::high(g) == high && (g & 0xFF_FFFF) == u64::from(db))
        // A guid is unique per db row, but a stale and a fresh spawn of one row may coexist
        // for a frame across a respawn: pick the same one every time.
        .min_by_key(|(&g, _)| g)
        .map(|(&g, &e)| (e, g))
}

/// Resolve the pending request and make its object the frame's pick.
pub(super) fn aim(
    mut commands: Commands,
    request: Option<Res<HostRightClick>>,
    index: Res<GuidIndex>,
    by_name: ByNameScan,
    mut hovered: ResMut<Hovered>,
    mut object: ResMut<HoveredObject>,
    mut aimed: ResMut<HostClickAim>,
) {
    let Some(request) = request else {
        return;
    };
    let found = match &request.0 {
        HostClickTarget::Creature(db) => find_by_db_guid(&index, guid::HIGH_UNIT, *db),
        HostClickTarget::Object(db) => find_by_db_guid(&index, guid::HIGH_GAMEOBJECT, *db),
        HostClickTarget::Name(name) => by_name.resolve_exact_quiet(name),
        HostClickTarget::Guid(g) => index.0.get(g).map(|&e| (e, *g)),
    };
    let Some((entity, g)) = found else {
        return; // not streamed (yet) -- pending, silent
    };
    commands.remove_resource::<HostRightClick>();
    debug!("host click: {:?} -> {g:#x}", request.0);
    // One pick, one slot: the unit slot or the gameobject slot, never both, as the real pick's
    // arbiter would have left them. Distance 0 -- there is no ray, and no competitor.
    if guid::is_gameobject(g) {
        *hovered = Hovered::default();
        *object = HoveredObject {
            target: Some(entity),
            guid: Some(g),
            distance: 0.0,
        };
    } else {
        *hovered = Hovered {
            target: Some(entity),
            guid: Some(g),
            distance: 0.0,
            ..Hovered::default()
        };
        *object = HoveredObject::default();
    }
    aimed.0 = Some(g);
}

/// Latch the classified pick as the press, send the click, and tell the host what the cursor said.
pub(super) fn press(
    mut aimed: ResMut<HostClickAim>,
    hovered: Res<Hovered>,
    object: Res<HoveredObject>,
    cursor: Res<WorldCursor>,
    mut latch: ResMut<PressPick>,
    mut clicks: MessageWriter<WorldRightClick>,
    mut told: MessageWriter<HostRightClicked>,
) {
    let Some(guid) = aimed.0.take() else {
        return;
    };
    *latch = PressPick {
        hovered: *hovered,
        object: *object,
        occlusion: PickOcclusion::default(),
        cursor: *cursor,
    };
    clicks.write(WorldRightClick);
    // Attack is never range-gated -- `unable` only grays the sword, and the server holds the swing
    // until we are in reach -- so a grayed Attack still acts.
    let outcome = if cursor.kind == CursorKind::Point {
        HostClickOutcome::NoAction
    } else if cursor.unable && cursor.kind != CursorKind::Attack {
        HostClickOutcome::OutOfRange
    } else {
        HostClickOutcome::Acted(cursor.kind.name())
    };
    debug!("host click on {guid:#x}: {outcome:?}");
    told.write(HostRightClicked {
        guid: Some(guid),
        outcome,
    });
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;

    use super::super::click::{act_on_right_click, tests as click_tests};
    use super::super::cursor_mode::{classify_cursor, npc_flags};
    use super::*;
    use crate::net::{ClientCommand, Guid, NetCommands, SelfPlayer};

    const F_HEALTH: u16 = 22;
    const F_MAXHEALTH: u16 = 28;
    const F_NPC_FLAGS: u16 = 147;
    const GO_TYPE_FIELD: u16 = 21;

    /// A streamed guid the way the server composes one: `counter | entry << 24 | high << 48`.
    fn compose(high: u16, entry: u32, counter: u32) -> u64 {
        u64::from(counter) | (u64::from(entry) << 24) | (u64::from(high) << 48)
    }

    /// The whole right-click path -- aim, the real classifier, press, the real click -- over one
    /// world, with the commands the click sent collected on `rx`.
    struct Rig {
        world: World,
        rx: crossbeam_channel::Receiver<ClientCommand>,
    }

    impl Rig {
        fn new() -> Self {
            let (tx, rx) = crossbeam_channel::unbounded::<ClientCommand>();
            let (mut world, boar) = click_tests::right_click_world();
            world.insert_resource(NetCommands(tx));
            world.despawn(boar);
            world.init_resource::<GuidIndex>();
            world.init_resource::<Hovered>();
            world.init_resource::<HoveredObject>();
            world.init_resource::<WorldCursor>();
            world.init_resource::<HostClickAim>();
            world.init_resource::<crate::names::NameCache>();
            world.init_resource::<crate::net::Reputations>();
            world.init_resource::<crate::ui_loot::LootConfig>();
            world.init_resource::<ButtonInput<KeyCode>>();
            world.init_resource::<Messages<HostRightClicked>>();
            // Us, standing at the origin with a body.
            let me = world
                .query_filtered::<Entity, With<SelfPlayer>>()
                .single(&world)
                .unwrap();
            world.entity_mut(me).insert((
                Transform::default(),
                click_tests::store(&[(F_HEALTH, 100), (F_MAXHEALTH, 100)]),
            ));
            Rig { world, rx }
        }

        /// A streamed object `yards` away from us, indexed as the net layer would.
        fn stream(&mut self, guid: u64, fields: &[(u16, u32)], yards: f32) -> Entity {
            let mut pairs = vec![(F_HEALTH, 100), (F_MAXHEALTH, 100)];
            pairs.extend_from_slice(fields);
            let e = self
                .world
                .spawn((
                    Guid(guid),
                    click_tests::store(&pairs),
                    Transform::from_xyz(yards, 0.0, 0.0),
                ))
                .id();
            self.world.resource_mut::<GuidIndex>().0.insert(guid, e);
            e
        }

        fn ask(&mut self, target: HostClickTarget) {
            self.world.insert_resource(HostRightClick(target));
        }

        /// One frame of the chain, in the order the plugin runs it.
        fn frame(&mut self) {
            self.world.run_system_once(aim).unwrap();
            self.world.run_system_once(classify_cursor).unwrap();
            self.world.run_system_once(press).unwrap();
            self.world.run_system_once(act_on_right_click).unwrap();
        }

        fn told(&mut self) -> Vec<HostRightClicked> {
            let mut messages = self.world.resource_mut::<Messages<HostRightClicked>>();
            messages.drain().collect()
        }

        /// What went on the wire, minus the `SetSelection` every click on a unit makes: a right
        /// click selects first, in range or not.
        fn sent(&self) -> Vec<ClientCommand> {
            self.rx
                .try_iter()
                .filter(|c| !matches!(c, ClientCommand::SetSelection { .. }))
                .collect()
        }

        fn pending(&self) -> bool {
            self.world.contains_resource::<HostRightClick>()
        }
    }

    /// A GOSSIP unit in reach: the click is the player's -- the cursor says Speak, and the click
    /// sends `CMSG_GOSSIP_HELLO`. The creature is asked for by its DB guid, and its streamed guid
    /// carries an entry nobody asked about.
    #[test]
    fn a_gossip_unit_in_range_is_greeted() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_UNIT, 6929, 2556037);
        rig.stream(g, &[(F_NPC_FLAGS, npc_flags::GOSSIP)], 3.0);
        rig.ask(HostClickTarget::Creature(2556037));
        rig.frame();
        assert_eq!(
            rig.told(),
            vec![HostRightClicked {
                guid: Some(g),
                outcome: HostClickOutcome::Acted("Speak")
            }]
        );
        assert!(matches!(
            rig.sent().as_slice(),
            [ClientCommand::GossipHello { guid }] if *guid == g
        ));
        assert!(!rig.pending(), "the request is consumed once it lands");
    }

    /// A QUESTGIVER-only unit with a quest on offer: Speak, and the questgiver hello.
    #[test]
    fn a_questgiver_with_an_offer_gets_the_questgiver_hello() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_UNIT, 197, 77);
        rig.stream(g, &[(F_NPC_FLAGS, npc_flags::QUESTGIVER)], 3.0);
        rig.world
            .resource_mut::<crate::ui_quest::QuestGiver>()
            .set_status(g, benilla_protocol::messages::dialog_status::AVAILABLE);
        rig.ask(HostClickTarget::Guid(g));
        rig.frame();
        assert_eq!(rig.told()[0].outcome, HostClickOutcome::Acted("Speak"));
        assert!(matches!(
            rig.sent().as_slice(),
            [ClientCommand::QuestgiverHello { npc }] if *npc == g
        ));
    }

    /// The same kind of unit beyond service range: the cursor is grayed, so the host is told so
    /// and nothing goes out -- there is no auto-approach.
    #[test]
    fn a_unit_beyond_service_range_is_out_of_range_and_sends_nothing() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_UNIT, 6929, 5);
        rig.stream(g, &[(F_NPC_FLAGS, npc_flags::GOSSIP)], 60.0);
        rig.ask(HostClickTarget::Creature(5));
        rig.frame();
        assert_eq!(
            rig.told(),
            vec![HostRightClicked {
                guid: Some(g),
                outcome: HostClickOutcome::OutOfRange
            }]
        );
        assert!(rig.sent().is_empty());
    }

    /// A unit with no service on it (and nothing to attack): the pointer, so nothing to do.
    #[test]
    fn a_unit_with_no_service_has_no_action() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_UNIT, 1, 9);
        rig.stream(g, &[], 2.0);
        rig.ask(HostClickTarget::Creature(9));
        rig.frame();
        assert_eq!(
            rig.told(),
            vec![HostRightClicked {
                guid: Some(g),
                outcome: HostClickOutcome::NoAction
            }]
        );
    }

    /// A gameobject goes through its own slot: `CMSG_GAMEOBJ_USE` for a plain use, by DB guid.
    #[test]
    fn a_gameobject_in_reach_is_used() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_GAMEOBJECT, 2061, 12345);
        // Type 0 (a door) is the gear cursor and a plain use.
        rig.stream(g, &[(GO_TYPE_FIELD, 0)], 2.0);
        rig.ask(HostClickTarget::Object(12345));
        rig.frame();
        let told = rig.told();
        assert_eq!(told[0].guid, Some(g));
        assert_eq!(told[0].outcome, HostClickOutcome::Acted("Interact"));
        assert!(matches!(
            rig.sent().as_slice(),
            [ClientCommand::GameObjUse { guid }] if *guid == g
        ));
        assert!(
            rig.world.resource::<Hovered>().target.is_none(),
            "a gameobject click leaves the unit slot empty"
        );
    }

    /// The database guid is the low 24 bits and nothing else: another family's high bits and a
    /// different counter do not match, whatever entry rides the middle.
    #[test]
    fn the_db_guid_match_ignores_the_entry_and_the_family() {
        let mut rig = Rig::new();
        let wrong_family = compose(guid::HIGH_GAMEOBJECT, 6929, 42);
        let wrong_counter = compose(guid::HIGH_UNIT, 6929, 43);
        let right = compose(guid::HIGH_UNIT, 1234, 42);
        rig.stream(wrong_family, &[], 1.0);
        rig.stream(wrong_counter, &[], 1.0);
        rig.ask(HostClickTarget::Creature(42));
        rig.frame();
        assert!(rig.told().is_empty());
        assert!(rig.pending(), "nothing matched, so the request waits");
        rig.stream(right, &[], 1.0);
        rig.frame();
        assert_eq!(rig.told()[0].guid, Some(right));
    }

    /// An unstreamed target is not a verdict: nothing is told and nothing is sent, the request
    /// stays, and the host withdrawing it ends the wait.
    #[test]
    fn an_unstreamed_target_stays_pending_and_silent_until_withdrawn() {
        let mut rig = Rig::new();
        rig.ask(HostClickTarget::Guid(compose(guid::HIGH_UNIT, 1, 1)));
        for _ in 0..3 {
            rig.frame();
        }
        assert!(rig.told().is_empty());
        assert!(rig.sent().is_empty());
        assert!(rig.pending());
        rig.world.remove_resource::<HostRightClick>();
        rig.frame();
        assert!(rig.told().is_empty());
    }

    /// A name resolves to the nearest exact match, case-insensitively; a prefix is not a match.
    #[test]
    fn a_name_is_an_exact_nearest_match() {
        let mut rig = Rig::new();
        let near = compose(guid::HIGH_UNIT, 6929, 1);
        let far = compose(guid::HIGH_UNIT, 6929, 2);
        let flags = [(F_NPC_FLAGS, npc_flags::GOSSIP)];
        rig.stream(near, &flags, 4.0);
        rig.stream(far, &flags, 30.0);
        let record = crate::names::CreatureRecord {
            name: "Innkeeper Farley".into(),
            subname: None,
            creature_type: 7,
            pet_family: 0,
            rank: 0,
            type_flags: 0,
            civilian: false,
            racial_leader: false,
            display_id: 0,
        };
        rig.world
            .resource_mut::<crate::names::NameCache>()
            .insert_creature(6929, Some(record));
        rig.ask(HostClickTarget::Name("Innkeeper".into()));
        rig.frame();
        assert!(rig.told().is_empty(), "a prefix is not the name");
        rig.ask(HostClickTarget::Name("innkeeper farley".into()));
        rig.frame();
        assert_eq!(rig.told()[0].guid, Some(near));
    }

    /// A grayed Attack still acts: the sword is never range-gated, only grayed, so the host is
    /// not told "out of range" about a swing the click makes.
    #[test]
    fn a_grayed_attack_still_acts() {
        let mut world = World::new();
        world.init_resource::<Hovered>();
        world.init_resource::<HoveredObject>();
        world.init_resource::<PressPick>();
        world.init_resource::<HostClickAim>();
        world.init_resource::<Messages<WorldRightClick>>();
        world.init_resource::<Messages<HostRightClicked>>();
        world.insert_resource(WorldCursor {
            kind: CursorKind::Attack,
            unable: true,
        });
        world.resource_mut::<HostClickAim>().0 = Some(7);
        world.run_system_once(press).unwrap();
        let told: Vec<_> = world
            .resource_mut::<Messages<HostRightClicked>>()
            .drain()
            .collect();
        assert_eq!(told[0].outcome, HostClickOutcome::Acted("Attack"));
    }
}
