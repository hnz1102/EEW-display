//! Offline "where is this epicenter" text for the TFT, whose font is ASCII only
//! (so JMA's Japanese hypocenter names can't be drawn) and for epicenters that
//! are mostly out at sea (so a reverse geocoder has nothing to return): the
//! nearest of ~100 well-known places, with distance and compass direction,
//! e.g. "45km ESE of Iwaki". No ESP-IDF dependency (the screenshot tool uses it).

/// (name, lat, lon): prefecture capitals, main coastal cities and outlying islands.
const PLACES: &[(&str, f64, f64)] = &[
    ("Sapporo", 43.06, 141.35), ("Asahikawa", 43.77, 142.36), ("Wakkanai", 45.42, 141.67),
    ("Abashiri", 44.02, 144.27), ("Kushiro", 42.98, 144.38), ("Nemuro", 43.33, 145.58),
    ("Obihiro", 42.92, 143.20), ("Hakodate", 41.77, 140.73), ("Muroran", 42.32, 140.97),
    ("Aomori", 40.82, 140.74), ("Hachinohe", 40.51, 141.49), ("Morioka", 39.70, 141.15),
    ("Miyako", 39.64, 141.96), ("Sendai", 38.27, 140.87), ("Ishinomaki", 38.43, 141.30),
    ("Akita", 39.72, 140.10), ("Yamagata", 38.24, 140.33), ("Fukushima", 37.76, 140.47),
    ("Iwaki", 37.05, 140.89), ("Mito", 36.34, 140.45), ("Choshi", 35.73, 140.83),
    ("Utsunomiya", 36.57, 139.88), ("Maebashi", 36.39, 139.06), ("Saitama", 35.86, 139.65),
    ("Chiba", 35.61, 140.12), ("Tokyo", 35.68, 139.69), ("Yokohama", 35.44, 139.64),
    ("Niigata", 37.90, 139.02), ("Toyama", 36.70, 137.21), ("Kanazawa", 36.59, 136.63),
    ("Fukui", 36.07, 136.22), ("Kofu", 35.66, 138.57), ("Nagano", 36.65, 138.18),
    ("Gifu", 35.42, 136.76), ("Shizuoka", 34.98, 138.38), ("Nagoya", 35.18, 136.91),
    ("Tsu", 34.73, 136.51), ("Otsu", 35.00, 135.87), ("Kyoto", 35.01, 135.77),
    ("Osaka", 34.69, 135.50), ("Kobe", 34.69, 135.20), ("Nara", 34.69, 135.83),
    ("Wakayama", 34.23, 135.17), ("Tottori", 35.50, 134.24), ("Matsue", 35.47, 133.05),
    ("Okayama", 34.66, 133.93), ("Hiroshima", 34.40, 132.46), ("Yamaguchi", 34.19, 131.47),
    ("Tokushima", 34.07, 134.56), ("Takamatsu", 34.34, 134.04), ("Matsuyama", 33.84, 132.77),
    ("Kochi", 33.56, 133.53), ("Muroto", 33.29, 134.15), ("Fukuoka", 33.59, 130.40),
    ("Saga", 33.25, 130.30), ("Nagasaki", 32.74, 129.87), ("Kumamoto", 32.79, 130.74),
    ("Oita", 33.24, 131.61), ("Miyazaki", 31.91, 131.42), ("Kagoshima", 31.60, 130.56),
    ("Naha", 26.21, 127.68),
    // islands
    ("Sado Is.", 38.02, 138.37), ("Oki Is.", 36.20, 133.33), ("Tsushima", 34.20, 129.29),
    ("Goto Is.", 32.69, 128.84), ("Tanegashima", 30.73, 131.00), ("Amami-Oshima", 28.38, 129.49),
    ("Miyakojima", 24.80, 125.28), ("Ishigaki", 24.34, 124.16), ("Yonaguni", 24.47, 122.99),
    ("Izu-Oshima", 34.75, 139.36), ("Niijima", 34.37, 139.26), ("Miyakejima", 34.08, 139.53),
    ("Hachijojima", 33.11, 139.79), ("Chichijima", 27.09, 142.19),
];

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = p2 - p1;
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    6371.0 * 2.0 * a.sqrt().asin()
}

/// Compass direction (8 points) of (lat2, lon2) as seen from (lat1, lon1).
fn direction(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> &'static str {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dl = (lon2 - lon1).to_radians();
    let y = dl.sin() * p2.cos();
    let x = p1.cos() * p2.sin() - p1.sin() * p2.cos() * dl.cos();
    let deg = (y.atan2(x).to_degrees() + 360.0) % 360.0;
    const DIRS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    DIRS[((deg + 22.5) / 45.0) as usize % 8]
}

/// "Near Iwaki" / "45km ESE of Iwaki"-style text for an epicenter (empty if the
/// coordinates are unknown).
pub fn describe(lat: f64, lon: f64) -> String {
    if lat.is_nan() || lon.is_nan() { return String::new(); }
    let (name, plat, plon) = PLACES.iter().copied()
        .min_by(|a, b| {
            haversine_km(lat, lon, a.1, a.2).partial_cmp(&haversine_km(lat, lon, b.1, b.2))
                .unwrap_or(core::cmp::Ordering::Equal)
        })
        .unwrap();
    let d = haversine_km(lat, lon, plat, plon);
    if d < 15.0 {
        format!("Near {}", name)
    } else {
        // 16 compass points would be overkill at these distances; round the distance.
        let km = if d < 100.0 { (d / 5.0).round() as i32 * 5 } else { (d / 10.0).round() as i32 * 10 };
        format!("{}km {} of {}", km, direction(plat, plon, lat, lon), name)
    }
}
