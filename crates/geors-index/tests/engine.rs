//! End-to-end: write synthetic partitions, then check search, filters and
//! nearest-neighbour exactness against brute force.

use geors_core::geom::{self, BBox, LonLat};
use geors_core::{AdminUnit, Layer, OsmType, Place};
use geors_index::{
    Engine, EngineConfig, EngineError, NearestRequest, PlaceRef, SearchRequest, Shape,
    write_partition_from_places,
};

fn place(id: i64, layer: Layer, name: Option<&str>, lon: f64, lat: f64) -> Place {
    Place {
        osm_type: OsmType::Node,
        osm_id: id,
        osm_key: "amenity".into(),
        osm_value: "cafe".into(),
        layer,
        name: name.map(str::to_string),
        names: Default::default(),
        alt_names: vec![],
        housenumber: None,
        street: None,
        postcode: None,
        city: None,
        parents: vec![0],
        country_code: Some("LI".into()),
        center: LonLat::new(lon, lat),
        extent: None,
        importance: layer.base_importance(),
        lines: vec![],
        polygons: Vec::new(),
        merged_ids: Vec::new(),
        nominatim_id: None,
    }
}

/// Deterministic pseudo random numbers in [0, 1).
fn rng(seed: &mut u64) -> f64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    (*seed % 1_000_000) as f64 / 1_000_000.0
}

fn fixture() -> (tempfile::TempDir, Engine, Vec<Place>) {
    let dir = tempfile::tempdir().unwrap();
    let mut places = vec![
        place(1, Layer::City, Some("Vaduz"), 9.5209, 47.1410),
        place(2, Layer::Poi, Some("Café Vaduz"), 9.5215, 47.1405),
        place(3, Layer::Poi, Some("Kunstmuseum"), 9.5225, 47.1395),
        place(4, Layer::City, Some("Schaan"), 9.5095, 47.1650),
    ];
    let mut street = place(5, Layer::Street, Some("Landstrasse"), 9.50, 47.10);
    // A long street: its centre is far from (9.55, 47.10) but the line passes close.
    street.lines = vec![vec![LonLat::new(9.50, 47.10), LonLat::new(9.60, 47.10)]];
    street.osm_type = OsmType::Way;
    street.merged_ids = vec![77];
    places.push(street);
    let mut park = place(6, Layer::Poi, Some("Hauptstrasse Park"), 9.53, 47.13);
    park.osm_type = OsmType::Way;
    park.polygons = vec![vec![vec![
        LonLat::new(9.529, 47.129),
        LonLat::new(9.531, 47.129),
        LonLat::new(9.531, 47.131),
        LonLat::new(9.529, 47.129),
    ]]];
    places.push(park);
    let mut seed = 42u64;
    for i in 0..2_000 {
        let lon = 9.45 + rng(&mut seed) * 0.2;
        let lat = 47.05 + rng(&mut seed) * 0.2;
        let layer = if i % 3 == 0 { Layer::House } else { Layer::Poi };
        places.push(place(100 + i, layer, Some(&format!("Shop {i}")), lon, lat));
    }
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "Liechtenstein".into(),
        names: Default::default(),
    }];
    let li = Some("Liechtenstein".to_string());
    write_partition_from_places(dir.path(), "li", li, "test", &places, admins.clone()).unwrap();
    // A second, tiny partition to exercise country selection.
    let andorra = [place(9, Layer::City, Some("Vaduz Andorra"), 1.52, 42.50)];
    let ad = Some("Andorra".to_string());
    write_partition_from_places(dir.path(), "ad", ad, "test", &andorra, admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    (dir, engine, places)
}

fn search(engine: &Engine, f: impl FnOnce(&mut SearchRequest)) -> Vec<String> {
    let mut req = SearchRequest {
        limit: 10,
        autocomplete: true,
        ..Default::default()
    };
    f(&mut req);
    engine
        .search(&req)
        .unwrap()
        .into_iter()
        .map(|h| h.place.name.unwrap_or_default())
        .collect()
}

#[test]
fn text_search_ranks_city_first() {
    let (_d, engine, _) = fixture();
    let names = search(&engine, |r| r.query = Some("vaduz".into()));
    assert_eq!(names[0], "Vaduz");
    // Autocomplete prefix and typo tolerance.
    assert_eq!(
        search(&engine, |r| r.query = Some("vad".into()))[0],
        "Vaduz"
    );
    assert_eq!(
        search(&engine, |r| r.query = Some("kunstmusem".into()))[0],
        "Kunstmuseum"
    );
}

#[test]
fn country_filter_selects_partition() {
    let (_d, engine, _) = fixture();
    let names = search(&engine, |r| {
        r.query = Some("vaduz".into());
        r.countries = vec!["ad".into()];
    });
    assert_eq!(names, vec!["Vaduz Andorra"]);
    let err = engine
        .search(&SearchRequest {
            query: Some("x".into()),
            countries: vec!["de".into()],
            limit: 1,
            ..Default::default()
        })
        .err()
        .unwrap();
    assert!(matches!(err, EngineError::CountryNotLoaded { .. }));
}

#[test]
fn radius_and_bbox_are_hard_filters() {
    let (_d, engine, places) = fixture();
    let center = LonLat::new(9.55, 47.15);
    let hits = engine
        .search(&SearchRequest {
            query: Some("shop".into()),
            focus: Some(center),
            radius_m: Some(1_500.0),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty());
    for h in &hits {
        assert!(h.distance_m.unwrap() <= 1_500.0);
    }
    let expected = places
        .iter()
        .filter(|p| p.name.as_deref().is_some_and(|n| n.starts_with("Shop")))
        .filter(|p| geom::haversine(center, p.center) <= 1_500.0)
        .count();
    assert_eq!(hits.len(), expected.min(50));

    let bbox = BBox::new(9.50, 47.10, 9.52, 47.12);
    let hits = engine
        .search(&SearchRequest {
            query: Some("shop".into()),
            bbox: Some(bbox),
            limit: 50,
            ..Default::default()
        })
        .unwrap();
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| bbox.contains(h.place.center)));
}

#[test]
fn nearest_matches_brute_force() {
    let (_d, engine, places) = fixture();
    let li: Vec<&Place> = places.iter().collect();
    let mut seed = 7u64;
    for _ in 0..50 {
        let p = LonLat::new(9.45 + rng(&mut seed) * 0.2, 47.05 + rng(&mut seed) * 0.2);
        for layers in [vec![], vec![Layer::House]] {
            let hits = engine
                .nearest(&NearestRequest {
                    tags: Default::default(),
                    point: p,
                    max_distance_m: None,
                    countries: vec!["li".into()],
                    layers: layers.clone(),
                    limit: 5,
                })
                .unwrap();
            let mut brute: Vec<f64> = li
                .iter()
                .filter(|pl| layers.is_empty() || layers.contains(&pl.layer))
                .map(|pl| match pl.lines.first() {
                    Some(line) => geom::point_line_distance(p, line).unwrap(),
                    None => geom::haversine(p, pl.center),
                })
                .collect();
            brute.sort_by(f64::total_cmp);
            let got: Vec<f64> = hits.iter().map(|h| h.distance_m.unwrap()).collect();
            assert_eq!(got.len(), 5);
            for (g, b) in got.iter().zip(&brute) {
                // Coordinates are stored with 1e-7 degree precision.
                assert!(
                    (g - b).abs() < 0.05,
                    "got {got:?} expected {:?}",
                    &brute[..5]
                );
            }
        }
    }
}

#[test]
fn reverse_uses_line_geometry() {
    let (_d, engine, _) = fixture();
    // 100 m north of the middle of Landstrasse, ~3.8 km from its first node.
    let p = LonLat::new(9.55, 47.1009);
    let hits = engine
        .nearest(&NearestRequest {
            tags: Default::default(),
            point: p,
            max_distance_m: Some(200.0),
            countries: vec![],
            layers: vec![Layer::Street],
            limit: 1,
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].place.name.as_deref(), Some("Landstrasse"));
    assert!((hits[0].distance_m.unwrap() - 100.0).abs() < 1.0);
}

#[test]
fn spatial_only_search_needs_location() {
    let (_d, engine, _) = fixture();
    assert!(matches!(
        engine.search(&SearchRequest {
            limit: 5,
            ..Default::default()
        }),
        Err(EngineError::BadRequest(_))
    ));
    let hits = engine
        .search(&SearchRequest {
            focus: Some(LonLat::new(9.5209, 47.1410)),
            radius_m: Some(200.0),
            layers: vec![Layer::City],
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].place.name.as_deref(), Some("Vaduz"));
}

#[test]
fn lookup_by_osm_id_including_merged_segments() {
    let (_d, engine, _) = fixture();
    let ids = [
        (OsmType::Way, 77),
        (OsmType::Node, 1),
        (OsmType::Node, 999_999),
        (OsmType::Way, 5),
    ];
    let refs: Vec<PlaceRef> = ids.iter().map(|&(t, id)| PlaceRef::Osm(t, id)).collect();
    let hits = engine.lookup(&refs, &[]).unwrap();
    let names: Vec<_> = hits.iter().map(|h| h.place.name.clone().unwrap()).collect();
    // Request order, unknown ids skipped, merged id resolves to the street.
    assert_eq!(names, vec!["Landstrasse", "Vaduz", "Landstrasse"]);
}

#[test]
fn abbreviations_expand() {
    let (_d, engine, _) = fixture();
    let hits = engine
        .search(&SearchRequest {
            query: Some("hauptstr. park ".into()),
            limit: 1,
            autocomplete: false,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].place.name.as_deref(), Some("Hauptstrasse Park"));
}

#[test]
fn shapes_are_stored() {
    let (_d, engine, _) = fixture();
    let hits = engine
        .lookup(
            &[
                PlaceRef::Osm(OsmType::Way, 6),
                PlaceRef::Osm(OsmType::Way, 5),
                PlaceRef::Osm(OsmType::Node, 1),
            ],
            &[],
        )
        .unwrap();
    assert!(matches!(hits[0].shape(), Shape::Polygons(ref p) if p[0][0].len() == 4));
    assert!(matches!(hits[1].shape(), Shape::Lines(ref l) if l[0].len() == 2));
    assert!(matches!(hits[2].shape(), Shape::Point(_)));
}

/// Serving must work from read-only data (images, read-only volumes): the
/// text index must not need lock files.
#[cfg(unix)]
#[test]
fn opens_read_only_partitions() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, engine, _) = fixture();
    drop(engine);
    let set_mode = |mode: u32| {
        for entry in walk(dir.path()) {
            let mut p = std::fs::metadata(&entry).unwrap().permissions();
            p.set_mode(if entry.is_dir() { mode | 0o111 } else { mode });
            std::fs::set_permissions(&entry, p).unwrap();
        }
    };
    set_mode(0o444);
    let result = Engine::open(dir.path(), &[], EngineConfig::default());
    set_mode(0o644); // let the temp dir be cleaned up
    let engine = result.expect("read-only partitions open");
    assert_eq!(search(&engine, |r| r.query = Some("vaduz".into()))[0], "Vaduz");
}

#[cfg(unix)]
fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[test]
fn words_written_together_are_split() {
    let dir = tempfile::tempdir().unwrap();
    let names = [
        "Karl-Marx-Allee",
        "Erich-Heckel-Strasse",
        "Raiffeisenbank",
        "Raiffeisen-Volksbank",
        "Marktplatz",
    ];
    let places: Vec<Place> = names
        .iter()
        .enumerate()
        .map(|(i, n)| place(i as i64 + 1, Layer::Street, Some(n), 9.5 + i as f64 * 0.01, 47.1))
        .collect();
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "Liechtenstein".into(),
        names: Default::default(),
    }];
    write_partition_from_places(dir.path(), "li", None, "test", &places, admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    let first = |q: &str| search(&engine, |r| r.query = Some(q.into())).into_iter().next();
    // Not the last word, the last word, and a word still being typed.
    assert_eq!(first("karlmarx allee").as_deref(), Some("Karl-Marx-Allee"));
    assert_eq!(first("karlmarxallee").as_deref(), Some("Karl-Marx-Allee"));
    assert_eq!(first("erichheckelstr").as_deref(), Some("Erich-Heckel-Strasse"));
    // The start of a real word is not split: "raiffeisen" + "ban…" would
    // also match Raiffeisen-Volksbank.
    assert_eq!(
        search(&engine, |r| r.query = Some("raiffeisenban".into())),
        vec!["Raiffeisenbank"]
    );
    // Unrelated words do not combine into matches.
    assert_eq!(first("markthecke"), None);
}

#[test]
fn partitions_without_a_typed_word_are_skipped_safely() {
    let (_d, engine, _) = fixture();
    // "vaduz" is in both partitions, "andorra" only in one: the other is
    // skipped in the exact phase, and the result is the same as without.
    assert_eq!(
        search(&engine, |r| r.query = Some("vaduz andorra".into()))[0],
        "Vaduz Andorra"
    );
    // A word only reachable through an abbreviation, a split, or as a
    // house number still matches.
    assert_eq!(
        search(&engine, |r| r.query = Some("hauptstr. park".into()))[0],
        "Hauptstrasse Park"
    );
    assert_eq!(
        search(&engine, |r| r.query = Some("hauptstrassepark p".into()))[0],
        "Hauptstrasse Park"
    );
    // Misspelled: no partition has it exactly, the typo phase still runs.
    assert_eq!(
        search(&engine, |r| r.query = Some("kunstmusem liechtenstein".into()))
            .first()
            .map(String::as_str),
        Some("Kunstmuseum")
    );
    // Typos in the word that selects the partition, in the word being
    // typed, and one word missing: later phases skip partitions too.
    assert_eq!(
        search(&engine, |r| r.query = Some("vaduz andora".into()))[0],
        "Vaduz Andorra"
    );
    assert_eq!(
        search(&engine, |r| r.query = Some("vaduz andorr".into()))[0],
        "Vaduz Andorra"
    );
    assert_eq!(
        search(&engine, |r| r.query = Some("vaduz andorra xyzzyq".into()))[0],
        "Vaduz Andorra"
    );
}

#[test]
fn short_prefixes_suggest_major_places_first() {
    let (_d, engine, _) = fixture();
    // Enough cities start with "vad": only they are suggested.
    let names = search(&engine, |r| {
        r.query = Some("vad".into());
        r.limit = 2;
    });
    assert_eq!(names, ["Vaduz", "Vaduz Andorra"]);
    // Too few: everything is searched.
    let names = search(&engine, |r| r.query = Some("vad".into()));
    assert!(names.contains(&"Café Vaduz".to_string()), "{names:?}");
    let names = search(&engine, |r| r.query = Some("sho".into()));
    assert_eq!(names.len(), 10);
    // A location narrows the search: no major-first step.
    let names = search(&engine, |r| {
        r.query = Some("vad".into());
        r.limit = 2;
        r.focus = Some(LonLat::new(9.5215, 47.1405));
    });
    assert!(names.contains(&"Café Vaduz".to_string()), "{names:?}");
}

#[test]
fn other_scripts_match_in_latin_letters() {
    let dir = tempfile::tempdir().unwrap();
    let places = vec![
        place(1, Layer::Poi, Some("Кафе Москва"), 9.50, 47.1),
        place(2, Layer::Poi, Some("Αθήνα Taverna"), 9.51, 47.1),
        place(3, Layer::Poi, Some("Hotel Kiev"), 9.52, 47.1),
    ];
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "Liechtenstein".into(),
        names: Default::default(),
    }];
    write_partition_from_places(dir.path(), "li", None, "test", &places, admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    let first = |q: &str| search(&engine, |r| r.query = Some(q.into())).into_iter().next();
    // Latin query, name in another script: the name is shown as is.
    assert_eq!(first("kafe moskva").as_deref(), Some("Кафе Москва"));
    assert_eq!(first("mosk").as_deref(), Some("Кафе Москва"));
    assert_eq!(first("athena").as_deref(), Some("Αθήνα Taverna"));
    // A query in another script also matches Latin names.
    assert_eq!(first("Киев").as_deref(), Some("Hotel Kiev"));
}

#[test]
fn phrase_synonyms() {
    let dir = tempfile::tempdir().unwrap();
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "België".into(),
        names: Default::default(),
    }];
    let mut basilica = place(1, Layer::Poi, Some("Onze Lieve Vrouw Basiliek"), 4.40, 51.2);
    basilica.country_code = Some("BE".into());
    write_partition_from_places(dir.path(), "be", None, "test", &[basilica], admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    // Dutch rules apply in Belgium: "olv" -> "onze lieve vrouw". A lone
    // word has no missing-word fallback, so only the phrase can match.
    let olv = |q: &str| search(&engine, |r| r.query = Some(q.into()));
    assert_eq!(olv("olv"), vec!["Onze Lieve Vrouw Basiliek"]);
    assert!(olv("xyz").is_empty());
}

#[test]
fn areas_containing_a_point() {
    let dir = tempfile::tempdir().unwrap();
    let square = |a: f64, b: f64| {
        vec![vec![
            LonLat::new(a, a + 40.0),
            LonLat::new(b, a + 40.0),
            LonLat::new(b, b + 40.0),
            LonLat::new(a, b + 40.0),
            LonLat::new(a, a + 40.0),
        ]]
    };
    let area = |id: i64, layer: Layer, name: &str, poly: Vec<Vec<LonLat>>| {
        let mut p = place(id, layer, Some(name), (poly[0][0].lon + poly[0][1].lon) / 2.0, poly[0][0].lat);
        p.osm_type = OsmType::Relation;
        p.extent = Some(BBox::new(poly[0][0].lon, poly[0][0].lat, poly[0][2].lon, poly[0][2].lat));
        p.polygons = vec![poly];
        p
    };
    let mut big = area(1, Layer::State, "Big", square(0.0, 10.0));
    // A hole: points in it are outside the state.
    big.polygons[0].push(square(4.0, 5.0)[0].clone());
    let places = vec![
        big,
        area(2, Layer::City, "Small", square(1.0, 3.0)),
        area(3, Layer::City, "Elsewhere", square(20.0, 30.0)),
        place(4, Layer::Poi, Some("Cafe"), 2.0, 42.0),
    ];
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "Testland".into(),
        names: Default::default(),
    }];
    write_partition_from_places(dir.path(), "li", None, "test", &places, admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    let names = |lon: f64, lat: f64, layers: &[Layer]| -> Vec<String> {
        engine
            .containing(LonLat::new(lon, lat), &[], layers)
            .unwrap()
            .into_iter()
            .map(|h| h.place.name.unwrap_or_default())
            .collect()
    };
    // Most specific first; only areas (not the cafe at the same spot).
    assert_eq!(names(2.0, 42.0, &[]), vec!["Small", "Big"]);
    assert_eq!(names(2.0, 42.0, &[Layer::State]), vec!["Big"]);
    assert_eq!(names(4.5, 44.5, &[]), Vec::<String>::new(), "in the hole");
    assert_eq!(names(8.0, 48.0, &[]), vec!["Big"]);
    assert!(names(15.0, 55.0, &[]).is_empty());
}

#[test]
fn osm_tag_filter() {
    let (_d, engine, _) = fixture();
    let tags = |v: &[&str]| geors_index::TagFilter::parse(v.iter().copied()).unwrap();
    let names = |q: Option<&str>, t: &[&str]| {
        search(&engine, |r| {
            r.query = q.map(str::to_string);
            r.focus = Some(LonLat::new(9.52, 47.14));
            r.tags = tags(t);
        })
    };
    // Every fixture place is amenity=cafe.
    assert!(!names(Some("shop"), &["amenity:cafe"]).is_empty());
    assert!(names(Some("shop"), &["!amenity"]).is_empty());
    assert!(names(Some("shop"), &["shop"]).is_empty());
    // Spatial-only search and nearest apply it too.
    assert!(!names(None, &[":cafe"]).is_empty());
    assert!(names(None, &["amenity:bar"]).is_empty());
}

#[test]
fn lookup_by_nominatim_place_id() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = place(10, Layer::Poi, Some("Rathaus"), 9.52, 47.14);
    a.nominatim_id = Some(123_456);
    let b = place(11, Layer::Poi, Some("Post"), 9.53, 47.14);
    let admins = vec![AdminUnit {
        layer: Layer::Country,
        name: "Liechtenstein".into(),
        names: Default::default(),
    }];
    write_partition_from_places(dir.path(), "li", None, "test", &[a, b], admins).unwrap();
    let engine = Engine::open(dir.path(), &[], EngineConfig::default()).unwrap();
    let hits = engine
        .lookup(
            &[
                PlaceRef::Osm(OsmType::Node, 11),
                PlaceRef::Nominatim(123_456),
                PlaceRef::Nominatim(999),
            ],
            &[],
        )
        .unwrap();
    let names: Vec<_> = hits.iter().map(|h| h.place.name.clone().unwrap()).collect();
    assert_eq!(names, vec!["Post", "Rathaus"]);
}
