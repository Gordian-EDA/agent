//! Per-model token pricing, for the status-line cost HUD.
//!
//! Pure data + arithmetic — no rendering, no `App` — so the ledger math stays
//! unit-testable in isolation. [`MODEL_PRICES`] is keyed by the canonical model
//! family ([`canonical_model`] folds the Bedrock / gateway / native id shapes
//! Gordian uses onto one key), and [`Ledger`] accumulates a session's usage and
//! computes its dollar cost — cache reads billed at the cheaper cached-input
//! rate, which is where prompt caching shows its savings.

/// USD per 1,000,000 tokens for one model: full-price input, output, and the
/// (cheaper) rate billed on tokens served from the prompt cache.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    /// Cache-read rate — ~0.1× the input rate on the Anthropic API.
    pub cached_input: f64,
}

/// Pricing for every Claude family Gordian routes to (Anthropic published rates,
/// USD / 1M tokens; cache-read at ~0.1× input). Keys are canonical families
/// produced by [`canonical_model`]. An unknown id has no entry → the HUD renders
/// "—" rather than a wrong number.
pub const MODEL_PRICES: &[(&str, Price)] = &[
    ("claude-opus-4-8", Price { input: 5.0, output: 25.0, cached_input: 0.5 }),
    ("claude-opus-4-7", Price { input: 5.0, output: 25.0, cached_input: 0.5 }),
    ("claude-opus-4-6", Price { input: 5.0, output: 25.0, cached_input: 0.5 }),
    ("claude-opus-4-5", Price { input: 5.0, output: 25.0, cached_input: 0.5 }),
    ("claude-sonnet-4-6", Price { input: 3.0, output: 15.0, cached_input: 0.3 }),
    ("claude-sonnet-4-5", Price { input: 3.0, output: 15.0, cached_input: 0.3 }),
    ("claude-haiku-4-5", Price { input: 1.0, output: 5.0, cached_input: 0.1 }),
    ("claude-fable-5", Price { input: 10.0, output: 50.0, cached_input: 1.0 }),
];

/// Fold a provider model id onto its canonical family key. Handles the three id
/// shapes Gordian uses: Bedrock (`us.anthropic.claude-opus-4-5-20251101-v1:0`),
/// the gateway's `provider/model` (`anthropic/claude-sonnet-4-6`), and the
/// native Anthropic id (`claude-opus-4-8`). Returns the longest `MODEL_PRICES`
/// key that the id contains, so a dated/versioned suffix still matches.
pub fn canonical_model(model: &str) -> Option<&'static str> {
    MODEL_PRICES
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| model.contains(k))
        .max_by_key(|k| k.len())
}

/// Look up the price for a (possibly decorated) model id, `None` if unknown.
pub fn price_of(model: &str) -> Option<Price> {
    let key = canonical_model(model)?;
    MODEL_PRICES.iter().find(|(k, _)| *k == key).map(|(_, p)| *p)
}

/// Cumulative token usage across a session, for the cost + cached HUD. Token
/// fields mirror the provider's accounting: `input` is the full-price prompt
/// remainder (cache reads/writes are billed separately, not double-counted), so
/// the three input-side fields sum to the prompt size of every call.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ledger {
    /// Full-price prompt tokens (excludes cache reads/writes).
    pub input: u64,
    pub output: u64,
    /// Tokens written to the cache (billed at ~1.25× input — the write premium).
    pub cache_write: u64,
    /// Tokens served from the cache (billed at the cheaper `cached_input` rate).
    pub cache_read: u64,
}

impl Ledger {
    /// Fold one `AgentEvent::Usage` into the running totals. `input_tokens` from
    /// the event *includes* the cache counts, so the full-price remainder is
    /// `input_tokens − cache_read − cache_write`.
    pub fn record(&mut self, input_tokens: u64, output_tokens: u64, cache_write: u64, cache_read: u64) {
        self.input += input_tokens.saturating_sub(cache_read).saturating_sub(cache_write);
        self.output += output_tokens;
        self.cache_write += cache_write;
        self.cache_read += cache_read;
    }

    /// Total prompt + completion tokens billed this session.
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_write + self.cache_read
    }

    /// Session cost in USD for `model`, or `None` when the model isn't priced.
    /// Cache reads bill at `cached_input` and cache writes at 1.25× input (the
    /// 5-minute-TTL write premium) — so the savings from caching show directly.
    pub fn cost(&self, model: &str) -> Option<f64> {
        let p = price_of(model)?;
        let per_m = |tokens: u64, rate: f64| tokens as f64 / 1_000_000.0 * rate;
        Some(
            per_m(self.input, p.input)
                + per_m(self.output, p.output)
                + per_m(self.cache_read, p.cached_input)
                + per_m(self.cache_write, p.input * 1.25),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_folds_every_id_shape() {
        assert_eq!(
            canonical_model("us.anthropic.claude-opus-4-5-20251101-v1:0"),
            Some("claude-opus-4-5"),
        );
        assert_eq!(canonical_model("anthropic/claude-sonnet-4-6"), Some("claude-sonnet-4-6"));
        assert_eq!(canonical_model("claude-opus-4-8"), Some("claude-opus-4-8"));
        assert_eq!(canonical_model("gpt-4o"), None);
    }

    #[test]
    fn ledger_accumulates_and_splits_out_the_cached_prefix() {
        let mut l = Ledger::default();
        // Turn 1: cold prompt, writes 800 to cache; 1000 input (incl. the 800 write).
        l.record(1000, 100, 800, 0);
        // Turn 2: warm — reads back the 800, only 250 fresh input.
        l.record(1050, 120, 0, 800);
        // Turn 3: more cache reads.
        l.record(1200, 80, 0, 1000);

        assert_eq!(l.input, 200 + 250 + 200, "full-price input is the non-cached remainder");
        assert_eq!(l.output, 300);
        assert_eq!(l.cache_write, 800);
        assert_eq!(l.cache_read, 1800);
        assert_eq!(l.total_tokens(), 650 + 300 + 800 + 1800);
    }

    #[test]
    fn cost_bills_cache_reads_cheaper_than_full_input() {
        // Opus 4.8: $5/$25 input/output, $0.50 cached-input, $6.25 cache-write.
        let mut l = Ledger::default();
        l.record(1_000_000, 1_000_000, 0, 0); // 1M full input + 1M output
        let full = l.cost("claude-opus-4-8").unwrap();
        assert!((full - 30.0).abs() < 1e-9, "5 + 25 = $30, got {full}");

        // Same 1M of prompt, but served from cache: billed at $0.50, not $5.
        let mut cached = Ledger::default();
        cached.record(1_000_000, 1_000_000, 0, 1_000_000);
        let with_cache = cached.cost("claude-opus-4-8").unwrap();
        assert!((with_cache - 25.5).abs() < 1e-9, "0.5 + 25 = $25.50, got {with_cache}");
        assert!(with_cache < full, "cache reads must be cheaper — that's the savings");

        // A cache write costs 1.25× input: 1M write → $6.25 on top of output.
        let mut wrote = Ledger::default();
        wrote.record(1_000_000, 0, 1_000_000, 0);
        assert!((wrote.cost("claude-opus-4-8").unwrap() - 6.25).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_has_no_cost() {
        let mut l = Ledger::default();
        l.record(1000, 100, 0, 0);
        assert_eq!(l.cost("gpt-4o"), None);
    }
}
