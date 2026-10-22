//! The public API end to end on a small synthetic partition.

use geors::{
    AdminUnit, Contains, Error, Geocoder, GeocoderConfig, Layer, LonLat, Nearest, OsmType, Place,
    PlaceRef, Reverse, Search,
};

fn place(id: i64, layer: Layer, name: &str, lon: f64, lat: f64) -> Place {
    Place {
        osm_type: OsmType::Node,
        osm_id: id,
        osm_key: "amenity".into(),
        osm_value: "cafe".into(),
        layer,
        name: Some(name.into()),
        names: Default::default(),
        alt_names: vec![],
        housenumber: None,
        street: None,
        postcode: None,
        city: None,
        parents: vec![0, 1],
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

fn write(dir: &std::path::Path, places: &[Place]) {
    let admins = vec![
        AdminUnit {
            layer: Layer::Country,
            name: "Liechtenstein".into(),
            names: Default::default(),
        },
        AdminUnit {
            layer: Layer::City,
            name: "Vaduz".into(),
            names: Default::default(),
        },
    ];
    geors_index::write_partition_from_places(
        dir,
        "li",
        Some("Liechtenstein".into()),
        "test",
        places,
        admins,
    )
    .unwrap();
    geors_core::storage::bump_generation(dir).unwrap();
}

#[test]
fn every_query_type() {
    let dir = tempfile::tempdir().unwrap();
    let vaduz = LonLat::new(9.5209, 47.1410);
    write(
        dir.path(),
        &[
            place(1, Layer::City, "Vaduz", 9.5209, 47.1410),
            place(2, Layer::Poi, "Kunstmuseum", 9.5225, 47.1395),
            place(3, Layer::Poi, "Café Vaduz", 9.5215, 47.1405),
        ],
    );
    let g = Geocoder::open(dir.path(), GeocoderConfig::default()).unwrap();

    let hits = g.search(&Search::text("kunstmuseum")).unwrap();
    assert_eq!(hits[0].place.name.as_deref(), Some("Kunstmuseum"));
    let address = hits[0].address(None);
    assert_eq!(
        (address.city.as_deref(), address.country.as_deref()),
        (Some("Vaduz"), Some("Liechtenstein"))
    );

    let near = g
        .search(&Search::around(vaduz).radius_m(100.0).layers([Layer::Poi]))
        .unwrap();
    assert_eq!(near[0].place.name.as_deref(), Some("Café Vaduz"));

    let here = g
        .reverse(&Reverse::at(LonLat::new(9.5226, 47.1394)))
        .unwrap();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].place.name.as_deref(), Some("Kunstmuseum"));

    assert_eq!(g.nearest(&Nearest::to(vaduz).limit(3)).unwrap().len(), 3);

    let id: PlaceRef = "N2".parse().unwrap();
    assert_eq!(g.lookup(&[id], &[]).unwrap()[0].place.osm_id, 2);
    assert!("X2".parse::<PlaceRef>().is_err());

    assert!(g.contains(&Contains::at(vaduz)).unwrap().is_empty());

    let status = g.status();
    assert_eq!(status.partitions.len(), 1);
    assert_eq!(
        (
            status.partitions[0].name.as_str(),
            status.partitions[0].places
        ),
        ("li", 3)
    );

    // Invalid requests are errors, not panics; unknown countries name the loaded ones.
    assert!(matches!(
        g.search(&Search::text("x").radius_m(10.0)),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        g.search(&Search::text("x").countries(["de"])),
        Err(Error::CountryNotLoaded { .. })
    ));
}

#[test]
fn reloads_after_an_import() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        &[place(1, Layer::City, "Vaduz", 9.5209, 47.1410)],
    );
    let g = Geocoder::open(dir.path(), GeocoderConfig::default()).unwrap();
    assert!(!g.reload_if_changed().unwrap());
    write(
        dir.path(),
        &[
            place(1, Layer::City, "Vaduz", 9.5209, 47.1410),
            place(2, Layer::City, "Schaan", 9.5095, 47.1650),
        ],
    );
    assert!(g.reload_if_changed().unwrap());
    assert_eq!(g.status().partitions[0].places, 2);
    assert_eq!(
        g.search(&Search::text("schaan")).unwrap()[0]
            .place
            .name
            .as_deref(),
        Some("Schaan")
    );
}
