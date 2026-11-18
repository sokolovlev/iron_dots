//! Player colours. Eight hues chosen to stay distinguishable under the
//! common forms of colour blindness (based on the Okabe–Ito set).

/// Index 0 is "neutral"; 1..=8 are players.
pub const DEFAULT: [[f32; 4]; 9] = [
    [0.55, 0.55, 0.55, 1.0], // neutral
    [0.00, 0.45, 0.70, 1.0], // blue
    [0.84, 0.37, 0.00, 1.0], // vermillion
    [0.00, 0.62, 0.45, 1.0], // bluish green
    [0.80, 0.47, 0.65, 1.0], // reddish purple
    [0.90, 0.62, 0.00, 1.0], // orange
    [0.34, 0.71, 0.91, 1.0], // sky blue
    [0.55, 0.40, 0.20, 1.0], // brown
    [0.25, 0.25, 0.30, 1.0], // charcoal
];

/// Colours for "own / allies / enemies" mode (spec 2.9): own blue, allies
/// green, enemies red, regardless of player slots.
pub fn relative(me_team: u8, team_of: impl Fn(u8) -> u8, me: u8) -> [[f32; 4]; 9] {
    let mut c = DEFAULT;
    for p in 1..=8u8 {
        c[p as usize] = if p == me {
            [0.00, 0.45, 0.70, 1.0]
        } else if team_of(p) == me_team {
            [0.00, 0.62, 0.45, 1.0]
        } else {
            [0.84, 0.25, 0.15, 1.0]
        };
    }
    c
}
