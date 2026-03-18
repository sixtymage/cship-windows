//! Daily cost module — renders today's total Claude Code spend across all sessions.
//!
//! Scans `~/.claude/projects/**/*.jsonl`, filters assistant messages timestamped today (UTC),
//! sums token counts by model, applies Anthropic per-model pricing, and caches for 60s.
//!
//! No authentication required — reads only local files written by Claude Code.
//! Works on Windows with LiteLLM proxy (auth-agnostic local file scanning).

use crate::config::{CshipConfig, DailyCostConfig};
use crate::context::Context;

/// Renders `$cship.daily_cost` — total spend today across all Claude Code sessions.
pub fn render(_ctx: &Context, cfg: &CshipConfig) -> Option<String> {
    let dc_cfg = cfg.daily_cost.as_ref();

    if dc_cfg.and_then(|c| c.disabled).unwrap_or(false) {
        return None;
    }

    let cost = match compute_daily_cost(dc_cfg) {
        Some(c) => c,
        None => {
            tracing::warn!("cship.daily_cost: could not compute daily cost");
            return None;
        }
    };

    let symbol = dc_cfg.and_then(|c| c.symbol.as_deref()).unwrap_or("");
    let style = dc_cfg.and_then(|c| c.style.as_deref());
    let formatted = format!("${:.2}", cost);

    let warn_threshold = dc_cfg.and_then(|c| c.warn_threshold);
    let warn_style = dc_cfg.and_then(|c| c.warn_style.as_deref());
    let critical_threshold = dc_cfg.and_then(|c| c.critical_threshold);
    let critical_style = dc_cfg.and_then(|c| c.critical_style.as_deref());

    if let Some(fmt) = dc_cfg.and_then(|c| c.format.as_deref()) {
        let effective_style = crate::ansi::resolve_threshold_style(
            Some(cost),
            style,
            warn_threshold,
            warn_style,
            critical_threshold,
            critical_style,
        );
        return crate::format::apply_module_format(
            fmt,
            Some(&formatted),
            Some(symbol),
            effective_style,
        );
    }

    let content = format!("{symbol}{formatted}");
    Some(crate::ansi::apply_style_with_threshold(
        &content,
        Some(cost),
        style,
        warn_threshold,
        warn_style,
        critical_threshold,
        critical_style,
    ))
}

// ── Internal implementation ───────────────────────────────────────────────────

/// Compute today's total cost: check cache first, scan JSONL files on miss.
fn compute_daily_cost(cfg: Option<&DailyCostConfig>) -> Option<f64> {
    let claude_dir = claude_data_dir()?;
    let today = today_utc_date();
    let ttl = cfg.and_then(|c| c.ttl).unwrap_or(60);

    let cache_path = claude_dir.join("cache").join("cship-daily-cost.json");
    if let Some(cached) = read_cache(&cache_path, &today) {
        return Some(cached);
    }

    let projects_dir = claude_dir.join("projects");
    let cost = sum_daily_cost(&projects_dir, &today);
    write_cache(&cache_path, &today, cost, ttl);
    Some(cost)
}

/// Resolve `~/.claude` on Windows (USERPROFILE) and Unix (HOME).
fn claude_data_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let dir = std::path::Path::new(&home).join(".claude");
    if dir.is_dir() {
        Some(dir)
    } else {
        tracing::warn!(
            "cship.daily_cost: ~/.claude not found at {}",
            dir.display()
        );
        None
    }
}

/// Read a valid, non-expired daily cost cache entry for `today`.
fn read_cache(path: &std::path::Path, today: &str) -> Option<f64> {
    let content = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;

    if v.get("date")?.as_str()? != today {
        return None; // stale: different date
    }

    let expires_at = v.get("expires_at")?.as_u64()?;
    let now = now_secs();
    if now >= expires_at {
        return None; // expired
    }

    v.get("cost")?.as_f64()
}

/// Write a daily cost cache entry with the given TTL (seconds).
fn write_cache(path: &std::path::Path, today: &str, cost: f64, ttl: u64) {
    let cache = serde_json::json!({
        "date": today,
        "cost": cost,
        "expires_at": now_secs() + ttl,
    });
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, cache.to_string());
}

/// Walk `~/.claude/projects/` and sum today's token costs from all JSONL files.
fn sum_daily_cost(projects_dir: &std::path::Path, today: &str) -> f64 {
    let mut total = 0.0f64;

    let project_entries = match std::fs::read_dir(projects_dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                "cship.daily_cost: cannot read {}: {e}",
                projects_dir.display()
            );
            return 0.0;
        }
    };

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let session_entries = match std::fs::read_dir(&project_path) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for session_entry in session_entries.flatten() {
            let session_path = session_entry.path();
            if session_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // Skip files not modified today — avoids reading old sessions
            if !modified_today(&session_path, today) {
                continue;
            }
            total += cost_from_jsonl(&session_path, today);
        }
    }

    total
}

/// Return `true` if the file's mtime falls within today (UTC).
fn modified_today(path: &std::path::Path, today: &str) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return true, // can't check — process to be safe
    };
    let mtime = match meta.modified() {
        Ok(t) => t,
        Err(_) => return true,
    };
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    epoch_secs_to_date(secs) == today
}

/// Parse a JSONL file and return the total cost of today's completed API calls.
///
/// Only counts assistant messages that have `output_tokens` present (completed
/// calls). Streaming partials lack `output_tokens` and are skipped to avoid
/// double-counting input/cache tokens.
fn cost_from_jsonl(path: &std::path::Path, today: &str) -> f64 {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0.0,
    };

    let mut total = 0.0f64;

    for line in content.lines() {
        // Fast pre-filter before JSON parse
        if !line.contains("\"assistant\"") || !line.contains("\"output_tokens\"") {
            continue;
        }

        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if obj.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }

        // Filter to today's messages (timestamp field on outer object)
        let ts = obj
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if !ts.starts_with(today) {
            continue;
        }

        let msg = match obj.get("message") {
            Some(m) => m,
            None => continue,
        };

        let usage = match msg.get("usage") {
            Some(u) => u,
            None => continue,
        };

        // Skip streaming partials — only count completed messages with output_tokens
        let output_tokens = match usage.get("output_tokens").and_then(|v| v.as_u64()) {
            Some(t) => t,
            None => continue,
        };

        let input_tokens = usage
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_write = usage
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cache_read = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let model = msg
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("claude-sonnet");

        let p = model_pricing(model);
        total += (input_tokens as f64 * p.input_per_mtok
            + output_tokens as f64 * p.output_per_mtok
            + cache_write as f64 * p.cache_write_per_mtok
            + cache_read as f64 * p.cache_read_per_mtok)
            / 1_000_000.0;
    }

    total
}

struct Pricing {
    input_per_mtok: f64,
    output_per_mtok: f64,
    cache_write_per_mtok: f64,
    cache_read_per_mtok: f64,
}

/// Return per-million-token pricing for a Claude model.
///
/// Matches by model name substring. Defaults to Sonnet pricing for unknown models.
/// Prices are in USD per 1 000 000 tokens.
///
/// Anthropic list prices as of March 2026:
/// | Model family | Input  | Output  | Cache write | Cache read |
/// |--------------|--------|---------|-------------|------------|
/// | Haiku 4.5    | $0.80  | $4.00   | $1.00       | $0.08      |
/// | Sonnet 4.x   | $3.00  | $15.00  | $3.75       | $0.30      |
/// | Opus 4.x     | $15.00 | $75.00  | $18.75      | $1.50      |
fn model_pricing(model: &str) -> Pricing {
    if model.contains("haiku") {
        Pricing {
            input_per_mtok: 0.80,
            output_per_mtok: 4.00,
            cache_write_per_mtok: 1.00,
            cache_read_per_mtok: 0.08,
        }
    } else if model.contains("opus") {
        Pricing {
            input_per_mtok: 15.00,
            output_per_mtok: 75.00,
            cache_write_per_mtok: 18.75,
            cache_read_per_mtok: 1.50,
        }
    } else {
        // sonnet-4-x, unknown → Sonnet pricing
        Pricing {
            input_per_mtok: 3.00,
            output_per_mtok: 15.00,
            cache_write_per_mtok: 3.75,
            cache_read_per_mtok: 0.30,
        }
    }
}

/// Current Unix timestamp in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Convert a Unix timestamp (seconds) to a UTC date string `"YYYY-MM-DD"`.
/// Uses the Howard Hinnant days-to-civil algorithm.
fn epoch_secs_to_date(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Return today's date in UTC as `"YYYY-MM-DD"`.
fn today_utc_date() -> String {
    epoch_secs_to_date(now_secs())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CshipConfig, DailyCostConfig};
    use crate::context::Context;
    use std::io::Write;

    // ── render() ──────────────────────────────────────────────────────────────

    #[test]
    fn test_render_disabled_returns_none() {
        let ctx = Context::default();
        let cfg = CshipConfig {
            daily_cost: Some(DailyCostConfig {
                disabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(render(&ctx, &cfg).is_none());
    }

    // ── epoch_secs_to_date() ──────────────────────────────────────────────────

    #[test]
    fn test_epoch_secs_to_date_unix_epoch() {
        assert_eq!(epoch_secs_to_date(0), "1970-01-01");
    }

    #[test]
    fn test_epoch_secs_to_date_known_date() {
        // 2026-03-18 00:00:00 UTC
        // Days from epoch: 1970→2026 = 20454 days, Jan(31)+Feb(28)+17 = 76 more → 20530
        // 20530 * 86400 = 1_773_792_000
        assert_eq!(epoch_secs_to_date(1_773_792_000), "2026-03-18");
    }

    #[test]
    fn test_epoch_secs_to_date_mid_day() {
        // 2026-03-18 14:30:00 UTC = 1_773_792_000 + 14*3600 + 30*60
        assert_eq!(epoch_secs_to_date(1_773_792_000 + 52_200), "2026-03-18");
    }

    // ── model_pricing() ───────────────────────────────────────────────────────

    #[test]
    fn test_model_pricing_haiku() {
        let p = model_pricing("claude-haiku-4-5");
        assert!((p.input_per_mtok - 0.80).abs() < f64::EPSILON);
        assert!((p.output_per_mtok - 4.00).abs() < f64::EPSILON);
        assert!((p.cache_write_per_mtok - 1.00).abs() < f64::EPSILON);
        assert!((p.cache_read_per_mtok - 0.08).abs() < f64::EPSILON);
    }

    #[test]
    fn test_model_pricing_opus() {
        let p = model_pricing("claude-opus-4-6");
        assert!((p.input_per_mtok - 15.00).abs() < f64::EPSILON);
        assert!((p.output_per_mtok - 75.00).abs() < f64::EPSILON);
        assert!((p.cache_write_per_mtok - 18.75).abs() < f64::EPSILON);
        assert!((p.cache_read_per_mtok - 1.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_model_pricing_sonnet_and_unknown_match() {
        let s = model_pricing("claude-sonnet-4-6");
        let u = model_pricing("claude-unknown-model");
        // Both should return the same Sonnet-tier pricing
        assert!((s.input_per_mtok - u.input_per_mtok).abs() < f64::EPSILON);
        assert!((s.output_per_mtok - u.output_per_mtok).abs() < f64::EPSILON);
        assert!((s.input_per_mtok - 3.00).abs() < f64::EPSILON);
        assert!((s.output_per_mtok - 15.00).abs() < f64::EPSILON);
    }

    // ── cost_from_jsonl() ─────────────────────────────────────────────────────

    /// Build a minimal assistant JSONL line for testing.
    fn make_assistant_line(
        timestamp: &str,
        model: &str,
        input: u64,
        output: u64,
        cache_write: u64,
        cache_read: u64,
    ) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": timestamp,
            "message": {
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_creation_input_tokens": cache_write,
                    "cache_read_input_tokens": cache_read
                }
            }
        })
        .to_string()
    }

    /// Build a streaming partial (no output_tokens) — must be skipped.
    fn make_partial_line(timestamp: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": timestamp,
            "message": {
                "model": "claude-sonnet-4-6",
                "usage": {
                    "input_tokens": 100,
                    "cache_creation_input_tokens": 500,
                    "cache_read_input_tokens": 1000
                    // no output_tokens
                }
            }
        })
        .to_string()
    }

    #[test]
    fn test_cost_from_jsonl_empty_file_returns_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.jsonl");
        std::fs::File::create(&path).unwrap();
        assert!((cost_from_jsonl(&path, "2026-03-18")).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cost_from_jsonl_skips_partial_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{}", make_partial_line("2026-03-18T10:00:00Z")).unwrap();
        // Partials have no output_tokens — cost must be zero
        assert!((cost_from_jsonl(&path, "2026-03-18")).abs() < f64::EPSILON);
    }

    #[test]
    fn test_cost_from_jsonl_filters_to_today() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Yesterday's message — must be excluded
        writeln!(
            f,
            "{}",
            make_assistant_line(
                "2026-03-17T23:59:59Z",
                "claude-sonnet-4-6",
                1_000_000,
                1_000_000,
                0,
                0
            )
        )
        .unwrap();
        // Today's message — must be included
        writeln!(
            f,
            "{}",
            make_assistant_line("2026-03-18T00:00:00Z", "claude-sonnet-4-6", 0, 0, 0, 0)
        )
        .unwrap();

        let cost = cost_from_jsonl(&path, "2026-03-18");
        // Only today's zero-token message — cost should be zero
        assert!(cost.abs() < 1e-6, "expected ~0, got {cost}");
    }

    #[test]
    fn test_cost_from_jsonl_sonnet_pricing_matches_ccusage() {
        // Verify against the values ccusage computed for today's cship session:
        // 262 input, 9093 output, 586505 cache_write, 3522600 cache_read → $3.39
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            make_assistant_line(
                "2026-03-18T06:05:00Z",
                "claude-sonnet-4-6",
                262,
                9_093,
                586_505,
                3_522_600
            )
        )
        .unwrap();

        let cost = cost_from_jsonl(&path, "2026-03-18");
        // Expected: (262*3 + 9093*15 + 586505*3.75 + 3522600*0.30) / 1_000_000
        let expected = (262.0 * 3.0
            + 9_093.0 * 15.0
            + 586_505.0 * 3.75
            + 3_522_600.0 * 0.30)
            / 1_000_000.0;
        assert!(
            (cost - expected).abs() < 0.001,
            "expected ~{expected:.3}, got {cost:.3}"
        );
    }

    #[test]
    fn test_cost_from_jsonl_haiku_pricing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("haiku.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "{}",
            make_assistant_line(
                "2026-03-18T08:00:00Z",
                "claude-haiku-4-5-20251001",
                1_000_000,
                1_000_000,
                1_000_000,
                1_000_000
            )
        )
        .unwrap();

        let cost = cost_from_jsonl(&path, "2026-03-18");
        let expected = 0.80 + 4.00 + 1.00 + 0.08; // per MTok → 1M tokens each
        assert!(
            (cost - expected).abs() < 0.001,
            "expected {expected}, got {cost}"
        );
    }

    #[test]
    fn test_cost_from_jsonl_sums_multiple_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.jsonl");
        let mut f = std::fs::File::create(&path).unwrap();
        // Two 1M-token sonnet messages = 2× the cost
        for _ in 0..2 {
            writeln!(
                f,
                "{}",
                make_assistant_line(
                    "2026-03-18T09:00:00Z",
                    "claude-sonnet-4-6",
                    1_000_000,
                    1_000_000,
                    0,
                    0
                )
            )
            .unwrap();
        }

        let cost = cost_from_jsonl(&path, "2026-03-18");
        let per_message = (3.00 + 15.00) / 1.0; // $18.00 per 1M input + 1M output
        assert!(
            (cost - 2.0 * per_message).abs() < 0.001,
            "expected {}, got {cost}",
            2.0 * per_message
        );
    }
}
