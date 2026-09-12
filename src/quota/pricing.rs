//! models.dev 价格表：把分桶 token 用量换算成 API 牌价等价美元。
//!
//! 数据两层兜底：app-data 里的上次成功拉取 → 空表（成本列显示 `--`）。
//! 没有内置快照——新模型月月发布，静态清单必然过期；首次安装离线时
//! 成本列为 `--` 是诚实未知，联网后 30 秒内由在线刷新补齐。

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::AppError;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const CACHE_FILE_NAME: &str = "models-dev-openai.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ModelPrice {
    /// 未缓存 input，USD / 1M tokens。
    pub(crate) input: f64,
    /// 缓存命中的 input（`cache_read`），USD / 1M tokens。
    pub(crate) cached_input: f64,
    /// output（含 reasoning），USD / 1M tokens。
    pub(crate) output: f64,
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PriceTable {
    models: HashMap<String, ModelPrice>,
    /// 最近一次成功拉取的 UNIX 秒时间戳；None 表示从未拉取过。
    fetched_at: Option<u64>,
}

impl PriceTable {
    /// app-data 上次成功拉取的价格表；没有（首次安装/文件损坏）时为
    /// 空表，成本列显示 `--`，等在线刷新补齐。
    pub(crate) fn load() -> Self {
        crate::config::app_data_dir()
            .ok()
            .and_then(|directory| std::fs::read_to_string(directory.join(CACHE_FILE_NAME)).ok())
            .and_then(|text| serde_json::from_str::<PriceSnapshot>(&text).ok())
            .map(PriceSnapshot::into_table)
            .unwrap_or_default()
    }

    /// 上次成功拉取的 UNIX 秒时间戳；从未拉取过则为 None。
    pub(crate) fn fetched_at(&self) -> Option<u64> {
        self.fetched_at
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// 模型匹配回退链：精确 → 去日期后缀 → 逐段剥右侧变体后缀（-max/-spark 等
    /// 回退到 base 模型价）。None 表示不计价——宁可少算不虚算。
    pub(crate) fn lookup(&self, model: Option<&str>) -> Option<ModelPrice> {
        let model = model?;
        if let Some(price) = self.models.get(model) {
            return Some(*price);
        }
        if let Some((base, suffix)) = model.rsplit_once('-')
            && is_release_date(suffix)
            && let Some(price) = self.models.get(base)
        {
            return Some(*price);
        }
        let mut prefix = model;
        while let Some((stripped, _)) = prefix.rsplit_once('-') {
            prefix = stripped;
            if let Some(price) = self.models.get(prefix) {
                return Some(*price);
            }
        }
        None
    }

    /// 从 models.dev 拉取并替换内存价格表，成功后写入 app-data 缓存。
    /// 失败时保留现有表，由调用方决定是否记录日志。
    pub(crate) fn refresh(&mut self) -> Result<(), AppError> {
        // Windows 上 native-tls 走系统 SChannel：不打包 rustls+ring，
        // 证书验证用系统证书库。provider 默认是 Rustls 且不会被自动
        // 拾取，必须显式设置。
        let agent = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    // 用系统证书库（Windows 证书存储随系统更新），而非
                    // 打包 Mozilla 根证书数据。
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .timeout_global(Some(FETCH_TIMEOUT))
            .build()
            .new_agent();
        let mut response = agent
            .get(MODELS_DEV_URL)
            .call()
            .map_err(|error| AppError::Protocol(format!("models.dev 拉取失败：{error}")))?;
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| AppError::Protocol(format!("models.dev 响应读取失败：{error}")))?;
        let table = Self::from_models_dev(&body)
            .ok_or_else(|| AppError::Protocol("models.dev 响应缺少 openai 价格".to_owned()))?;
        if let Ok(directory) = crate::config::app_data_dir()
            && let Ok(json) = serde_json::to_string(&PriceSnapshot::from(&table))
        {
            let _ = std::fs::write(directory.join(CACHE_FILE_NAME), json);
        }
        *self = table;
        Ok(())
    }

    pub(crate) fn from_models_dev(text: &str) -> Option<Self> {
        let api: ModelsDevApi = serde_json::from_str(text).ok()?;
        let provider = api.openai?;
        Some(Self {
            models: provider
                .models
                .into_iter()
                .filter_map(|(id, model)| {
                    let cost = model.cost?;
                    Some((
                        id,
                        ModelPrice {
                            input: cost.input,
                            // models.dev 缺 cache_read 时按原价上界处理。
                            cached_input: cost.cache_read.unwrap_or(cost.input),
                            output: cost.output,
                        },
                    ))
                })
                .collect(),
            fetched_at: Some(unix_now()),
        })
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[derive(Serialize, Deserialize, Default)]
struct PriceSnapshot {
    #[serde(default)]
    models: HashMap<String, ModelPriceEntry>,
    /// 拉取时刻的 UNIX 秒时间戳；旧格式文件缺此字段按从未拉取处理。
    #[serde(default)]
    fetched_at: Option<u64>,
}

impl PriceSnapshot {
    fn into_table(self) -> PriceTable {
        PriceTable {
            fetched_at: self.fetched_at,
            models: self
                .models
                .into_iter()
                .map(|(id, entry)| {
                    (
                        id,
                        ModelPrice {
                            input: entry.input,
                            cached_input: entry.cached_input,
                            output: entry.output,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl From<&PriceTable> for PriceSnapshot {
    fn from(table: &PriceTable) -> Self {
        Self {
            fetched_at: table.fetched_at,
            models: table
                .models
                .iter()
                .map(|(id, price)| {
                    (
                        id.clone(),
                        ModelPriceEntry {
                            input: price.input,
                            cached_input: price.cached_input,
                            output: price.output,
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ModelPriceEntry {
    input: f64,
    cached_input: f64,
    output: f64,
}

#[derive(Deserialize, Default)]
struct ModelsDevApi {
    #[serde(default)]
    openai: Option<ModelsDevProvider>,
}

#[derive(Deserialize, Default)]
struct ModelsDevProvider {
    #[serde(default)]
    models: HashMap<String, ModelsDevModel>,
}

#[derive(Deserialize, Default)]
struct ModelsDevModel {
    #[serde(default)]
    cost: Option<ModelsDevCost>,
}

#[derive(Deserialize)]
struct ModelsDevCost {
    input: f64,
    output: f64,
    #[serde(default)]
    cache_read: Option<f64>,
}

/// 形如 `2026-01-15` 的发布日期后缀。
fn is_release_date(suffix: &str) -> bool {
    suffix.len() == 10
        && suffix.starts_with('2')
        && suffix
            .chars()
            .all(|character| character.is_ascii_digit() || character == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table_with(entries: &[(&str, f64, f64, f64)]) -> PriceTable {
        PriceTable {
            fetched_at: None,
            models: entries
                .iter()
                .map(|(id, input, cached, output)| {
                    (
                        (*id).to_owned(),
                        ModelPrice {
                            input: *input,
                            cached_input: *cached,
                            output: *output,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn lookup_matches_exact_model() {
        let table = table_with(&[("gpt-5.6-sol", 4.0, 0.4, 20.0)]);

        assert_eq!(
            table.lookup(Some("gpt-5.6-sol")),
            Some(ModelPrice {
                input: 4.0,
                cached_input: 0.4,
                output: 20.0
            })
        );
    }

    #[test]
    fn lookup_strips_variant_suffixes_down_to_base_model() {
        let table = table_with(&[("gpt-5.1", 1.10, 0.11, 9.0)]);

        assert_eq!(
            table.lookup(Some("gpt-5.1-codex-max")),
            Some(ModelPrice {
                input: 1.10,
                cached_input: 0.11,
                output: 9.0
            })
        );
    }

    #[test]
    fn lookup_strips_release_date_suffix() {
        let table = table_with(&[("gpt-5.6-sol", 4.0, 0.4, 20.0)]);

        assert!(table.lookup(Some("gpt-5.6-sol-2026-01-15")).is_some());
    }

    #[test]
    fn lookup_returns_none_for_unknown_family_or_missing_name() {
        let table = table_with(&[("gpt-5.6-sol", 4.0, 0.4, 20.0)]);

        assert_eq!(table.lookup(Some("claude-opus")), None);
        // 逐段剥离回退是设计行为：未知后缀（-x9）按 base 模型价近似。
        assert_eq!(
            table.lookup(Some("gpt-5.6-sol-x9")),
            Some(ModelPrice {
                input: 4.0,
                cached_input: 0.4,
                output: 20.0
            })
        );
        assert_eq!(table.lookup(None), None);
    }

    #[test]
    fn parses_models_dev_response_with_unknown_fields() {
        // 真实响应带 tiers/modes/experimental 等字段，反序列化必须容忍。
        let text = r#"{"openai":{"id":"openai","models":{"gpt-5.6-sol":{"id":"gpt-5.6-sol","cost":{"input":4,"output":20,"cache_read":0.4,"cache_write":5,"tiers":[{"input":8}]},"experimental":{}},"gpt-no-cache":{"cost":{"input":2,"output":8}}}}}"#;

        let table = PriceTable::from_models_dev(text).expect("应解析出 openai 价格表");

        assert_eq!(
            table.lookup(Some("gpt-5.6-sol")),
            Some(ModelPrice {
                input: 4.0,
                cached_input: 0.4,
                output: 20.0
            })
        );
        // 缺 cache_read 的条目按原价上界处理。
        assert_eq!(
            table.lookup(Some("gpt-no-cache")),
            Some(ModelPrice {
                input: 2.0,
                cached_input: 2.0,
                output: 8.0
            })
        );
    }

    #[test]
    fn models_dev_response_without_openai_prices_is_rejected() {
        assert!(PriceTable::from_models_dev(r#"{"other":{}}"#).is_none());
    }

    #[test]
    fn models_dev_parse_stamps_fetched_at_and_round_trips() {
        let table = PriceTable::from_models_dev(
            r#"{"openai":{"models":{"gpt-5.6-sol":{"cost":{"input":4,"output":20,"cache_read":0.4}}}}}"#,
        )
        .expect("应解析出价格表");

        let fetched_at = table.fetched_at().expect("解析时刻应被记录");
        let json = serde_json::to_string(&PriceSnapshot::from(&table)).unwrap();
        let parsed: PriceSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.fetched_at, Some(fetched_at));
        assert_eq!(parsed.into_table().fetched_at(), Some(fetched_at));
    }

    /// 真实网络冒烟测试：验证当前 TLS 后端能完成握手，且线上价格表
    /// 覆盖本机日志中在用的模型。平时跳过，需要时手动运行：
    /// `cargo test --release native_tls_smoke -- --ignored`
    #[test]
    #[ignore = "需要外网"]
    fn native_tls_smoke_fetches_models_dev() {
        let mut table = PriceTable::load();
        table.refresh().expect("TLS 握手与解析应成功");
        assert!(!table.is_empty());

        for model in [
            "gpt-5.6-sol",
            "gpt-6-astra",
            "gpt-5.6-luna",
            "gpt-5.3-codex",
        ] {
            assert!(
                table.lookup(Some(model)).is_some(),
                "线上价格表缺少 {model}"
            );
        }
    }
}
