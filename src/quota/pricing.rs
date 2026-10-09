//! 官方 API 价格：下载 Markdown，校验后保存结构化缓存。

mod markdown;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::error::AppError;

const PRICING_URL: &str = "https://developers.openai.com/api/docs/pricing.md";
const CACHE_FILE_NAME: &str = "openai-pricing-v1.json";
const CACHE_VERSION: u32 = 1;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SHORT_CONTEXT_LIMIT: u64 = 272_000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceTier {
    Standard,
    Fast,
    Ultrafast,
    Unknown(String),
}

impl ServiceTier {
    pub(crate) fn from_codex(value: &str) -> Self {
        match value {
            "default" | "standard" => Self::Standard,
            "priority" | "fast" => Self::Fast,
            "ultrafast" => Self::Ultrafast,
            _ => Self::Unknown(value.to_owned()),
        }
    }

    pub(crate) fn label(&self) -> &str {
        match self {
            Self::Standard => "Standard",
            Self::Fast => "Fast",
            Self::Ultrafast => "Ultrafast",
            Self::Unknown(value) => value,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum ContextBand {
    Short,
    Long,
    #[default]
    Unknown,
}

impl ContextBand {
    pub(crate) fn from_input(input: Option<u64>) -> Self {
        match input {
            Some(value) if value <= SHORT_CONTEXT_LIMIT => Self::Short,
            Some(_) => Self::Long,
            None => Self::Unknown,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Short => "短上下文",
            Self::Long => "长上下文",
            Self::Unknown => "上下文未知",
        }
    }
}

/// USD / 1M tokens。缺失单价只在对应的 token 数非零时阻止计价。
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelPrice {
    pub(crate) input: Option<f64>,
    pub(crate) cached_input: Option<f64>,
    pub(crate) output: Option<f64>,
}

impl ModelPrice {
    fn has_price(self) -> bool {
        self.input.is_some() || self.cached_input.is_some() || self.output.is_some()
    }

    fn valid(self) -> bool {
        [self.input, self.cached_input, self.output]
            .into_iter()
            .flatten()
            .all(|price| price.is_finite() && price >= 0.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ContextPrices {
    Flat {
        price: ModelPrice,
    },
    Split {
        short: ModelPrice,
        long: Option<ModelPrice>,
    },
}

impl ContextPrices {
    fn lookup(&self, band: ContextBand) -> Result<ModelPrice, LookupError> {
        match (self, band) {
            (Self::Flat { price }, _) | (Self::Split { short: price, .. }, ContextBand::Short) => {
                Ok(*price)
            }
            (Self::Split { long, .. }, ContextBand::Long) => {
                long.ok_or(LookupError::MissingContext)
            }
            (Self::Split { .. }, ContextBand::Unknown) => Err(LookupError::UnknownContext),
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Flat { price } => price.valid(),
            Self::Split { short, long } => short.valid() && long.is_none_or(ModelPrice::valid),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct ModelPrices {
    standard: Option<ContextPrices>,
    fast: Option<ContextPrices>,
    ultrafast: Option<ContextPrices>,
}

impl ModelPrices {
    fn get(&self, tier: &ServiceTier) -> Option<&ContextPrices> {
        match tier {
            ServiceTier::Standard => self.standard.as_ref(),
            ServiceTier::Fast => self.fast.as_ref(),
            ServiceTier::Ultrafast => self.ultrafast.as_ref(),
            ServiceTier::Unknown(_) => None,
        }
    }

    fn insert(&mut self, tier: &ServiceTier, prices: ContextPrices) -> Result<(), String> {
        let slot = match tier {
            ServiceTier::Standard => &mut self.standard,
            ServiceTier::Fast => &mut self.fast,
            ServiceTier::Ultrafast => &mut self.ultrafast,
            ServiceTier::Unknown(_) => return Err("未支持的价格档位".to_owned()),
        };
        if slot.as_ref().is_some_and(|existing| *existing != prices) {
            return Err(format!("{} 重复价格冲突", tier.label()));
        }
        *slot = Some(prices);
        Ok(())
    }

    fn valid(&self) -> bool {
        [&self.standard, &self.fast, &self.ultrafast]
            .into_iter()
            .flatten()
            .all(ContextPrices::valid)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LookupError {
    MissingModel,
    MissingTier,
    MissingContext,
    UnknownTier,
    UnknownContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PriceField {
    Model,
    Tier,
    Context,
    Input,
    CachedInput,
    Output,
}

/// 补拉和闭环必须检查同一模型、档位、上下文及价格项。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct PriceGap {
    pub(crate) model: String,
    pub(crate) tier: ServiceTier,
    pub(crate) context: ContextBand,
    pub(crate) field: PriceField,
}

impl PriceGap {
    pub(crate) fn description(&self) -> String {
        let field = match self.field {
            PriceField::Model => "模型条目",
            PriceField::Tier => "档位价格",
            PriceField::Context => "上下文价格",
            PriceField::Input => "输入价格",
            PriceField::CachedInput => "缓存输入价格",
            PriceField::Output => "输出价格",
        };
        format!(
            "{} / {} / {} / {field}",
            self.model,
            self.tier.label(),
            self.context.label()
        )
    }

    pub(crate) fn resolved(&self, table: &PriceTable) -> bool {
        let Ok(price) = table.lookup(Some(&self.model), Some(&self.tier), self.context) else {
            return false;
        };
        match self.field {
            PriceField::Model | PriceField::Tier | PriceField::Context => true,
            PriceField::Input => price.input.is_some(),
            PriceField::CachedInput => price.cached_input.is_some(),
            PriceField::Output => price.output.is_some(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct PriceTable {
    models: HashMap<String, ModelPrices>,
    fetched_at: Option<u64>,
}

impl PriceTable {
    pub(crate) fn load() -> Self {
        crate::config::app_data_dir()
            .ok()
            .map(|directory| Self::load_from(&directory.join(CACHE_FILE_NAME)))
            .unwrap_or_default()
    }

    fn load_from(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<PriceSnapshot>(&text).ok())
            .filter(|snapshot| {
                snapshot.version == CACHE_VERSION
                    && snapshot.models.values().all(ModelPrices::valid)
            })
            .map(|snapshot| Self {
                models: snapshot.models,
                fetched_at: snapshot.fetched_at,
            })
            .unwrap_or_default()
    }

    pub(crate) fn fetched_at(&self) -> Option<u64> {
        self.fetched_at
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub(crate) fn lookup(
        &self,
        model: Option<&str>,
        tier: Option<&ServiceTier>,
        context: ContextBand,
    ) -> Result<ModelPrice, LookupError> {
        let tier = tier.unwrap_or(&ServiceTier::Standard);
        if matches!(tier, ServiceTier::Unknown(_)) {
            return Err(LookupError::UnknownTier);
        }
        let model = model.ok_or(LookupError::MissingModel)?;
        let prices = self
            .models
            .get(model)
            .or_else(|| release_base(model).and_then(|base| self.models.get(base)))
            .ok_or(LookupError::MissingModel)?;
        prices
            .get(tier)
            .ok_or(LookupError::MissingTier)?
            .lookup(context)
    }

    /// 解析完全成功才替换旧表；网络和格式错误均保留成功缓存。
    pub(crate) fn refresh(&mut self) -> Result<(), AppError> {
        self.refresh_from(PRICING_URL, crate::config::app_data_dir().ok().as_deref())
    }

    fn refresh_from(&mut self, url: &str, directory: Option<&Path>) -> Result<(), AppError> {
        let body = Self::fetch_markdown(url)?;
        self.replace_markdown(&body, directory)
    }

    fn fetch_markdown(url: &str) -> Result<String, AppError> {
        let agent = ureq::Agent::config_builder()
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::NativeTls)
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .timeout_global(Some(FETCH_TIMEOUT))
            .build()
            .new_agent();
        let mut response = agent
            .get(url)
            .header("User-Agent", "codex-quota")
            .header("Accept", "text/markdown")
            .call()
            .map_err(|error| AppError::Protocol(format!("OpenAI 价格拉取失败：{error}")))?;
        response
            .body_mut()
            .read_to_string()
            .map_err(|error| AppError::Protocol(format!("OpenAI 价格响应读取失败：{error}")))
    }

    fn replace_markdown(&mut self, body: &str, directory: Option<&Path>) -> Result<(), AppError> {
        let table = Self::from_pricing_md(body)
            .map_err(|error| AppError::Protocol(format!("OpenAI 价格解析失败：{error}")))?;
        if let Some(directory) = directory
            && let Err(error) = table.save_to(&directory.join(CACHE_FILE_NAME))
        {
            crate::logging::log(&format!("OpenAI 价格缓存写入失败：{error}"));
        }
        *self = table;
        Ok(())
    }

    fn save_to(&self, path: &Path) -> Result<(), std::io::Error> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;
        let directory = path
            .parent()
            .ok_or_else(|| std::io::Error::other("价格缓存没有父目录"))?;
        std::fs::create_dir_all(directory)?;
        let snapshot = PriceSnapshot {
            version: CACHE_VERSION,
            fetched_at: self.fetched_at,
            models: self.models.clone(),
        };
        let bytes = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, bytes)?;
        let from: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: 两个缓冲区均以 NUL 结尾，并在调用期间存活。
        unsafe {
            MoveFileExW(
                PCWSTR(from.as_ptr()),
                PCWSTR(to.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(std::io::Error::other)
    }

    pub(crate) fn from_pricing_md(text: &str) -> Result<Self, String> {
        Ok(Self {
            models: markdown::parse_prices(text)?,
            fetched_at: Some(unix_now()),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(entries: &[(&str, f64, f64, f64)]) -> Self {
        Self {
            fetched_at: None,
            models: entries
                .iter()
                .map(|(id, input, cached, output)| {
                    (
                        (*id).to_owned(),
                        ModelPrices {
                            standard: Some(ContextPrices::Flat {
                                price: ModelPrice {
                                    input: Some(*input),
                                    cached_input: Some(*cached),
                                    output: Some(*output),
                                },
                            }),
                            ..ModelPrices::default()
                        },
                    )
                })
                .collect(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct PriceSnapshot {
    version: u32,
    fetched_at: Option<u64>,
    models: HashMap<String, ModelPrices>,
}

fn release_base(model: &str) -> Option<&str> {
    let start = model.len().checked_sub(11)?;
    if model.as_bytes()[start] != b'-' {
        return None;
    }
    let date = model.get(start + 1..)?;
    let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
    time::Date::parse(date, &format).ok()?;
    model.get(..start)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
pub(crate) const OFFICIAL_PRICING: &str = include_str!("pricing/fixtures/openai-pricing.md");

#[cfg(test)]
mod tests;
