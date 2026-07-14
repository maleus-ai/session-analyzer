//! Model pricing tables (USD per million tokens) and cost computation.

use crate::model::Usage;

/// Per-million-token prices for a model.
#[derive(Debug, Clone, Copy)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Cache-write at the **5-minute** TTL (1.25× input).
    pub cache_write: f64,
    /// Cache-write at the **1-hour** TTL (2.0× input).
    pub cache_write_1h: f64,
    pub cache_read: f64,
}

impl Price {
    /// Dollar cost of a single usage record under this price sheet.
    ///
    /// Cache-write is TTL-aware: `ephemeral_1h` tokens are charged at the 1-hour rate
    /// (2.0× input) and `ephemeral_5m` at the 5-minute rate (1.25× input). Any cache-write
    /// tokens not broken down by TTL (older logs that only report the total) fall back to
    /// the 5-minute rate, preserving prior behavior.
    pub fn cost(&self, u: &Usage) -> f64 {
        let cw_1h = u.ephemeral_1h.min(u.cache_creation_input_tokens);
        let cw_5m = u.ephemeral_5m;
        let cw_rest = u.cache_creation_input_tokens.saturating_sub(cw_1h + cw_5m);
        let cache_write_cost = cw_1h as f64 * self.cache_write_1h
            + (cw_5m + cw_rest) as f64 * self.cache_write;
        (u.input_tokens as f64 * self.input
            + cache_write_cost
            + u.cache_read_input_tokens as f64 * self.cache_read
            + u.output_tokens as f64 * self.output)
            / 1_000_000.0
    }
}

/// Resolve public list pricing from a model identifier substring.
///
/// Falls back to Opus-class pricing for unknown models so cost is never
/// silently understated.
pub fn price_for(model: &str) -> Price {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") {
        Price { input: 0.80, output: 4.00, cache_write: 1.00, cache_write_1h: 1.60, cache_read: 0.08 }
    } else if m.contains("sonnet") {
        Price { input: 3.00, output: 15.00, cache_write: 3.75, cache_write_1h: 6.00, cache_read: 0.30 }
    } else if m.contains("opus") {
        Price { input: 15.00, output: 75.00, cache_write: 18.75, cache_write_1h: 30.00, cache_read: 1.50 }
    } else {
        // Unknown / <synthetic> — assume Opus so we don't understate.
        Price { input: 15.00, output: 75.00, cache_write: 18.75, cache_write_1h: 30.00, cache_read: 1.50 }
    }
}
