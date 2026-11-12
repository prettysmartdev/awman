//! Seeded round-trip and merge-law properties for Layer 0 config.

use awman::data::config::effective::{ConfigLayers, EffectiveConfig};
use awman::data::config::env::EnvSnapshot;
use awman::data::config::file::{ConfigFile, FileKind};
use awman::data::config::flags::FlagConfig;
use awman::data::config::overrides::ConfigOverrides;
use proptest::prelude::*;
use serde_json::{json, Value};

proptest! {
    #![proptest_config(proptest::test_runner::Config {
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x0144_0002),
        ..proptest::test_runner::Config::default()
    })]

    #[test]
    fn config_parse_serialize_round_trip_preserves_valid_agent_and_scrollback(
        agent in "[a-z][a-z0-9_-]{0,20}",
        scrollback in 1_i64..=100_000,
    ) {
        awman_test_support::evidence::note("seed", "0x01440002 (fixed proptest seed)");
        let value = json!({"agents":{"default":agent},"tui":{"scrollbackLines":scrollback}});
        let parsed = ConfigFile::from_value(value, FileKind::Global).unwrap();
        let encoded = serde_json::to_string(&parsed).unwrap();
        let reparsed = ConfigFile::parse(&encoded, FileKind::Global, std::path::Path::new("memory/config.json")).unwrap();
        prop_assert_eq!(parsed, reparsed);
    }

    #[test]
    fn identical_layers_are_idempotent_and_later_set_override_wins(
        first in "[a-z][a-z0-9_-]{0,20}",
        later in "[a-z][a-z0-9_-]{0,20}",
    ) {
        awman_test_support::evidence::note("seed", "0x01440002 (fixed proptest seed)");
        let global = ConfigFile::from_value(json!({"agents":{"default":first}}), FileKind::Global).unwrap();
        let once = EffectiveConfig::from_files(global.clone(), ConfigFile::default(), FlagConfig::default(), EnvSnapshot::empty()).unwrap();
        let twice = EffectiveConfig::resolve(
            ConfigLayers { global, ..Default::default() }, FlagConfig::default(), EnvSnapshot::empty(),
        ).unwrap();
        prop_assert_eq!(once.value(), twice.value());

        let overrides = ConfigOverrides::new(vec![
            ("agents.default".into(), json!(first)),
            ("agents.default".into(), json!(later.clone())),
        ]);
        let merged = EffectiveConfig::resolve(
            ConfigLayers { set: overrides, ..Default::default() }, FlagConfig::default(), EnvSnapshot::empty(),
        ).unwrap();
        prop_assert_eq!(merged.value()["agents"]["default"].as_str(), Some(later.as_str()));
    }

    #[test]
    fn additive_guidance_merge_is_ordered_and_deduplicates_trimmed_values(
        first in "[a-z]{1,12}",
        second in "[a-z]{1,12}",
        third in "[a-z]{1,12}",
    ) {
        awman_test_support::evidence::note("seed", "0x01440002 (fixed proptest seed)");
        let global = ConfigFile::from_value(
            json!({"workflows":{"dynamic":{"guidance":[first.clone(), second.clone()]}}}),
            FileKind::Global,
        ).unwrap();
        let repo = ConfigFile::from_value(
            json!({"workflows":{"dynamic":{"guidance":[format!(" {second} "), third.clone()]}}}),
            FileKind::Repo,
        ).unwrap();
        let merged = EffectiveConfig::resolve(
            ConfigLayers { global, repo, ..Default::default() },
            FlagConfig::default(),
            EnvSnapshot::empty(),
        ).unwrap();
        let guidance = merged.value()["workflows"]["dynamic"]["guidance"].as_array().unwrap();
        let expected = [first, second, third].into_iter().fold(Vec::new(), |mut acc, item| {
            if !acc.contains(&item) { acc.push(item); }
            acc
        });
        let expected: Vec<Value> = expected.into_iter().map(Value::String).collect();
        prop_assert_eq!(guidance, &expected);
    }
}
