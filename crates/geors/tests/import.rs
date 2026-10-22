//! `geors::import` then querying the result (feature `import`).
#![cfg(feature = "import")]

use geors::import::{ImportOptions, import};
use geors::{Error, Geocoder, GeocoderConfig, Search};

/// A Nominatim dump: `li` places, plus one `ch` place (a border sliver).
fn dump(path: &std::path::Path, li: usize) {
    let mut out = String::from(r#"{"type":"NominatimDumpFile","content":{"version":"0.1.0"}}"#);
    let codes = std::iter::repeat_n("li", li).chain(["ch"]);
    for (i, cc) in codes.enumerate() {
        out.push_str(&format!(
            "\n{{\"type\":\"Place\",\"content\":[{{\"place_id\":{i},\"object_type\":\"N\",\"object_id\":{i},\"osm_key\":\"amenity\",\"osm_value\":\"cafe\",\"rank_address\":30,\"name\":{{\"name\":\"Cafe {i}\"}},\"country_code\":\"{cc}\",\"centroid\":[9.52,47.14]}}]}}"
        ));
    }
    std::fs::write(path, out + "\n").unwrap();
}

#[test]
fn import_then_search() {
    let dir = tempfile::tempdir().unwrap();
    let (input, data) = (dir.path().join("li.jsonl"), dir.path().join("data"));
    dump(&input, 200);

    let report = import(&input, &data, &ImportOptions::default()).unwrap();
    assert_eq!(report.partitions, ["li"]);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].country_code, "ch");
    assert!(report.skipped[0].reason.starts_with("border sliver"));

    let geocoder = Geocoder::open(&data, GeocoderConfig::default()).unwrap();
    let hits = geocoder.search(&Search::text("cafe 42")).unwrap();
    assert_eq!(hits[0].place.name.as_deref(), Some("Cafe 42"));

    // Importing again replaces the partition; the geocoder reloads it.
    dump(&input, 300);
    import(&input, &data, &ImportOptions::default()).unwrap();
    assert!(geocoder.reload_if_changed().unwrap());
    assert_eq!(geocoder.status().partitions[0].places, 300);

    // Asking for the sliver keeps it.
    let only_ch = ImportOptions {
        countries: vec!["CH".into()],
        ..Default::default()
    };
    assert_eq!(import(&input, &data, &only_ch).unwrap().partitions, ["ch"]);
}

#[test]
fn bad_inputs_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let opts = ImportOptions::default();
    assert!(matches!(
        import("https://example.org/x.osm.pbf", &data, &opts),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        import(dir.path().join("x.txt"), &data, &opts),
        Err(Error::Import(_))
    ));
    assert!(matches!(
        import(dir.path().join("missing.osm.pbf"), &data, &opts),
        Err(Error::Import(_))
    ));
    let bad_country = ImportOptions {
        countries: vec!["XYZ".into()],
        ..Default::default()
    };
    assert!(matches!(
        import(dir.path().join("x.jsonl"), &data, &bad_country),
        Err(Error::InvalidRequest(_))
    ));
}
