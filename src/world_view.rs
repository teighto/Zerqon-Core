use crate::players::Position;
use std::collections::HashSet;

pub fn center(x: f64, z: f64) -> (i32, i32) {
    ((x / 16.0).floor() as i32, (z / 16.0).floor() as i32)
}

pub fn desired(center: (i32, i32), radius: i32) -> HashSet<(i32, i32)> {
    (-radius..=radius)
        .flat_map(|x| (-radius..=radius).map(move |z| (center.0 + x, center.1 + z)))
        .collect()
}

pub fn valid_position(position: Position) -> bool {
    position.xyz.iter().all(|value| value.is_finite())
        && position.xyz[0].abs() <= 30_000_000.0
        && position.xyz[2].abs() <= 30_000_000.0
        && position.xyz[1].abs() <= 20_000_000.0
        && position.yaw.is_finite()
        && position.pitch.is_finite()
}

pub fn can_move(
    previous: [f64; 3],
    next: [f64; 3],
    delivered: &HashSet<(i32, i32)>,
    desired: &HashSet<(i32, i32)>,
) -> bool {
    if previous.iter().chain(&next).any(|value| !value.is_finite())
        || (next[0] - previous[0]).abs() > 64.0
        || (next[2] - previous[2]).abs() > 64.0
    {
        return false;
    }
    let minimum = center(
        previous[0].min(next[0]) - 0.3,
        previous[2].min(next[2]) - 0.3,
    );
    let maximum = center(
        previous[0].max(next[0]) + 0.3,
        previous[2].max(next[2]) + 0.3,
    );
    (minimum.0..=maximum.0).all(|x| {
        (minimum.1..=maximum.1).all(|z| delivered.contains(&(x, z)) && desired.contains(&(x, z)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_requires_the_swept_player_footprint_not_only_destination() {
        let delivered = HashSet::from([(0, 0), (2, 0)]);
        assert!(!can_move(
            [0.5, 0.0, 0.5],
            [32.5, 0.0, 0.5],
            &delivered,
            &delivered
        ));
        let delivered = HashSet::from([(0, 0), (1, 0), (2, 0)]);
        assert!(can_move(
            [0.5, 0.0, 0.5],
            [47.69, 0.0, 0.5],
            &delivered,
            &delivered
        ));
        assert!(!can_move(
            [0.5, 0.0, 0.5],
            [47.8, 0.0, 0.5],
            &delivered,
            &delivered
        ));
    }

    #[test]
    fn scheduled_unloads_and_unsent_chunks_are_unavailable() {
        let delivered = HashSet::from([(0, 0)]);
        let desired = HashSet::from([(1, 0)]);
        assert!(!can_move(
            [0.5, 0.0, 0.5],
            [1.0, 0.0, 1.0],
            &delivered,
            &desired
        ));
        assert!(!can_move(
            [16.5, 0.0, 0.5],
            [17.0, 0.0, 1.0],
            &delivered,
            &desired
        ));
    }

    #[test]
    fn negative_chunks_and_unbounded_moves_are_checked() {
        let delivered = HashSet::from([(-1, -1)]);
        assert!(can_move(
            [-1.0, 0.0, -1.0],
            [-15.0, 0.0, -15.0],
            &delivered,
            &delivered
        ));
        assert!(!can_move(
            [-1.0, 0.0, -1.0],
            [-0.2, 0.0, -1.0],
            &delivered,
            &delivered
        ));
        assert!(!can_move(
            [-1.0, 0.0, -1.0],
            [f64::MAX, 0.0, 0.0],
            &delivered,
            &delivered
        ));
        assert!(!can_move(
            [-1.0, 0.0, -1.0],
            [f64::NAN, 0.0, 0.0],
            &delivered,
            &delivered
        ));
    }
}
