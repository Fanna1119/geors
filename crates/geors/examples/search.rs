//! Search a data directory built by `geors import`:
//!
//! ```sh
//! cargo run -p geors --example search -- data "Städtle 1, Vaduz"
//! ```

use geors::{Geocoder, GeocoderConfig, Search};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(data), Some(query)) = (args.next(), args.next()) else {
        return Err("usage: search <data dir> <query>".into());
    };
    let geocoder = Geocoder::open(&data, GeocoderConfig::default())?;
    for hit in geocoder.search(&Search::text(&query).limit(5))? {
        let p = &hit.place;
        // Houses have no name: show their street and number.
        let label = match (&p.name, &p.street, &p.housenumber) {
            (Some(name), _, _) => name.clone(),
            (None, Some(street), Some(number)) => format!("{street} {number}"),
            _ => "-".into(),
        };
        let address = hit.address(None);
        println!(
            "{label:<30} {:?}  {}, {}  ({:.5}, {:.5})",
            p.layer,
            address.city.as_deref().unwrap_or("-"),
            address.country_code,
            p.center.lat,
            p.center.lon,
        );
    }
    Ok(())
}
