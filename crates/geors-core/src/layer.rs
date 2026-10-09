use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Coarse classification of a place. Mirrors Photon's `type` values, with
/// `poi` for everything that is a named thing rather than an address unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Layer {
    House = 0,
    Poi = 1,
    Street = 2,
    Locality = 3,
    District = 4,
    City = 5,
    County = 6,
    State = 7,
    Country = 8,
}

impl Layer {
    pub const ALL: [Layer; 9] = [
        Layer::House,
        Layer::Poi,
        Layer::Street,
        Layer::Locality,
        Layer::District,
        Layer::City,
        Layer::County,
        Layer::State,
        Layer::Country,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Layer::House => "house",
            Layer::Poi => "poi",
            Layer::Street => "street",
            Layer::Locality => "locality",
            Layer::District => "district",
            Layer::City => "city",
            Layer::County => "county",
            Layer::State => "state",
            Layer::Country => "country",
        }
    }

    pub fn from_u8(v: u8) -> Option<Layer> {
        Layer::ALL.get(v as usize).copied()
    }

    /// Layers that can act as a parent in the address hierarchy.
    pub fn is_admin(self) -> bool {
        matches!(
            self,
            Layer::District | Layer::City | Layer::County | Layer::State | Layer::Country
        )
    }

    /// A rough prior for how important a place of this layer is, in `[0, 1]`.
    pub fn base_importance(self) -> f32 {
        match self {
            Layer::House => 0.10,
            Layer::Poi => 0.20,
            Layer::Street => 0.25,
            Layer::Locality => 0.30,
            Layer::District => 0.40,
            Layer::City => 0.55,
            Layer::County => 0.60,
            Layer::State => 0.80,
            Layer::Country => 1.00,
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Layer {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let layer = match s.trim().to_ascii_lowercase().as_str() {
            "house" | "housenumber" | "address" => Layer::House,
            "poi" | "other" | "venue" => Layer::Poi,
            "street" => Layer::Street,
            "locality" => Layer::Locality,
            "district" => Layer::District,
            "city" => Layer::City,
            "county" => Layer::County,
            "state" => Layer::State,
            "country" => Layer::Country,
            other => {
                return Err(format!(
                    "unknown layer '{other}' (expected one of: house, poi, street, locality, district, city, county, state, country)"
                ));
            }
        };
        Ok(layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        for l in Layer::ALL {
            assert_eq!(Layer::from_u8(l as u8), Some(l));
            assert_eq!(l.as_str().parse::<Layer>().unwrap(), l);
        }
        assert_eq!("housenumber".parse::<Layer>().unwrap(), Layer::House);
        assert!("planet".parse::<Layer>().is_err());
    }
}
