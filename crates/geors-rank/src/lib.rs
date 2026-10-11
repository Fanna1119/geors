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
//!       * city_echo_penalty                                                 // see `city_echo`
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
    /// Multiplier when the name only echoes the place's own city / region
    /// words of the query (see [`RankingConfig::city_echo`]).
    pub city_echo_penalty: f32,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            importance_weight: 1.0,
            location_bias: 0.6,
            location_bias_scale_km: 20.0,
            exact_match_boost: 1.6,
            prefix_match_boost: 1.25,
            city_echo_penalty: 0.5,
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

    /// Penalty for a place whose name matches only query words that are also
    /// its own address context (city, region, country), while other query
    /// words do not match the name.
    ///
    /// "long st cape town" should find Long Street in Cape Town, not "The
    /// Westin Cape Town" on Lower Long Street: the hotel's name matches only
    /// "cape town", which is where it is. A query naming the place itself
    /// ("westin cape town") or only the city ("cape town") is unaffected.
    ///
    /// `names` and `context` are tokenised like the query; with
    /// `last_is_prefix` the last query token may be incomplete.
    pub fn city_echo(
        &self,
        query: &[String],
        names: &[Vec<String>],
        context: &[String],
        last_is_prefix: bool,
    ) -> f32 {
        let name_words: Vec<&String> = names.iter().flatten().collect();
        let context_words: Vec<&String> = context.iter().collect();
        let hits = |i: usize, words: &[&String]| {
            let (t, prefix) = (&query[i], last_is_prefix && i + 1 == query.len());
            words
                .iter()
                .any(|w| *w == t || (prefix && w.starts_with(t.as_str())))
        };
        let in_name: Vec<usize> = (0..query.len()).filter(|&i| hits(i, &name_words)).collect();
        let echo = !in_name.is_empty()
            && in_name.len() < query.len()
            && in_name.iter().all(|&i| hits(i, &context_words));
        if echo { self.city_echo_penalty } else { 1.0 }
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
    fn city_echo() {
        let c = RankingConfig::default();
        let q = toks("long st cape town");
        let ctx = toks("cape town city of cape town western cape south africa");
        let hotel = vec![toks("the westin cape town convention square")];
        let street = vec![toks("long street")];
        assert_eq!(c.city_echo(&q, &hotel, &ctx, false), c.city_echo_penalty);
        assert_eq!(c.city_echo(&q, &street, &ctx, false), 1.0);
        // The place itself, or only the city: unaffected.
        assert_eq!(
            c.city_echo(&toks("westin cape town"), &hotel, &ctx, false),
            1.0
        );
        assert_eq!(c.city_echo(&toks("cape town"), &hotel, &ctx, false), 1.0);
        // Typing: "tow" is a prefix of "town".
        assert_eq!(
            c.city_echo(&toks("long st cape tow"), &hotel, &ctx, true),
            c.city_echo_penalty
        );
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
