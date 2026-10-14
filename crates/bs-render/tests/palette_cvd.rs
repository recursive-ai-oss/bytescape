//! The category palette under simulated colorblindness.
//!
//! Every pair of category fills must stay apart when seen with protanopia,
//! deuteranopia or tritanopia, in both modes, and every label must keep a
//! readable contrast. The simulation is Machado, Oliveira and Fernandes (2009)
//! at full severity, applied in linear RGB; distances are CIE76 ΔE in CIELAB
//! (D65).
//!
//! Threshold: ΔE ≥ 6. A just-noticeable difference is about 2.3, so 6 is
//! clearly visible, and the categories are also told apart by the legend and by
//! their labels. The floor is set by the Okabe-Ito set itself: after the tint
//! toward white, Images and Audio are 6.2 apart for a deuteranope in light
//! mode, which no choice of the two free colors (Code & dev and Other) can
//! raise. The test is a regression gate: a palette edit that merges two
//! categories fails here.

use bs_render::scene::{CATEGORY_COUNT, CATEGORY_NAMES};
use bs_render::{Color, Palette};

const MIN_DELTA_E: f32 = 6.0;
const MIN_LABEL_CONTRAST: f32 = 4.5;

type Matrix = [[f32; 3]; 3];

const PROTAN: Matrix = [
    [0.152286, 1.052583, -0.204868],
    [0.114503, 0.786281, 0.099216],
    [-0.003882, -0.048116, 1.051998],
];
const DEUTAN: Matrix = [
    [0.367322, 0.860646, -0.227968],
    [0.280085, 0.672501, 0.047413],
    [-0.011820, 0.042940, 0.968881],
];
const TRITAN: Matrix = [
    [1.255528, -0.076749, -0.178779],
    [-0.078411, 0.930809, 0.147602],
    [0.004733, 0.691367, 0.303900],
];

fn to_linear(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn simulate(c: Color, m: &Matrix) -> [f32; 3] {
    let v = [to_linear(c.r), to_linear(c.g), to_linear(c.b)];
    let mut out = [0.0; 3];
    for (o, row) in out.iter_mut().zip(m) {
        *o = (row[0] * v[0] + row[1] * v[1] + row[2] * v[2]).clamp(0.0, 1.0);
    }
    out
}

fn lab(linear: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = linear;
    let x = (0.4124564 * r + 0.3575761 * g + 0.1804375 * b) / 0.95047;
    let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
    let z = (0.0193339 * r + 0.119192 * g + 0.9503041 * b) / 1.08883;
    let f = |t: f32| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

fn delta_e(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn normal(c: Color) -> [f32; 3] {
    lab([to_linear(c.r), to_linear(c.g), to_linear(c.b)])
}

/// The smallest pairwise distance for one vision type, with the pair.
fn weakest(pal: &Palette, m: Option<&Matrix>) -> (f32, usize, usize) {
    let labs: Vec<[f32; 3]> = pal
        .categories
        .iter()
        .map(|&c| match m {
            Some(m) => lab(simulate(c, m)),
            None => normal(c),
        })
        .collect();
    let mut worst = (f32::MAX, 0, 0);
    for i in 0..CATEGORY_COUNT {
        for j in i + 1..CATEGORY_COUNT {
            let d = delta_e(labs[i], labs[j]);
            if d < worst.0 {
                worst = (d, i, j);
            }
        }
    }
    worst
}

#[test]
fn every_pair_of_categories_stays_apart_under_every_simulation() {
    let mut report = String::new();
    let mut failures = Vec::new();
    for pal in [Palette::light(), Palette::dark()] {
        let mode = if pal.dark { "dark" } else { "light" };
        for (name, m) in [
            ("normal", None),
            ("protanopia", Some(&PROTAN)),
            ("deuteranopia", Some(&DEUTAN)),
            ("tritanopia", Some(&TRITAN)),
        ] {
            let (d, i, j) = weakest(&pal, m);
            report.push_str(&format!(
                "{mode:5} {name:13} weakest pair {} / {}: dE {d:.1}\n",
                CATEGORY_NAMES[i], CATEGORY_NAMES[j]
            ));
            if d < MIN_DELTA_E {
                failures.push(format!(
                    "{mode} {name}: {} and {} are only dE {d:.1} apart",
                    CATEGORY_NAMES[i], CATEGORY_NAMES[j]
                ));
            }
        }
    }
    println!("{report}");
    assert!(failures.is_empty(), "{failures:#?}\n{report}");
}

#[test]
fn labels_keep_their_contrast_on_every_fill() {
    for pal in [Palette::light(), Palette::dark()] {
        for c in 0..CATEGORY_COUNT as u8 {
            let ratio = pal.category_fill(c).contrast(pal.category_label(c));
            assert!(
                ratio >= MIN_LABEL_CONTRAST,
                "dark={} {}: {ratio:.2}",
                pal.dark,
                CATEGORY_NAMES[c as usize]
            );
        }
    }
}

#[test]
fn fills_stay_apart_from_the_background_they_sit_on() {
    // A tile that melts into the map background would read as empty space. The
    // same floor as between categories applies.
    for pal in [Palette::light(), Palette::dark()] {
        for (c, fill) in pal.categories.iter().enumerate() {
            let d = delta_e(normal(*fill), normal(pal.bg));
            assert!(
                d >= MIN_DELTA_E,
                "dark={} {}: dE {d:.1} from the background",
                pal.dark,
                CATEGORY_NAMES[c]
            );
        }
    }
}

#[test]
fn the_simulation_itself_is_sane() {
    // Gray stays gray under every simulation, and pure red loses its
    // distance from green for deuteranopes.
    let gray = Color::rgb(128, 128, 128);
    for m in [&PROTAN, &DEUTAN, &TRITAN] {
        let [r, g, b] = simulate(gray, m);
        assert!((r - g).abs() < 0.01 && (g - b).abs() < 0.01, "{r} {g} {b}");
    }
    let red = Color::rgb(220, 30, 30);
    let green = Color::rgb(30, 160, 30);
    let seen = delta_e(lab(simulate(red, &DEUTAN)), lab(simulate(green, &DEUTAN)));
    assert!(seen < delta_e(normal(red), normal(green)));
}
