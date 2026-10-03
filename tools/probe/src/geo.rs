//! Where the player is, and the speed-of-light floor that distance sets.

use serde::Serialize;

/// Light in optical fiber covers about 200,000 km/s.
const FIBER_KM_PER_SECOND: f64 = 200_000.0;
const EARTH_RADIUS_KM: f64 = 6371.0;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Origin {
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub source: OriginSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginSource {
    /// Given on the command line.
    Given,
    /// Guessed from the country the player's ISP is registered in.
    Country,
}

/// Cities players commonly give with `--from` (case-insensitive).
pub const CITIES: &[(&str, f64, f64)] = &[
    ("Riyadh", 24.71, 46.68),
    ("Jeddah", 21.49, 39.19),
    ("Mecca", 21.39, 39.86),
    ("Medina", 24.47, 39.61),
    ("Dammam", 26.43, 50.10),
    ("Khobar", 26.22, 50.20),
    ("Tabuk", 28.38, 36.57),
    ("Abha", 18.22, 42.50),
    ("Dubai", 25.20, 55.27),
    ("Abu Dhabi", 24.45, 54.38),
    ("Sharjah", 25.35, 55.42),
    ("Doha", 25.29, 51.53),
    ("Kuwait City", 29.38, 47.99),
    ("Manama", 26.23, 50.59),
    ("Muscat", 23.59, 58.41),
    ("Amman", 31.95, 35.93),
    ("Baghdad", 33.31, 44.36),
    ("Basra", 30.51, 47.81),
    ("Erbil", 36.19, 44.01),
    ("Beirut", 33.89, 35.50),
    ("Cairo", 30.04, 31.24),
    ("Alexandria", 31.20, 29.92),
];

/// The city used when only the ISP's country is known.
const COUNTRY_CITIES: &[(&str, &str)] = &[
    ("SA", "Riyadh"),
    ("AE", "Dubai"),
    ("QA", "Doha"),
    ("KW", "Kuwait City"),
    ("BH", "Manama"),
    ("OM", "Muscat"),
    ("JO", "Amman"),
    ("IQ", "Baghdad"),
    ("LB", "Beirut"),
    ("EG", "Cairo"),
];

pub fn city(name: &str) -> Option<Origin> {
    CITIES
        .iter()
        .find(|(city, ..)| city.eq_ignore_ascii_case(name.trim()))
        .map(|&(name, lat, lon)| Origin {
            name: name.to_string(),
            lat,
            lon,
            source: OriginSource::Given,
        })
}

pub fn from_country(code: &str) -> Option<Origin> {
    let (_, name) = COUNTRY_CITIES
        .iter()
        .find(|(cc, _)| cc.eq_ignore_ascii_case(code))?;
    city(name).map(|o| Origin {
        name: format!("{} (from ISP country {code})", o.name),
        source: OriginSource::Country,
        ..o
    })
}

/// Parses `"24.71,46.68"`.
pub fn coordinates(text: &str) -> Option<Origin> {
    let (lat, lon) = text.split_once(',')?;
    let (lat, lon): (f64, f64) = (lat.trim().parse().ok()?, lon.trim().parse().ok()?);
    ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then(|| Origin {
        name: format!("{lat:.2}, {lon:.2}"),
        lat,
        lon,
        source: OriginSource::Given,
    })
}

/// Great-circle distance.
pub fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * h.sqrt().asin()
}

/// The lowest round trip physics allows over fiber along the great circle.
pub fn fiber_floor_ms(distance_km: f64) -> f64 {
    2.0 * distance_km / FIBER_KM_PER_SECOND * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn riyadh_to_frankfurt_matches_the_plan() {
        let riyadh = city("riyadh").unwrap();
        let km = distance_km(riyadh.lat, riyadh.lon, 50.11, 8.68);
        assert!((4250.0..4350.0).contains(&km), "{km}");
        let floor = fiber_floor_ms(km);
        assert!((42.0..44.0).contains(&floor), "{floor}");
    }

    #[test]
    fn parses_origins() {
        assert_eq!(city(" Abu Dhabi ").unwrap().name, "Abu Dhabi");
        assert!(city("Atlantis").is_none());
        assert_eq!(from_country("sa").unwrap().source, OriginSource::Country);
        assert_eq!(coordinates("24.7, 46.7").unwrap().lat, 24.7);
        assert!(coordinates("124.7,46.7").is_none());
    }
}
