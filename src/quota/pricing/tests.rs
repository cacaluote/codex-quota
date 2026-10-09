use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

static CACHE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TestCache {
    directory: PathBuf,
}

impl TestCache {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "codex-price-{}-{}",
            std::process::id(),
            CACHE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self { directory }
    }
    fn path(&self) -> PathBuf {
        self.directory.join(CACHE_FILE_NAME)
    }
}

impl Drop for TestCache {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn insert_row(text: &str, heading: &str, row: &str) -> String {
    let (before, rest) = text.split_once(heading).unwrap();
    let mut lines: Vec<&str> = rest.lines().collect();
    let index = lines
        .iter()
        .position(|line| line.starts_with("| ---"))
        .unwrap()
        + 1;
    lines.insert(index, row);
    format!("{before}{heading}{}", lines.join("\n"))
}

#[test]
fn official_rates_cover_all_bands() {
    let table = PriceTable::from_pricing_md(OFFICIAL_PRICING).unwrap();
    for (model, tier, band, expected) in [
        (
            "gpt-6.1-sol",
            ServiceTier::Standard,
            ContextBand::Short,
            (2.0, 0.1, 10.0),
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Standard,
            ContextBand::Long,
            (4.0, 0.2, 15.0),
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Fast,
            ContextBand::Short,
            (4.0, 0.2, 20.0),
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Fast,
            ContextBand::Long,
            (8.0, 0.4, 30.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Standard,
            ContextBand::Short,
            (10.0, 1.0, 50.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Standard,
            ContextBand::Long,
            (20.0, 2.0, 75.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Fast,
            ContextBand::Short,
            (20.0, 2.0, 100.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Fast,
            ContextBand::Long,
            (40.0, 4.0, 150.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Ultrafast,
            ContextBand::Short,
            (60.0, 6.0, 300.0),
        ),
        (
            "gpt-6-astra",
            ServiceTier::Ultrafast,
            ContextBand::Long,
            (120.0, 12.0, 450.0),
        ),
        (
            "gpt-5.5",
            ServiceTier::Fast,
            ContextBand::Short,
            (12.5, 1.25, 75.0),
        ),
        (
            "gpt-5.3-codex",
            ServiceTier::Standard,
            ContextBand::Unknown,
            (1.75, 0.175, 14.0),
        ),
        (
            "gpt-5.3-codex",
            ServiceTier::Fast,
            ContextBand::Short,
            (3.5, 0.35, 28.0),
        ),
        // 微调表中同名快照是另一种价格，不能覆盖普通模型。
        (
            "gpt-4.1-2025-04-14",
            ServiceTier::Standard,
            ContextBand::Long,
            (2.0, 0.5, 8.0),
        ),
    ] {
        let price = table.lookup(Some(model), Some(&tier), band).unwrap();
        assert_eq!(
            (price.input, price.cached_input, price.output),
            (Some(expected.0), Some(expected.1), Some(expected.2)),
            "{model} / {tier:?} / {band:?}"
        );
    }
}

#[test]
fn future_model_uses_published_rates() {
    let text = insert_row(
        OFFICIAL_PRICING,
        "### Standard pricing data",
        "| future-model | $2.00 | $0.10 | - | $5.00 | $4.00 | $0.20 | - | $7.50 |",
    );
    let text = insert_row(
        &text,
        "### Fast pricing data",
        "| future-model | $7.00 | $0.25 | - | $9.00 | $11.00 | $0.35 | - | $13.00 |",
    );
    let table = PriceTable::from_pricing_md(&text).unwrap();
    let price = table
        .lookup(
            Some("future-model"),
            Some(&ServiceTier::Fast),
            ContextBand::Long,
        )
        .unwrap();
    assert_eq!(
        price,
        ModelPrice {
            input: Some(11.0),
            cached_input: Some(0.35),
            output: Some(13.0)
        }
    );
}

#[test]
fn table_columns_allow_reordering() {
    let text = OFFICIAL_PRICING
        .lines()
        .map(|line| {
            if !line.starts_with('|') {
                return line.to_owned();
            }
            let mut cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() != 9 {
                return line.to_owned();
            }
            cells.swap(1, 4);
            cells.push(if cells[0] == "Model" {
                "Future column"
            } else if cells[0].starts_with('-') {
                "---"
            } else {
                "ignored"
            });
            format!("| {} |", cells.join(" | "))
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        PriceTable::from_pricing_md(&text).unwrap().models,
        PriceTable::from_pricing_md(OFFICIAL_PRICING)
            .unwrap()
            .models
    );
}

#[test]
fn model_notes_and_missing_rates() {
    for (label, cell, input, cached, expected) in [
        (
            "annotation",
            "gpt-6.1-sol (preview)",
            "$2.00",
            "$0.10",
            (Some(2.0), Some(0.1)),
        ),
        (
            "code",
            "`gpt-6.1-sol`",
            "$2.00",
            "$0.10",
            (Some(2.0), Some(0.1)),
        ),
        (
            "code and annotation",
            "`gpt-6.1-sol` (preview)",
            "$2.00",
            "$0.10",
            (Some(2.0), Some(0.1)),
        ),
        (
            "comma and missing",
            "gpt-6.1-sol",
            "$2,000.00",
            "-",
            (Some(2_000.0), None),
        ),
        (
            "free numeric",
            "gpt-6.1-sol",
            "$0.00",
            "$0.00",
            (Some(0.0), Some(0.0)),
        ),
    ] {
        let text = OFFICIAL_PRICING.replacen(
            "| gpt-6.1-sol | $2.00 | $0.10 |",
            &format!("| {cell} | {input} | {cached} |"),
            1,
        );
        let table = PriceTable::from_pricing_md(&text).unwrap();
        let price = table
            .lookup(Some("gpt-6.1-sol"), None, ContextBand::Short)
            .unwrap();
        assert_eq!((price.input, price.cached_input), expected, "{label}");
    }
    let table = PriceTable::from_pricing_md(OFFICIAL_PRICING).unwrap();
    for model in [
        "text-embedding-3-small",
        "omni-moderation-latest",
        "gpt-realtime",
        "sora-2",
        "gpt-image-1",
    ] {
        assert!(
            !table.models.contains_key(model),
            "unrelated model: {model}"
        );
    }
}

#[test]
fn malformed_prices_preserve_old_table() {
    let good = PriceTable::from_pricing_md(OFFICIAL_PRICING).unwrap();
    let duplicate =
        "| gpt-6.1-sol | $2.00 | $0.10 | $2.50 | $10.00 | $4.00 | $0.20 | $5.00 | $15.00 |";
    assert!(
        PriceTable::from_pricing_md(&insert_row(
            OFFICIAL_PRICING,
            "### Standard pricing data",
            duplicate
        ))
        .is_ok()
    );
    for (label, text) in [
        ("HTML response", "<html>error</html>".to_owned()),
        (
            "negative",
            OFFICIAL_PRICING.replacen("$10.00", "$-10.00", 1),
        ),
        ("NaN", OFFICIAL_PRICING.replacen("$10.00", "$NaN", 1)),
        ("infinite", OFFICIAL_PRICING.replacen("$10.00", "$inf", 1)),
        (
            "units",
            OFFICIAL_PRICING.replace("Prices per 1M tokens.", "Prices per 1K tokens."),
        ),
        ("threshold", OFFICIAL_PRICING.replace("≤272K", "≤544K")),
        (
            "missing column",
            OFFICIAL_PRICING.replacen("Short context input", "Unknown input", 1),
        ),
        (
            "truncated row",
            OFFICIAL_PRICING.replacen(
                "| gpt-6-astra | $10.00",
                "| gpt-6-astra | extra | $10.00",
                1,
            ),
        ),
        (
            "conflicting duplicate",
            insert_row(
                OFFICIAL_PRICING,
                "### Standard pricing data",
                &duplicate.replace("$2.00", "$99.00"),
            ),
        ),
    ] {
        let mut table = good.clone();
        assert!(table.replace_markdown(&text, None).is_err(), "{label}");
        assert_eq!(table, good, "{label}");
    }
}

#[test]
fn lookup_only_allows_date_aliases() {
    let table = PriceTable::for_test(&[
        ("model-a", 2.0, 0.1, 10.0),
        ("model-a-2026-09-29", 3.0, 0.2, 11.0),
    ]);
    for (model, expected) in [
        ("model-a", Some(2.0)),
        ("model-a-2026-09-29", Some(3.0)),
        ("model-a-2026-01-15", Some(2.0)),
        ("model-a-2026-02-30", None),
        ("model-a-spark", None),
        ("model-a-max", None),
        ("model-a-x9", None),
    ] {
        assert_eq!(
            table
                .lookup(Some(model), None, ContextBand::Short)
                .ok()
                .and_then(|price| price.input),
            expected,
            "{model}"
        );
    }
    let mut table = PriceTable::from_pricing_md(OFFICIAL_PRICING).unwrap();
    table.models.insert(
        "gpt-6.1-sol-2026-01-15".to_owned(),
        ModelPrices {
            standard: table.models["gpt-6.1-sol"].standard.clone(),
            ..ModelPrices::default()
        },
    );
    assert_eq!(
        table.lookup(
            Some("gpt-6.1-sol-2026-01-15"),
            Some(&ServiceTier::Fast),
            ContextBand::Short
        ),
        Err(LookupError::MissingTier)
    );
}

#[test]
fn missing_context_and_tiers_remain_unknown() {
    let table = PriceTable::from_pricing_md(OFFICIAL_PRICING).unwrap();
    for (model, tier, context, expected) in [
        (
            "gpt-5.5",
            ServiceTier::Fast,
            ContextBand::Long,
            LookupError::MissingContext,
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Standard,
            ContextBand::Unknown,
            LookupError::UnknownContext,
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Ultrafast,
            ContextBand::Short,
            LookupError::MissingTier,
        ),
        (
            "gpt-6.1-sol",
            ServiceTier::Unknown("flex".to_owned()),
            ContextBand::Short,
            LookupError::UnknownTier,
        ),
    ] {
        assert_eq!(
            table.lookup(Some(model), Some(&tier), context),
            Err(expected),
            "{model}/{tier:?}/{context:?}"
        );
    }
    assert_eq!(
        table
            .lookup(Some("gpt-5.5-pro"), None, ContextBand::Short)
            .unwrap()
            .cached_input,
        None
    );
    for (input, expected) in [
        (Some(272_000), ContextBand::Short),
        (Some(272_001), ContextBand::Long),
        (None, ContextBand::Unknown),
    ] {
        assert_eq!(ContextBand::from_input(input), expected, "input={input:?}");
    }
}

#[test]
fn cache_roundtrip_and_failure_preservation() {
    let cache = TestCache::new();
    std::fs::write(
        cache.directory.join("models-dev-openai.json"),
        "legacy cache",
    )
    .unwrap();
    assert!(PriceTable::load_from(&cache.path()).is_empty());
    let mut table = PriceTable::default();
    table
        .replace_markdown(OFFICIAL_PRICING, Some(&cache.directory))
        .unwrap();
    assert_eq!(PriceTable::load_from(&cache.path()), table);
    let bytes = std::fs::read(cache.path()).unwrap();
    let original = table.clone();
    assert!(
        table
            .replace_markdown("broken", Some(&cache.directory))
            .is_err()
    );
    assert_eq!(table, original);
    assert_eq!(std::fs::read(cache.path()).unwrap(), bytes);
    let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["version"] = serde_json::json!(0);
    std::fs::write(cache.path(), serde_json::to_vec(&json).unwrap()).unwrap();
    assert!(PriceTable::load_from(&cache.path()).is_empty());
}

#[test]
fn cache_write_failure_keeps_new_prices() {
    let cache = TestCache::new();
    let blocked = cache.directory.join("file");
    std::fs::write(&blocked, "keep").unwrap();
    let mut table = PriceTable::default();
    table
        .replace_markdown(OFFICIAL_PRICING, Some(&blocked))
        .unwrap();
    assert!(!table.is_empty());
    assert_eq!(std::fs::read_to_string(blocked).unwrap(), "keep");
}

#[test]
fn fetch_failure_keeps_successful_cache() {
    let cache = TestCache::new();
    let mut table = PriceTable::default();
    table
        .replace_markdown(OFFICIAL_PRICING, Some(&cache.directory))
        .unwrap();
    let original = table.clone();
    let bytes = std::fs::read(cache.path()).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/pricing.md", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).unwrap() > 0);
        stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    assert!(table.refresh_from(&url, Some(&cache.directory)).is_err());
    server.join().unwrap();
    assert_eq!(table, original);
    assert_eq!(std::fs::read(cache.path()).unwrap(), bytes);
}

#[test]
#[ignore = "需联网读取 OpenAI 官方价格，不依赖用户登录态"]
fn live_official_prices_cover_local_models() {
    let cache = TestCache::new();
    let mut table = PriceTable::default();
    table
        .refresh_from(PRICING_URL, Some(&cache.directory))
        .unwrap();
    for model in [
        "gpt-6.1-sol",
        "gpt-6-sol",
        "gpt-6-astra",
        "gpt-5.6-sol",
        "gpt-5.6-luna",
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.3-codex",
    ] {
        assert!(
            table.lookup(Some(model), None, ContextBand::Short).is_ok(),
            "{model}"
        );
    }
    assert_eq!(
        table
            .lookup(
                Some("gpt-6-astra"),
                Some(&ServiceTier::Ultrafast),
                ContextBand::Long
            )
            .unwrap(),
        ModelPrice {
            input: Some(120.0),
            cached_input: Some(12.0),
            output: Some(450.0)
        }
    );
    assert_eq!(PriceTable::load_from(&cache.path()), table);
    println!(
        "official_models={}, eight_local_models_covered=true, ultrafast_long_verified=true, cached_restart_verified=true",
        table.models.len()
    );
}
