//! Category filter on OSM key / value (Photon's `osm_tag` parameter).

/// One `osm_tag` value: `key`, `key:value` or `:value`.
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    key: Option<String>,
    value: Option<String>,
}

impl Rule {
    fn matches(&self, key: &str, value: &str) -> bool {
        self.key.as_deref().is_none_or(|k| k == key)
            && self.value.as_deref().is_none_or(|v| v == value)
    }
}

/// Keep places matching any include rule (all if there are none) and no
/// exclude rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TagFilter {
    include: Vec<Rule>,
    exclude: Vec<Rule>,
}

impl TagFilter {
    /// Parse `osm_tag` values: `amenity`, `amenity:cafe`, `:cafe`, and the
    /// same with a leading `!` to exclude (`!amenity:parking`).
    pub fn parse<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut f = Self::default();
        for raw in values {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let (exclude, spec) = match raw.strip_prefix('!') {
                Some(rest) => (true, rest),
                None => (false, raw),
            };
            let (key, value) = match spec.split_once(':') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (spec.trim(), None),
            };
            let key = (!key.is_empty()).then(|| key.to_string());
            let value = value.filter(|v| !v.is_empty()).map(str::to_string);
            if key.is_none() && value.is_none() {
                return Err(format!(
                    "invalid osm_tag '{raw}' (expected key, key:value or :value, optionally with a leading '!')"
                ));
            }
            let rule = Rule { key, value };
            if exclude {
                f.exclude.push(rule);
            } else {
                f.include.push(rule);
            }
        }
        Ok(f)
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    pub fn matches(&self, key: &str, value: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|r| r.matches(key, value)))
            && !self.exclude.iter().any(|r| r.matches(key, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn photon_syntax() {
        let f = TagFilter::parse(["amenity:cafe", "shop"]).unwrap();
        assert!(f.matches("amenity", "cafe"));
        assert!(f.matches("shop", "bakery"));
        assert!(!f.matches("amenity", "bar"));
        let f = TagFilter::parse([":cafe", "!amenity"]).unwrap();
        assert!(!f.matches("amenity", "cafe"), "excludes win");
        assert!(f.matches("shop", "cafe"));
        let f = TagFilter::parse(["!amenity:parking"]).unwrap();
        assert!(f.matches("highway", "residential"));
        assert!(!f.matches("amenity", "parking"));
        assert!(TagFilter::parse([""]).unwrap().is_empty());
        assert!(TagFilter::parse([":"]).is_err());
        assert!(TagFilter::parse(["!"]).is_err());
    }
}
