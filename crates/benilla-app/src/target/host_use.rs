//! **A host's gameobject use** -- `CMSG_GAMEOBJ_USE` sent without a click, for a test lab.
//!
//! [`super::host_click`] feeds the real classifier, and the classifier is faithful to the 1.12
//! client: a gameobject with `GO_FLAG_INTERACT_COND` and no `GO_DYNFLAG_ACTIVATE` is not
//! highlightable, so it gets no cursor and no right-click use -- nothing is sent. The server
//! still accepts the use from a GM (the flag is only a client hint), so a host that plays the
//! GM's hand needs the use itself, with no classifier and no range gate in front of it: the
//! server judges the distance. This is **not** the player's click; it is for a lab that drives
//! objects the client would grey out.
//!
//! As with the click, **an object that is not streamed yet stays pending, silently**; the host,
//! which owns the clock, withdraws the request by removing the resource if it gives up.

use bevy::prelude::*;

use benilla_protocol::guid;

use super::host_click::find_by_db_guid;
use crate::net::{ClientCommand, GuidIndex, NetCommands};

/// The host's standing request: use this gameobject, by its **database** guid
/// (`gameobject.guid`). Present until the object is streamed and the use is sent, then removed
/// by [`use_object`]; a host that gives up removes it itself.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostUseObject(pub u32);

/// A [`HostUseObject`] was sent, for this streamed object guid.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostUsedObject {
    pub guid: u64,
}

/// Resolve the pending request and send the use.
pub(super) fn use_object(
    mut commands: Commands,
    request: Option<Res<HostUseObject>>,
    index: Res<GuidIndex>,
    net: Res<NetCommands>,
    mut used: MessageWriter<HostUsedObject>,
) {
    let Some(request) = request else {
        return;
    };
    let Some((_, g)) = find_by_db_guid(&index, guid::HIGH_GAMEOBJECT, request.0) else {
        return; // not streamed (yet) -- pending, silent
    };
    commands.remove_resource::<HostUseObject>();
    debug!("host use: gameobject {} -> {g:#x}", request.0);
    let _ = net.0.send(ClientCommand::GameObjUse { guid: g });
    used.write(HostUsedObject { guid: g });
}

#[cfg(test)]
mod tests {
    use bevy::ecs::system::RunSystemOnce;

    use super::*;

    /// A streamed guid the way the server composes one: `counter | entry << 24 | high << 48`.
    fn compose(high: u16, entry: u32, counter: u32) -> u64 {
        u64::from(counter) | (u64::from(entry) << 24) | (u64::from(high) << 48)
    }

    struct Rig {
        world: World,
        rx: crossbeam_channel::Receiver<ClientCommand>,
    }

    impl Rig {
        fn new() -> Self {
            let (tx, rx) = crossbeam_channel::unbounded::<ClientCommand>();
            let mut world = World::new();
            world.insert_resource(NetCommands(tx));
            world.init_resource::<GuidIndex>();
            world.init_resource::<Messages<HostUsedObject>>();
            Rig { world, rx }
        }

        fn stream(&mut self, guid: u64) {
            let e = self.world.spawn_empty().id();
            self.world.resource_mut::<GuidIndex>().0.insert(guid, e);
        }

        fn frame(&mut self) {
            self.world.run_system_once(use_object).unwrap();
        }

        fn told(&mut self) -> Vec<HostUsedObject> {
            let mut messages = self.world.resource_mut::<Messages<HostUsedObject>>();
            messages.drain().collect()
        }

        fn pending(&self) -> bool {
            self.world.contains_resource::<HostUseObject>()
        }
    }

    /// Not streamed yet: nothing is sent, nothing is told, and the request waits.
    #[test]
    fn an_object_not_streamed_stays_pending() {
        let mut rig = Rig::new();
        rig.world.insert_resource(HostUseObject(12345));
        rig.frame();
        assert!(rig.pending());
        assert!(rig.told().is_empty());
        assert!(rig.rx.try_recv().is_err());
    }

    /// Streamed: one `CMSG_GAMEOBJ_USE` for the full guid (its entry is nobody's business), one
    /// message, and the request is consumed -- a second frame sends nothing more.
    #[test]
    fn a_streamed_object_is_used_once() {
        let mut rig = Rig::new();
        let g = compose(guid::HIGH_GAMEOBJECT, 177807, 12345);
        rig.stream(g);
        rig.world.insert_resource(HostUseObject(12345));
        rig.frame();
        assert_eq!(rig.told(), vec![HostUsedObject { guid: g }]);
        assert!(matches!(
            rig.rx.try_iter().collect::<Vec<_>>().as_slice(),
            [ClientCommand::GameObjUse { guid }] if *guid == g
        ));
        assert!(!rig.pending(), "the request is consumed once it is sent");
        rig.frame();
        assert!(rig.told().is_empty());
        assert!(rig.rx.try_recv().is_err());
    }

    /// A unit with the same low 24 bits is not a gameobject: the use never goes to it.
    #[test]
    fn a_unit_is_never_used() {
        let mut rig = Rig::new();
        rig.stream(compose(guid::HIGH_UNIT, 6929, 12345));
        rig.world.insert_resource(HostUseObject(12345));
        rig.frame();
        assert!(rig.pending());
        assert!(rig.told().is_empty());
        assert!(rig.rx.try_recv().is_err());
    }
}
