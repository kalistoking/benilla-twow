//! **The embedder's wire tap** -- [`HostWire`], one message per spell, sound or swing-refusal
//! packet, written from the drain's own events before any arm runs, so no gate an arm has (a
//! streamed-or-not lookup, a cast-time branch, a music slot that already holds the kit) can hide
//! a packet from the host. Read-only: no arm reads it back, and a message nobody reads is dropped
//! by Bevy.

use benilla_protocol::SessionEvent;
use bevy::prelude::*;

use super::super::{HostSoundKind, HostWire};

/// What the host is told of one event, if it is one the tap carries.
fn wire_of(ev: &SessionEvent) -> Option<HostWire> {
    Some(match ev {
        SessionEvent::SpellStart {
            caster,
            spell_id,
            target,
            ..
        } => HostWire::SpellStart {
            caster: *caster,
            spell_id: *spell_id,
            target: *target,
        },
        SessionEvent::SpellGo {
            caster,
            spell_id,
            hits,
            misses,
            ..
        } => HostWire::SpellGo {
            caster: *caster,
            spell_id: *spell_id,
            hits: hits.clone(),
            misses: misses.clone(),
        },
        // Our own failures only: `SMSG_CAST_RESULT` goes to the caster, and a success is the
        // cast's own start, which `SpellStart` already tells. No reason byte reads as `255`.
        SessionEvent::CastResult {
            spell_id,
            success: false,
            reason,
            ..
        } => HostWire::CastFailed {
            spell_id: *spell_id,
            reason: reason.unwrap_or(u8::MAX),
        },
        SessionEvent::PlaySound { sound_id } => HostWire::Sound {
            kind: HostSoundKind::Sound2d,
            sound_id: *sound_id,
            source: None,
        },
        SessionEvent::PlayMusic { music_id } => HostWire::Sound {
            kind: HostSoundKind::Music,
            sound_id: *music_id,
            source: None,
        },
        SessionEvent::PlayObjectSound { sound_id, guid } => HostWire::Sound {
            kind: HostSoundKind::Object,
            sound_id: *sound_id,
            source: Some(*guid),
        },
        SessionEvent::AttackSwingError(e) => HostWire::SwingRefused(*e),
        _ => return None,
    })
}

/// Tell the host every spell and sound packet of this run of the drain, in packet order.
pub(super) fn tap(events: &[SessionEvent], out: &mut MessageWriter<HostWire>) {
    for ev in events {
        if let Some(wire) = wire_of(ev) {
            out.write(wire);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASTER: u64 = 0x0000_0000_0000_0001;
    const ECTOPLASM: u64 = 0xF130_0000_9500_0007;

    fn go(misses: Vec<(u64, u8)>, hits: Vec<u64>) -> SessionEvent {
        SessionEvent::SpellGo {
            caster: CASTER,
            spell_id: 116,
            cast_flags: 0,
            hits,
            misses,
            target: Some(ECTOPLASM),
            go_target: None,
            dest: None,
            ammo_display_id: None,
            item_caster: None,
        }
    }

    /// The point of the tap: a launch whose only target is immune reaches the host with the
    /// miss code and the raw guid, though the client's own arms turn it into a floating word.
    #[test]
    fn a_spell_go_carries_its_hits_and_its_miss_codes_raw() {
        assert_eq!(
            wire_of(&go(vec![(ECTOPLASM, 7)], vec![])),
            Some(HostWire::SpellGo {
                caster: CASTER,
                spell_id: 116,
                hits: vec![],
                misses: vec![(ECTOPLASM, 7)],
            })
        );
        assert_eq!(
            wire_of(&go(vec![], vec![CASTER])),
            Some(HostWire::SpellGo {
                caster: CASTER,
                spell_id: 116,
                hits: vec![CASTER],
                misses: vec![],
            })
        );
    }

    #[test]
    fn a_spell_start_carries_its_caster_and_target() {
        let ev = SessionEvent::SpellStart {
            caster: CASTER,
            spell_id: 10917,
            cast_flags: 2,
            cast_time_ms: 1500,
            target: Some(CASTER),
            ammo_display_id: None,
        };
        assert_eq!(
            wire_of(&ev),
            Some(HostWire::SpellStart {
                caster: CASTER,
                spell_id: 10917,
                target: Some(CASTER),
            })
        );
    }

    /// Only a failure is a `CastFailed`; a success is the cast starting, which has its own line.
    #[test]
    fn only_a_failed_cast_result_is_told() {
        let result = |success, reason| SessionEvent::CastResult {
            spell_id: 116,
            success,
            reason,
            arg: None,
        };
        assert_eq!(wire_of(&result(true, None)), None);
        assert_eq!(
            wire_of(&result(false, Some(0x2a))),
            Some(HostWire::CastFailed {
                spell_id: 116,
                reason: 0x2a
            })
        );
        assert_eq!(
            wire_of(&result(false, None)),
            Some(HostWire::CastFailed {
                spell_id: 116,
                reason: 255
            })
        );
    }

    /// The three pushes keep their kind, and an object sound keeps its guid even when that
    /// object is not streamed -- the client's own message would drop it to 2D.
    #[test]
    fn the_three_sound_pushes_keep_their_kind_and_the_raw_source() {
        assert_eq!(
            wire_of(&SessionEvent::PlaySound { sound_id: 1 }),
            Some(HostWire::Sound {
                kind: HostSoundKind::Sound2d,
                sound_id: 1,
                source: None
            })
        );
        assert_eq!(
            wire_of(&SessionEvent::PlayMusic { music_id: 6762 }),
            Some(HostWire::Sound {
                kind: HostSoundKind::Music,
                sound_id: 6762,
                source: None
            })
        );
        assert_eq!(
            wire_of(&SessionEvent::PlayObjectSound {
                sound_id: 9,
                guid: ECTOPLASM
            }),
            Some(HostWire::Sound {
                kind: HostSoundKind::Object,
                sound_id: 9,
                source: Some(ECTOPLASM)
            })
        );
    }

    #[test]
    fn any_other_packet_is_not_told() {
        assert_eq!(wire_of(&SessionEvent::NextMailTime { seconds: 1.0 }), None);
    }

    /// A refused swing is told by its kind -- the facing and the range are what a lab gets wrong.
    #[test]
    fn a_refused_swing_is_told_by_its_kind() {
        use benilla_protocol::AttackSwingError as E;
        for e in [E::BadFacing, E::NotInRange, E::DeadOrUnattackable] {
            assert_eq!(wire_of(&SessionEvent::AttackSwingError(e)), Some(HostWire::SwingRefused(e)));
        }
    }

    /// Through the real drain on the built client: the packets reach the host's message queue in
    /// wire order, whatever the arms then do with them.
    #[test]
    fn the_drain_tells_the_host_in_packet_order() {
        let mut app = crate::game_plugins::schedule_tests::headless_client();
        let (tx, rx) = crossbeam_channel::unbounded();
        app.insert_resource(crate::net::NetEvents(rx));
        tx.send(go(vec![(ECTOPLASM, 8)], vec![])).unwrap();
        tx.send(SessionEvent::PlayMusic { music_id: 6762 }).unwrap();
        super::super::apply_net_updates(app.world_mut());
        let told: Vec<HostWire> = app
            .world_mut()
            .resource_mut::<Messages<HostWire>>()
            .drain()
            .collect();
        assert_eq!(
            told,
            vec![
                HostWire::SpellGo {
                    caster: CASTER,
                    spell_id: 116,
                    hits: vec![],
                    misses: vec![(ECTOPLASM, 8)],
                },
                HostWire::Sound {
                    kind: HostSoundKind::Music,
                    sound_id: 6762,
                    source: None
                },
            ]
        );
    }
}
