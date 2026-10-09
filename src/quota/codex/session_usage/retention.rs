use std::collections::{HashMap, HashSet};

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{SystemTimeToFileTime, TzSpecificLocalTimeToSystemTime};

use super::super::PeriodBoundary;
use super::model::{FileCache, ParentLink};
use super::parser::{source_peak_advances, system_time_from_unix_nanos};
use crate::quota::codex::protocol::local_calendar_date_at;

/// Keep the entire first required local day: events before the precise period
/// start on that day still anchor coverage and the overflow transition.
pub(super) fn required_start_nanos(today: &str, boundary: Option<&PeriodBoundary>) -> Option<i64> {
    let date = boundary.map_or(today, |boundary| today.min(boundary.local_date()));
    local_midnight_nanos(date)
}

fn local_midnight_nanos(date: &str) -> Option<i64> {
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;
    let mut parts = date.split('-');
    let year = parts.next()?.parse::<u16>().ok()?;
    let month = parts.next()?.parse::<u16>().ok()?;
    let day = parts.next()?.parse::<u16>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    time::Date::from_calendar_date(
        i32::from(year),
        time::Month::try_from(u8::try_from(month).ok()?).ok()?,
        u8::try_from(day).ok()?,
    )
    .ok()?;
    let local = SYSTEMTIME {
        wYear: year,
        wMonth: month,
        wDay: day,
        ..SYSTEMTIME::default()
    };
    let mut utc = SYSTEMTIME::default();
    let mut file_time = FILETIME::default();
    // SAFETY: all structures are initialized and the output buffers are
    // exclusively borrowed; None uses the system's current time zone.
    unsafe {
        TzSpecificLocalTimeToSystemTime(None, &raw const local, &raw mut utc).ok()?;
        SystemTimeToFileTime(&raw const utc, &raw mut file_time).ok()?;
    }
    let ticks = (u64::from(file_time.dwHighDateTime) << 32) | u64::from(file_time.dwLowDateTime);
    let nanos = i64::try_from(ticks.checked_sub(UNIX_EPOCH_TICKS)?.checked_mul(100)?).ok()?;
    // Midnight may not exist in a time zone with a clock transition. Retain
    // complete history if the platform cannot round-trip the requested date.
    (local_calendar_date_at(system_time_from_unix_nanos(nanos)?).as_deref() == Some(date))
        .then_some(nanos)
}

pub(super) fn needs_history(cache: &FileCache, start_nanos: Option<i64>) -> bool {
    cache
        .history_start_nanos
        .is_some_and(|retained| start_nanos.is_none_or(|requested| requested < retained))
}

/// Prefix matching and fork replay need complete signatures, including those
/// outside the display windows. Protect all copies and ancestor dependencies.
pub(super) fn protected_paths(
    caches: &HashMap<String, FileCache>,
    rollout_index: &HashMap<String, Vec<String>>,
    dependencies: &HashSet<String>,
) -> HashSet<String> {
    let mut protected = dependencies.clone();
    for paths in rollout_index.values().filter(|paths| paths.len() > 1) {
        protected.extend(paths.iter().cloned());
    }
    for (path, cache) in caches {
        if let Some(root) = cache.root.as_ref()
            && let ParentLink::Parent(parent) = &root.parent
        {
            protected.insert(path.clone());
            if let Some(paths) = rollout_index.get(parent) {
                protected.extend(paths.iter().cloned());
            }
        }
    }
    protected
}

#[derive(Debug, Default)]
pub(super) struct PrunedHistory {
    pub(super) events: usize,
    pub(super) balances: usize,
}

pub(super) fn compact_history(cache: &mut FileCache, start_nanos: i64) -> PrunedHistory {
    if cache.uncertain
        || cache.token_without_timestamp
        || cache.root.as_ref().is_none_or(|root| {
            root.provider.as_deref() != Some("openai")
                || root.thread_id.is_none()
                || !matches!(root.parent, ParentLink::None)
        })
    {
        return PrunedHistory::default();
    }
    // Only trim a consecutive prefix. Out-of-order old events later in the
    // file must keep their position relative to in-window cap changes.
    let prefix = cache
        .events
        .iter()
        .position(|event| {
            event
                .timestamp_nanos
                .is_none_or(|timestamp| timestamp >= start_nanos)
        })
        .unwrap_or(cache.events.len());
    let mut peaks: HashMap<Option<&str>, usize> = HashMap::new();
    for (index, event) in cache.events[..prefix].iter().enumerate() {
        let previous = peaks.get(&event.source.as_deref()).map(|previous| {
            let event = &cache.events[*previous];
            (&event.signature, event.model.as_deref())
        });
        if source_peak_advances(previous, &event.signature, event.model.as_deref()) {
            peaks.insert(event.source.as_deref(), index);
        }
    }
    let mut keep: HashSet<usize> = peaks.into_values().collect();
    if prefix > 0 {
        // Even a zero-delta snapshot supplies the adjacent signature and cap
        // state required by the first retained request.
        keep.insert(prefix - 1);
    }
    let before_events = cache.events.len();
    let mut index = 0;
    cache.events.retain(|_| {
        let retain = index >= prefix || keep.contains(&index);
        index += 1;
        retain
    });
    let before_balances = cache.balance_observations.len();
    // Debits require BOTH endpoints inside a window. Historical observations
    // cannot contribute; retain every in-window value, including repeats.
    cache
        .balance_observations
        .retain(|observation| observation.timestamp_nanos >= start_nanos);
    let pruned = PrunedHistory {
        events: before_events - cache.events.len(),
        balances: before_balances - cache.balance_observations.len(),
    };
    if pruned.events != 0 || pruned.balances != 0 {
        cache.history_start_nanos = Some(start_nanos);
        cache.events.shrink_to_fit();
        cache.balance_observations.shrink_to_fit();
    }
    pruned
}

#[cfg(test)]
mod tests;
