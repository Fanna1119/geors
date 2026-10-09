//! End-to-end: write synthetic partitions, then check search, filters and
//! nearest-neighbour exactness against brute force.

use geors_core::geom::{self, BBox, LonLat};
use geors_core::{AdminUnit, Layer, OsmType, Place};
use geors_index::{
    Engine, EngineConfig, EngineError, NearestRequest, PartitionInput, SearchRequest, Shape,
    write_partition,
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
    write_partition(
        dir.path(),
        PartitionInput {
            country_code: "li".into(),
            country_name: Some("Liechtenstein".into()),
            source: "test".into(),
            places: places.clone(),
            admins: admins.clone(),
        },
    )
    .unwrap();
    // A second, tiny partition to exercise country selection.
    write_partition(
        dir.path(),
        PartitionInput {
            country_code: "ad".into(),
            country_name: Some("Andorra".into()),
            source: "test".into(),
            places: vec![place(9, Layer::City, Some("Vaduz Andorra"), 1.52, 42.50)],
            admins,
        },
    )
    .unwrap();
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
    let hits = engine.lookup(&ids, &[]).unwrap();
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
            &[(OsmType::Way, 6), (OsmType::Way, 5), (OsmType::Node, 1)],
            &[],
        )
        .unwrap();
    assert!(matches!(hits[0].shape(), Shape::Polygons(ref p) if p[0][0].len() == 4));
    assert!(matches!(hits[1].shape(), Shape::Lines(ref l) if l[0].len() == 2));
    assert!(matches!(hits[2].shape(), Shape::Point(_)));
}
