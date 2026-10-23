//! Import an extract, then geocode against it:
//!
//! ```sh
//! curl -LO https://download.geofabrik.de/europe/liechtenstein-latest.osm.pbf
//! cargo run -p geors --features import --example import -- liechtenstein-latest.osm.pbf data
//! ```

use geors::import::{ImportOptions, import};
use geors::{Geocoder, GeocoderConfig, LonLat, Reverse};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(input), Some(data)) = (args.next(), args.next()) else {
        return Err("usage: import <extract.osm.pbf> <data dir>".into());
    };
    let report = import(&input, &data, &ImportOptions::default())?;
    println!("wrote partitions {:?}", report.partitions);
    for s in &report.skipped {
        println!("skipped {} ({} places): {}", s.country_code, s.places, s.reason);
    }

    let geocoder = Geocoder::open(&data, GeocoderConfig::default())?;
    for p in geocoder.status().partitions {
        println!("{}: {} places", p.name, p.places);
    }
    // The address of a partition's center.
    if let Some(p) = geocoder.status().partitions.first() {
        let b = p.bbox;
        let center = LonLat::new((b.min_lon + b.max_lon) / 2.0, (b.min_lat + b.max_lat) / 2.0);
        for hit in geocoder.reverse(&Reverse::at(center))? {
            println!("{center:?} -> {:?}", hit.place.name);
        }
    }
    Ok(())
}
