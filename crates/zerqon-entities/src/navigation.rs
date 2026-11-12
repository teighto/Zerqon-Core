use crate::{MobKind, WorldQuery, physics};
use std::collections::{BTreeMap, VecDeque};

type Cell = [i32; 3];

pub(crate) fn standing(
    world: &impl WorldQuery,
    kind: MobKind,
    scale: f64,
    x: i32,
    z: i32,
    y: f64,
) -> Option<[f64; 3]> {
    if !world.loaded_at(x, z) {
        return None;
    }
    for offset in [1.0, 0.5, 0.0, -0.5, -1.0, -1.5, -2.0] {
        let start = [f64::from(x) + 0.5, y + offset + 0.01, f64::from(z) + 0.5];
        if let Ok(movement) = physics::move_scaled_with_scratch(
            start,
            kind,
            scale,
            [0.0, -0.55, 0.0],
            false,
            world,
            &mut physics::Scratch::default(),
        ) && movement.on_ground
        {
            return Some(movement.position);
        }
    }
    None
}

pub(crate) fn route(
    world: &impl WorldQuery,
    kind: MobKind,
    scale: f64,
    start: [f64; 3],
    goal: [f64; 3],
) -> VecDeque<[f64; 3]> {
    let cell = |p: [f64; 3]| {
        [
            p[0].floor() as i32,
            (p[1] * 2.0).round() as i32,
            p[2].floor() as i32,
        ]
    };
    let origin = cell(start);
    let distance = |p: [f64; 3]| {
        (p[0] - goal[0]).abs() + (p[2] - goal[2]).abs() + (p[1] - goal[1]).abs() * 0.25
    };
    let mut nodes =
        BTreeMap::<Cell, ([f64; 3], Option<Cell>, f64)>::from([(origin, (start, None, 0.0))]);
    let mut open = vec![origin];
    let mut best = origin;
    for _ in 0..128 {
        if open.is_empty() {
            break;
        }
        let index = (0..open.len())
            .min_by(|&a, &b| {
                let (pa, _, ca) = nodes[&open[a]];
                let (pb, _, cb) = nodes[&open[b]];
                (ca + distance(pa)).total_cmp(&(cb + distance(pb)))
            })
            .unwrap_or(0);
        let current = open.swap_remove(index);
        let (position, _, cost) = nodes[&current];
        if distance(position) < distance(nodes[&best].0) {
            best = current;
        }
        if distance(position) < 0.8 {
            best = current;
            break;
        }
        for (dx, dz) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (x, z) = (current[0] + dx, current[2] + dz);
            if (x - origin[0]).abs().max((z - origin[2]).abs()) > 12 {
                continue;
            }
            let Some(next) = standing(world, kind, scale, x, z, position[1]) else {
                continue;
            };
            if next[1] - position[1] > 1.01 || position[1] - next[1] > 2.01 {
                continue;
            }
            let lift = (next[1] - position[1]).max(0.0);
            let raised = [position[0], position[1] + lift, position[2]];
            let Ok(movement) = physics::move_scaled_with_scratch(
                raised,
                kind,
                scale,
                [next[0] - position[0], 0.0, next[2] - position[2]],
                false,
                world,
                &mut physics::Scratch::default(),
            ) else {
                continue;
            };
            if (movement.position[0] - next[0]).abs() + (movement.position[2] - next[2]).abs()
                > 0.05
            {
                continue;
            }
            let key = cell(next);
            let next_cost = cost + 1.0 + (next[1] - position[1]).abs() * 0.25;
            if nodes.get(&key).is_some_and(|(_, _, old)| *old <= next_cost) {
                continue;
            }
            if nodes.len() >= 256 {
                continue;
            }
            nodes.insert(key, (next, Some(current), next_cost));
            open.push(key);
        }
    }
    let mut result = VecDeque::new();
    let mut at = best;
    while at != origin && result.len() < 128 {
        let (position, parent, _) = nodes[&at];
        result.push_front(position);
        let Some(parent) = parent else {
            break;
        };
        at = parent;
    }
    result
}

#[derive(Default)]
pub(crate) struct Brain {
    pub partner: Option<i32>,
    pub courtship: u16,
    pub path: VecDeque<[f64; 3]>,
    pub next_path: u64,
    pub goal: Option<[i32; 3]>,
    pub panic_until: u64,
    pub flee_from: [f64; 3],
    pub rest_until: u64,
}
