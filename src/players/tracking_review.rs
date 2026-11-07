use super::*;
use zerqon_protocol::codec::{get_string, get_varint, put_varint, take};

#[derive(Debug, PartialEq)]
struct RenderedProfile {
    name: String,
    properties: Vec<(String, String, Option<String>)>,
    latency: i32,
}

#[derive(Debug)]
struct RenderedEntity {
    uuid: Uuid,
    xyz: [f64; 3],
    body_yaw: u8,
    head_yaw: u8,
    equipment: Option<u8>,
}

#[derive(Default)]
struct VirtualClient {
    profiles: BTreeMap<Uuid, RenderedProfile>,
    entities: BTreeMap<i32, RenderedEntity>,
}

fn uuid(view: &mut &[u8]) -> Uuid {
    Uuid::from_slice(take(view, 16).unwrap()).unwrap()
}

fn double(view: &mut &[u8]) -> f64 {
    f64::from_be_bytes(take(view, 8).unwrap().try_into().unwrap())
}

fn float(view: &mut &[u8]) -> f32 {
    f32::from_be_bytes(take(view, 4).unwrap().try_into().unwrap())
}

fn quarter_turn(yaw: f32) -> u8 {
    match yaw {
        0.0 => 0,
        90.0 => 64,
        180.0 => 128,
        270.0 => 192,
        _ => panic!("unexpected fixture angle {yaw}"),
    }
}

impl VirtualClient {
    fn consume(&mut self, packet: &[u8]) {
        let mut view = packet;
        match get_varint(&mut view).unwrap() {
            71 => {
                let actions = take(&mut view, 1).unwrap()[0];
                assert_eq!(actions & !0x9d, 0);
                let count = get_varint(&mut view).unwrap();
                assert!((1..=8).contains(&count));
                for _ in 0..count {
                    let key = uuid(&mut view);
                    if actions & 1 != 0 {
                        let name = get_string(&mut view, 16).unwrap().to_owned();
                        let count = get_varint(&mut view).unwrap();
                        assert!((0..=16).contains(&count));
                        let mut properties = Vec::new();
                        for _ in 0..count {
                            let key = get_string(&mut view, 32767).unwrap().to_owned();
                            let value = get_string(&mut view, 32767).unwrap().to_owned();
                            let signature = match take(&mut view, 1).unwrap()[0] {
                                0 => None,
                                1 => Some(get_string(&mut view, 32767).unwrap().to_owned()),
                                value => panic!("invalid property signature flag {value}"),
                            };
                            properties.push((key, value, signature));
                        }
                        assert!(
                            self.entities.values().all(|entity| entity.uuid != key),
                            "profile replaced before its old entity was removed"
                        );
                        assert!(
                            self.profiles
                                .insert(
                                    key,
                                    RenderedProfile {
                                        name,
                                        properties,
                                        latency: 0
                                    }
                                )
                                .is_none(),
                            "duplicate profile add"
                        );
                    }
                    let profile = self
                        .profiles
                        .get_mut(&key)
                        .expect("TAB update before profile add");
                    if actions & 4 != 0 {
                        assert_eq!(get_varint(&mut view).unwrap(), 1);
                    }
                    if actions & 8 != 0 {
                        assert_eq!(take(&mut view, 1).unwrap(), [1]);
                    }
                    if actions & 16 != 0 {
                        profile.latency = get_varint(&mut view).unwrap();
                    }
                    if actions & 128 != 0 {
                        assert_eq!(take(&mut view, 1).unwrap(), [1]);
                    }
                }
            }
            70 => {
                let count = get_varint(&mut view).unwrap();
                assert!((1..=8).contains(&count));
                for _ in 0..count {
                    let key = uuid(&mut view);
                    assert!(
                        self.profiles.remove(&key).is_some(),
                        "removed unknown TAB profile"
                    );
                }
            }
            1 => {
                let id = get_varint(&mut view).unwrap();
                let uuid = uuid(&mut view);
                assert_eq!(get_varint(&mut view).unwrap(), 159);
                let xyz = [double(&mut view), double(&mut view), double(&mut view)];
                let tail = take(&mut view, 5).unwrap();
                assert_eq!([tail[0], tail[1], tail[4]], [0, 0, 0]);
                assert!(
                    self.profiles.contains_key(&uuid),
                    "entity spawned before its profile"
                );
                assert!(
                    self.entities.values().all(|entity| entity.uuid != uuid),
                    "duplicate entity UUID"
                );
                assert!(
                    self.entities
                        .insert(
                            id,
                            RenderedEntity {
                                uuid,
                                xyz,
                                body_yaw: tail[2],
                                head_yaw: tail[3],
                                equipment: None
                            }
                        )
                        .is_none(),
                    "duplicate entity spawn"
                );
            }
            35 => {
                let id = get_varint(&mut view).unwrap();
                assert_eq!(get_varint(&mut view).unwrap(), 0);
                let entity = self
                    .entities
                    .get_mut(&id)
                    .expect("movement before spawn or after removal");
                entity.xyz = [double(&mut view), double(&mut view), double(&mut view)];
                entity.body_yaw = quarter_turn(float(&mut view));
                assert_eq!(float(&mut view), 0.0);
                assert!(take(&mut view, 1).unwrap()[0] <= 1);
            }
            85 => {
                let id = get_varint(&mut view).unwrap();
                self.entities
                    .get_mut(&id)
                    .expect("head update for unknown entity")
                    .head_yaw = take(&mut view, 1).unwrap()[0];
            }
            78 => {
                let count = get_varint(&mut view).unwrap();
                assert!((1..=8).contains(&count));
                for _ in 0..count {
                    let id = get_varint(&mut view).unwrap();
                    assert!(
                        self.entities.remove(&id).is_some(),
                        "removed unknown entity {id}"
                    );
                }
            }
            101 => {
                let id = get_varint(&mut view).unwrap();
                assert!(self.entities.contains_key(&id), "metadata before spawn");
                assert_eq!(view, [16, 0, 127, 255]);
                view = &[];
            }
            104 => {
                let id = get_varint(&mut view).unwrap();
                assert_eq!(take(&mut view, 1).unwrap(), [0x80]);
                let count = get_varint(&mut view).unwrap();
                assert!((0..=64).contains(&count));
                if count != 0 {
                    assert_eq!(get_varint(&mut view).unwrap(), 1);
                    assert_eq!(get_varint(&mut view).unwrap(), 0);
                    assert_eq!(get_varint(&mut view).unwrap(), 0);
                }
                assert_eq!(take(&mut view, 2).unwrap(), [1, 0]);
                self.entities
                    .get_mut(&id)
                    .expect("equipment before spawn or after removal")
                    .equipment = Some(count as u8);
            }
            id => panic!("unexpected tracking packet {id}"),
        }
        assert!(view.is_empty(), "trailing tracking packet bytes");
    }
}

struct Session {
    handle: PlayerHandle,
    receiver: mpsc::Receiver<PacketBatch>,
    disconnected: watch::Receiver<bool>,
    active: bool,
    identity: Identity,
    position: Position,
    latency: i32,
    equipment: Option<u8>,
    client: VirtualClient,
}

fn connect(players: &Arc<Players>, slot: usize, generation: u32) -> Session {
    let identity = Identity {
        uuid: Uuid::from_u128(slot as u128 + 1),
        name: format!("Peer{slot}"),
        properties: vec![(
            "textures".into(),
            format!("generation-{generation}"),
            Some("fixture-signature".into()),
        )],
    };
    let (sender, receiver) = mpsc::channel(2);
    let (disconnect, disconnected) = watch::channel(false);
    let handle = players
        .reserve(identity.clone(), sender, disconnect)
        .unwrap();
    Session {
        handle,
        receiver,
        disconnected,
        active: false,
        identity,
        position: Position::default(),
        latency: 0,
        equipment: None,
        client: VirtualClient::default(),
    }
}

fn equipment(id: i32, count: u8) -> Bytes {
    let mut packet = vec![104];
    put_varint(id, &mut packet);
    packet.push(0x80);
    put_varint(i32::from(count), &mut packet);
    if count != 0 {
        packet.extend_from_slice(&[1, 0, 0]);
    }
    packet.extend_from_slice(&[1, 0]);
    packet.into()
}

fn flush_and_compare(players: &Players, sessions: &mut [Option<Session>], step: usize) {
    players.tracking_tick().unwrap();
    let authoritative = players.snapshots().unwrap();
    assert_eq!(
        authoritative.len(),
        sessions
            .iter()
            .flatten()
            .filter(|session| session.active)
            .count(),
        "step {step}"
    );
    let expected: BTreeMap<_, _> = sessions
        .iter()
        .flatten()
        .filter(|session| session.active)
        .map(|session| {
            let snapshot = authoritative
                .iter()
                .find(|snapshot| snapshot.uuid == session.identity.uuid)
                .unwrap();
            assert_eq!(snapshot.id, session.handle.id());
            assert_eq!(snapshot.position, session.position);
            (
                session.identity.uuid,
                (
                    session.handle.id(),
                    session.identity.clone(),
                    session.position,
                    session.latency,
                    session.equipment,
                ),
            )
        })
        .collect();
    for observer in sessions.iter_mut().flatten() {
        assert!(
            !*observer.disconnected.borrow(),
            "healthy observer disconnected at step {step}"
        );
        assert!(
            observer.receiver.len() <= 1,
            "more than one tracking batch at step {step}"
        );
        while let Ok(batch) = observer.receiver.try_recv() {
            assert!(batch.inventory_revision().is_none());
            for packet in batch {
                observer.client.consume(&packet);
            }
        }
        if !observer.active {
            assert!(observer.client.profiles.is_empty() && observer.client.entities.is_empty());
            continue;
        }
        assert_eq!(
            observer.client.profiles.len(),
            expected.len(),
            "TAB at step {step}"
        );
        let mut visible = BTreeMap::new();
        for (&uuid, (id, identity, position, latency, equipment)) in &expected {
            assert_eq!(
                observer.client.profiles.get(&uuid),
                Some(&RenderedProfile {
                    name: identity.name.clone(),
                    properties: identity.properties.clone(),
                    latency: *latency
                }),
                "profile at step {step}"
            );
            if uuid != observer.identity.uuid
                && (position.xyz[0] - observer.position.xyz[0]).abs() <= 16.0
                && (position.xyz[2] - observer.position.xyz[2]).abs() <= 16.0
            {
                visible.insert(*id, (uuid, *position, *equipment));
            }
        }
        assert_eq!(
            observer.client.entities.len(),
            visible.len(),
            "visible count at step {step}"
        );
        for (id, (uuid, position, equipment)) in visible {
            let rendered = observer
                .client
                .entities
                .get(&id)
                .unwrap_or_else(|| panic!("missing entity {id} at step {step}"));
            assert_eq!(rendered.uuid, uuid);
            assert_eq!(rendered.xyz, position.xyz);
            assert_eq!(rendered.body_yaw, quarter_turn(position.yaw));
            assert_eq!(rendered.head_yaw, quarter_turn(position.yaw));
            assert_eq!(rendered.equipment, equipment, "equipment at step {step}");
        }
    }
}

fn next(random: &mut u64) -> u64 {
    *random ^= *random << 13;
    *random ^= *random >> 7;
    *random ^= *random << 17;
    *random
}

#[test]
fn seeded_lifecycle_matches_each_observers_virtual_client() {
    for seed in [0x263777_u64, 0xcedf1231, 0x7f840126] {
        let ids = Arc::new(EntityIds::default());
        ids.advance_past(126).unwrap();
        let players = Players::new(8, 16.0, 159, ids);
        let mut random = seed;
        let mut generations = [0_u32; 8];
        let mut sessions: Vec<Option<Session>> = (0..8).map(|_| None).collect();
        for step in 0_usize..500 {
            let slot = next(&mut random) as usize % sessions.len();
            match next(&mut random) % 7 {
                0 => {
                    if sessions[slot].is_none() {
                        generations[slot] += 1;
                        sessions[slot] = Some(connect(&players, slot, generations[slot]));
                    }
                }
                1 => {
                    if let Some(session) = &mut sessions[slot] {
                        session.handle.activate().unwrap();
                        session.active = true;
                    }
                }
                2 => {
                    if let Some(session) = &mut sessions[slot] {
                        let x = ((next(&mut random) % 17) as i32 - 8) * 8;
                        let z = ((next(&mut random) % 17) as i32 - 8) * 8;
                        let yaw = (next(&mut random) % 4) as f32 * 90.0;
                        session.position = Position {
                            xyz: [
                                f64::from(x) + 0.5,
                                if next(&mut random) & 1 == 0 {
                                    -60.0
                                } else {
                                    64.0
                                },
                                f64::from(z) + 0.5,
                            ],
                            yaw,
                            pitch: 0.0,
                            on_ground: next(&mut random) & 1 == 0,
                        };
                        session.handle.move_to(session.position).unwrap();
                    }
                }
                3 => {
                    if let Some(session) = &mut sessions[slot] {
                        let count = (next(&mut random) % 65) as u8;
                        players
                            .set_equipment(
                                session.identity.uuid,
                                session.handle.id(),
                                equipment(session.handle.id(), count),
                            )
                            .unwrap();
                        session.equipment = Some(count);
                    }
                }
                4 => {
                    if let Some(session) = &mut sessions[slot]
                        && session.active
                    {
                        session.latency = (next(&mut random) % 1000) as i32;
                        session.handle.latency(session.latency).unwrap();
                    }
                }
                5 => {
                    drop(sessions[slot].take());
                }
                6 => {
                    drop(sessions[slot].take());
                    generations[slot] += 1;
                    let mut session = connect(&players, slot, generations[slot]);
                    session.handle.activate().unwrap();
                    session.active = true;
                    sessions[slot] = Some(session);
                }
                _ => unreachable!(),
            }
            if next(&mut random).is_multiple_of(5) || step.is_multiple_of(37) {
                flush_and_compare(&players, &mut sessions, step);
            }
        }
        flush_and_compare(&players, &mut sessions, 500);
    }
}
