//! Ranking: combines text relevance, place importance and distance into one
//! score.
//!
//! The model is deliberately multiplicative so that raw BM25 scores never have
//! to be normalised across a result set:
//!
//! ```text
//! score = text
//!       * (1 + importance_weight * importance)
//!       * ((1 - location_bias) + location_bias * exp(-distance / scale))   // only with a focus point
//!       * name_match_boost                                                  // exact / prefix name match
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RankingConfig {
    /// How much a place's importance (0..1) can multiply its text score.
    pub importance_weight: f32,
    /// Share of the score that depends on proximity to the focus point, 0..1.
    pub location_bias: f32,
    /// Distance at which the proximity factor has decayed to 1/e, in km.
    pub location_bias_scale_km: f64,
    /// Multiplier when the place name equals the query.
    pub exact_match_boost: f32,
    /// Multiplier when the place name starts with the query.
    pub prefix_match_boost: f32,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            importance_weight: 1.0,
            location_bias: 0.6,
            location_bias_scale_km: 20.0,
            exact_match_boost: 1.6,
            prefix_match_boost: 1.25,
        }
    }
}

impl RankingConfig {
    /// Proximity factor in `[1 - location_bias, 1]`.
    pub fn proximity(&self, distance_m: f64) -> f32 {
        let scale_m = (self.location_bias_scale_km * 1000.0).max(1.0);
        let bias = self.location_bias.clamp(0.0, 1.0);
        (1.0 - bias) + bias * (-distance_m / scale_m).exp() as f32
    }

    /// Score used while collecting text hits: text x importance x proximity.
    pub fn combined(&self, text: f32, importance: f32, distance_m: Option<f64>) -> f32 {
        let mut s = text * (1.0 + self.importance_weight * importance.clamp(0.0, 1.0));
        if let Some(d) = distance_m {
            s *= self.proximity(d);
        }
        s
    }

    /// Boost for how well a (tokenised, normalised) name matches the query.
    ///
    /// `last_is_prefix` marks search-as-you-type queries where the last query
    /// token may be incomplete.
    pub fn name_match(&self, query: &[String], name: &[String], last_is_prefix: bool) -> f32 {
        if query.is_empty() || name.is_empty() {
            return 1.0;
        }
        if query == name {
            return self.exact_match_boost;
        }
        if query.len() <= name.len() {
            let (head, last) = query.split_at(query.len() - 1);
            let head_ok = head.iter().zip(name).all(|(q, n)| q == n);
            let cand = &name[query.len() - 1];
            if head_ok && last_is_prefix && cand.starts_with(&last[0]) {
                return if query.len() == name.len() {
                    // Only the last word is incomplete: nearly exact.
                    (self.exact_match_boost + self.prefix_match_boost) / 2.0
                } else {
                    self.prefix_match_boost
                };
            }
            if head_ok && cand == &last[0] {
                return self.prefix_match_boost;
            }
        }
        1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn proximity_decays() {
        let c = RankingConfig::default();
        assert!((c.proximity(0.0) - 1.0).abs() < 1e-6);
        assert!(c.proximity(1_000.0) > c.proximity(50_000.0));
        assert!(c.proximity(1e9) >= 1.0 - c.location_bias - 1e-6);
    }

    #[test]
    fn importance_and_distance_order() {
        let c = RankingConfig::default();
        assert!(c.combined(1.0, 0.9, None) > c.combined(1.0, 0.1, None));
        assert!(c.combined(1.0, 0.5, Some(100.0)) > c.combined(1.0, 0.5, Some(100_000.0)));
    }

    #[test]
    fn name_matching() {
        let c = RankingConfig::default();
        assert_eq!(
            c.name_match(&toks("vaduz"), &toks("vaduz"), true),
            c.exact_match_boost
        );
        assert!(c.name_match(&toks("vad"), &toks("vaduz"), true) > 1.0);
        assert_eq!(c.name_match(&toks("vad"), &toks("vaduz"), false), 1.0);
        assert_eq!(c.name_match(&toks("haupt"), &toks("vaduz"), true), 1.0);
        assert_eq!(
            c.name_match(&toks("st"), &toks("st peter strasse"), true),
            c.prefix_match_boost
        );
    }
}
