use std::collections::{HashMap, HashSet};

use super::{ContextPrices, ModelPrice, ModelPrices, ServiceTier};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Flagship,
    Cyber,
    Specialized,
    Other,
}

pub(super) fn parse_prices(text: &str) -> Result<HashMap<String, ModelPrices>, String> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut section = Section::Other;
    let mut tier = None;
    let mut token_units = false;
    let mut threshold = false;
    let mut core_tiers = HashSet::new();
    let mut models: HashMap<String, ModelPrices> = HashMap::new();
    let mut index = 0;
    while let Some(line) = lines.get(index).copied() {
        let heading = line.trim_start_matches('#').trim();
        let next_section = match heading {
            "Flagship models" => Some(Section::Flagship),
            "Cyber models" => Some(Section::Cyber),
            "Specialized models" => Some(Section::Specialized),
            "GPT-Live sessions"
            | "Realtime and audio generation models"
            | "Image generation models"
            | "Transcription models"
            | "Fine-tuning models"
            | "Fine-tuning"
            | "Finetuning"
            | "Video generation models"
            | "Built-in tools" => Some(Section::Other),
            _ => None,
        };
        if let Some(next) = next_section {
            section = next;
            token_units = false;
            tier = (next == Section::Cyber).then_some(ServiceTier::Standard);
        }
        match heading {
            "Standard" | "Standard pricing data" => tier = Some(ServiceTier::Standard),
            "Fast" | "Fast pricing data" => tier = Some(ServiceTier::Fast),
            "Ultrafast" | "Ultrafast pricing data" => tier = Some(ServiceTier::Ultrafast),
            "Batch" | "Batch pricing data" | "Flex" | "Flex pricing data" => tier = None,
            _ => {}
        }
        if line == "Prices per 1M tokens." {
            token_units = true;
        }
        if line.starts_with("Short context:") {
            let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            if compact != "Shortcontext:≤272Kinputtokens.Longcontext:>272Kinputtokens." {
                return Err("短/长上下文阈值说明不匹配 272K".to_owned());
            }
            threshold = true;
        }
        if line.starts_with('|') {
            let start = index;
            while lines.get(index).is_some_and(|line| line.starts_with('|')) {
                index += 1;
            }
            if section != Section::Other
                && let Some(tier) = &tier
            {
                if !token_units {
                    return Err("目标价格表缺少 USD / 1M tokens 单位说明".to_owned());
                }
                let entries = parse_table(&lines[start..index], section)?;
                if section == Section::Flagship
                    && entries.iter().any(|(_, prices)| match prices {
                        ContextPrices::Flat { price } => price.has_price(),
                        ContextPrices::Split { short, .. } => short.has_price(),
                    })
                {
                    core_tiers.insert(tier.clone());
                }
                for (model, prices) in entries {
                    models
                        .entry(model.clone())
                        .or_default()
                        .insert(tier, prices)
                        .map_err(|error| format!("{model}：{error}"))?;
                }
            }
            continue;
        }
        index += 1;
    }
    if !threshold
        || [
            ServiceTier::Standard,
            ServiceTier::Fast,
            ServiceTier::Ultrafast,
        ]
        .iter()
        .any(|tier| !core_tiers.contains(tier))
    {
        return Err("缺少有效的 Standard/Fast/Ultrafast 主表或上下文阈值".to_owned());
    }
    Ok(models)
}

fn split_cells(line: &str) -> Vec<&str> {
    line.trim_matches('|').split('|').map(str::trim).collect()
}

fn parse_table(lines: &[&str], section: Section) -> Result<Vec<(String, ContextPrices)>, String> {
    let headers = split_cells(lines[0]);
    if headers.iter().collect::<HashSet<_>>().len() != headers.len() {
        return Err("价格表含重复列名".to_owned());
    }
    let column = |name: &str| {
        headers
            .iter()
            .position(|header| *header == name)
            .ok_or_else(|| format!("目标价格表缺少列：{name}"))
    };
    let model_index = column("Model")?;
    let category_index = if section == Section::Specialized {
        Some(column("Category")?)
    } else {
        None
    };
    let names = if section == Section::Specialized {
        ["Input", "Cached input", "Output"]
    } else {
        [
            "Short context input",
            "Short context cached input",
            "Short context output",
        ]
    };
    let short_columns = [column(names[0])?, column(names[1])?, column(names[2])?];
    let long_columns = if section == Section::Specialized {
        None
    } else {
        Some([
            column("Long context input")?,
            column("Long context cached input")?,
            column("Long context output")?,
        ])
    };
    if !lines.get(1).is_some_and(|line| {
        let cells = split_cells(line);
        cells.len() == headers.len()
            && cells.iter().all(|cell| {
                cell.contains('-') && cell.chars().all(|c| matches!(c, '-' | ':' | ' '))
            })
    }) {
        return Err("目标价格表分隔行损坏".to_owned());
    }
    let mut entries = Vec::new();
    for line in &lines[2..] {
        let cells = split_cells(line);
        if cells.len() != headers.len() {
            return Err("目标价格表行列数不匹配".to_owned());
        }
        if category_index.is_some_and(|index| matches!(cells[index], "Embedding" | "Moderation")) {
            continue;
        }
        let cell = cells[model_index];
        let (model, note) = cell.split_once(" (").unwrap_or((cell, ""));
        let model = model.trim_matches('`');
        if model.is_empty()
            || !model
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
        {
            return Err(format!("无法识别模型名：{cell}"));
        }
        let short = parse_rates(&cells, short_columns)?;
        let long = long_columns
            .map(|columns| parse_rates(&cells, columns))
            .transpose()?
            .filter(|price| price.has_price());
        let restricted = note.contains("272K") && note.contains("context length");
        let prices = if long.is_some() || restricted {
            ContextPrices::Split { short, long }
        } else {
            ContextPrices::Flat { price: short }
        };
        entries.push((model.to_owned(), prices));
    }
    Ok(entries)
}

fn parse_rates(cells: &[&str], columns: [usize; 3]) -> Result<ModelPrice, String> {
    Ok(ModelPrice {
        input: parse_price(cells[columns[0]])?,
        cached_input: parse_price(cells[columns[1]])?,
        output: parse_price(cells[columns[2]])?,
    })
}

fn parse_price(cell: &str) -> Result<Option<f64>, String> {
    if cell == "-" {
        return Ok(None);
    }
    let value = cell
        .strip_prefix('$')
        .and_then(|number| number.replace(',', "").parse::<f64>().ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or_else(|| format!("无效的美元单价：{cell}"))?;
    Ok(Some(value))
}
