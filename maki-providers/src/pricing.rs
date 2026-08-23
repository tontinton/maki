//! What a turn, and a whole session, cost.
//!
//! Rates live on the model. Turning them into a bill happens here: the
//! wall-clock schedules that scale them, and how a stored session's total comes
//! back. The rule everything else leans on is that a turn is priced once, when
//! it runs, and that number is what gets stored, summed and shown. History is
//! never re-priced, because rates move.

use std::collections::HashMap;
use std::fmt;

use jiff::Timestamp;
use maki_storage::sessions::StoredTokenUsage;

use crate::model::{Model, TokenUsage};

const HOURS_PER_DAY: u8 = 24;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = HOURS_PER_DAY as i64 * SECONDS_PER_HOUR;
/// Multiplier of a provider that bills the same rate around the clock.
pub(crate) const FLAT_RATE: f64 = 1.0;

/// Rates that move with the wall clock. DeepSeek doubles everything during its
/// peak UTC hours, so model tables quote the off-peak rates and the provider's
/// schedule scales the bill inside its windows. The next price change is then a
/// constant edit instead of new code.
///
/// One multiplier is enough while providers move all four rates together.
#[derive(Debug)]
pub struct PricingSchedule {
    windows: &'static [PricingWindow],
    multiplier: f64,
}

/// Half-open `[start, end)` in whole UTC hours. `start > end` wraps past
/// midnight, so a window never needs splitting in two.
#[derive(Debug)]
pub struct PricingWindow {
    start: u8,
    end: u8,
}

impl PricingWindow {
    /// `hours(22, 2)` wraps past midnight. Every window is a `const`, so a typo
    /// here is a build error rather than a mispriced turn.
    pub const fn hours(start: u8, end: u8) -> Self {
        assert!(start < HOURS_PER_DAY, "window starts after the day ends");
        assert!(end <= HOURS_PER_DAY, "window ends after the day ends");
        assert!(start != end, "window covers no time at all");
        Self { start, end }
    }

    fn contains(&self, hour: u8) -> bool {
        if self.start < self.end {
            hour >= self.start && hour < self.end
        } else {
            hour >= self.start || hour < self.end
        }
    }
}

/// `01:00-04:00`, the shape provider docs quote peak hours in.
impl fmt::Display for PricingWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:00-{:02}:00", self.start, self.end)
    }
}

impl PricingSchedule {
    pub const fn new(windows: &'static [PricingWindow], multiplier: f64) -> Self {
        assert!(
            !windows.is_empty(),
            "a schedule with no windows never applies; drop it instead"
        );
        assert!(
            multiplier > FLAT_RATE,
            "model tables quote the off-peak rates, so a schedule only ever adds a surcharge"
        );
        Self {
            windows,
            multiplier,
        }
    }

    pub(crate) fn multiplier_at(&self, at: Timestamp) -> f64 {
        let hour = (at.as_second().rem_euclid(SECONDS_PER_DAY) / SECONDS_PER_HOUR) as u8;
        if self.windows.iter().any(|w| w.contains(hour)) {
            self.multiplier
        } else {
            FLAT_RATE
        }
    }
}

/// `2x during 01:00-04:00, 06:00-10:00 UTC`. Docs and hints render this rather
/// than restate the hours in prose that drifts.
impl fmt::Display for PricingSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x during ", self.multiplier)?;
        for (i, window) in self.windows.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{window}")?;
        }
        f.write_str(" UTC")
    }
}

/// The bill a stored session ran up. Status bar, `/usage`, ACP and headless all
/// come here, so they cannot disagree about the same session.
///
/// Turns record what they paid, so summing those is the truth. What was written
/// before that kept counters only, and its estimate against today's table is
/// settled into the entry here, once: after a later turn merges its own cost in,
/// the counters no longer say which of them was already paid for, so estimating
/// again would drop everything the entry had before. Counters with no breakdown
/// get one seeded, so there is always somewhere to settle into.
///
/// `None` when nothing here is priced (oauth, local models), so callers show no
/// cost instead of a made up "$0.000".
pub fn settle_session(
    total: &TokenUsage,
    by_model: &mut HashMap<String, StoredTokenUsage>,
    current: &Model,
    fast: bool,
) -> Option<f64> {
    if by_model.is_empty() && *total != TokenUsage::default() {
        by_model.insert(current.id.clone(), total.billed(None));
    }
    for (id, usage) in by_model.iter_mut() {
        usage.cost = model_cost(id, usage, current, fast);
    }
    by_model
        .values()
        .filter_map(|usage| usage.cost)
        .reduce(|total, cost| total + cost)
}

/// One model's slice of [`settle_session`], as `/usage` breaks it down per row.
/// Usage is keyed by bare model id, so resolving it needs the session's
/// provider put back in front.
pub fn model_cost(id: &str, usage: &StoredTokenUsage, current: &Model, fast: bool) -> Option<f64> {
    if let Some(cost) = usage.cost {
        return Some(cost);
    }
    if id == current.id {
        return current.list_cost(&(*usage).into(), fast);
    }
    Model::from_spec(id)
        .or_else(|_| Model::from_spec(&format!("{}/{}", current.provider, id)))
        .ok()?
        .list_cost(&(*usage).into(), fast)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::ManifestRegistry;
    use crate::model::{FastPricing, ModelFamily, ModelPricing, ModelTier};
    use std::sync::Arc;
    use test_case::test_case;

    const SECONDS_PER_MINUTE: i64 = 60;
    const PEAK: f64 = 2.0;
    const DAYS_SINCE_EPOCH: i64 = 20_000;
    const PEAK_WINDOWS: &[PricingWindow] =
        &[PricingWindow::hours(1, 4), PricingWindow::hours(6, 10)];
    const WRAPPING_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, 2)];
    const UNTIL_MIDNIGHT_WINDOW: &[PricingWindow] = &[PricingWindow::hours(22, HOURS_PER_DAY)];
    const WHOLE_DAY_WINDOW: &[PricingWindow] = &[PricingWindow::hours(0, HOURS_PER_DAY)];
    const LAST_MINUTE: i64 = 59;
    const LAST_SECOND: i64 = 59;

    const CURRENT: &str = "current";
    const INPUT_RATE: f64 = 3.0;
    const UNPRICED: f64 = 0.0;
    const ONE_MILLION: u32 = 1_000_000;
    /// [`ONE_MILLION`] input tokens at [`INPUT_RATE`].
    const LIST_PRICE: f64 = 3.0;
    const RECORDED: f64 = 0.5;
    const UNRESOLVABLE: &str = "a-model-no-table-has-ever-heard-of";
    const ALSO_UNRESOLVABLE: &str = "another-model-no-table-has-ever-heard-of";
    const FAST_INPUT_RATE: f64 = 12.0;
    /// [`ONE_MILLION`] input tokens at [`FAST_INPUT_RATE`].
    const FAST_LIST_PRICE: f64 = 12.0;
    /// A real spec, so the bare-id fallback runs against the real tables.
    const SCHEDULED_SPEC: &str = "deepseek/deepseek-v4-pro";

    fn utc(day: i64, hour: i64, minute: i64, second: i64) -> Timestamp {
        Timestamp::from_second(
            day * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second,
        )
        .expect("timestamp in range")
    }

    fn model(id: &str, input_rate: f64) -> Model {
        Model {
            id: id.into(),
            provider: Arc::from("anthropic"),
            tier: ModelTier::Medium,
            family: ModelFamily::Claude,
            supports_tool_examples_override: None,
            thinking_override: None,
            supports_vision_override: None,
            pricing: ModelPricing {
                input: input_rate,
                ..ModelPricing::ZERO
            },
            discovered_free: false,
            max_output_tokens: None,
            context_window: 0,
            thinking_fields: None,
        }
    }

    fn stored(cost: Option<f64>) -> StoredTokenUsage {
        StoredTokenUsage {
            input: ONE_MILLION,
            cost,
            ..Default::default()
        }
    }

    fn breakdown(entries: &[(&str, Option<f64>)]) -> HashMap<String, StoredTokenUsage> {
        entries
            .iter()
            .map(|(id, cost)| ((*id).to_owned(), stored(*cost)))
            .collect()
    }

    /// Windows are half-open down to the second, since an off-by-one bills a
    /// whole hour at the wrong rate. Every case also runs on a day before the
    /// epoch, where the timestamp is negative and must not wrap into another
    /// window.
    #[test_case(PEAK_WINDOWS, 0, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "last_second_before_the_start")]
    #[test_case(PEAK_WINDOWS, 1, 0, 0, PEAK                          ; "first_second_of_a_window")]
    #[test_case(PEAK_WINDOWS, 3, LAST_MINUTE, LAST_SECOND, PEAK      ; "last_second_inside")]
    #[test_case(PEAK_WINDOWS, 4, 0, 0, FLAT_RATE                     ; "first_second_after_the_end")]
    #[test_case(PEAK_WINDOWS, 7, 15, 0, PEAK                         ; "second_window")]
    #[test_case(PEAK_WINDOWS, 23, 0, 0, FLAT_RATE                    ; "after_the_last_window")]
    #[test_case(WRAPPING_WINDOW, 23, 0, 0, PEAK                      ; "wrapping_before_midnight")]
    #[test_case(WRAPPING_WINDOW, 0, 0, 0, PEAK                       ; "wrapping_across_midnight")]
    #[test_case(WRAPPING_WINDOW, 21, LAST_MINUTE, LAST_SECOND, FLAT_RATE ; "wrapping_start_is_half_open")]
    #[test_case(WRAPPING_WINDOW, 2, 0, 0, FLAT_RATE                  ; "wrapping_end_is_half_open")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "ends_at_midnight")]
    #[test_case(UNTIL_MIDNIGHT_WINDOW, 0, 0, 0, FLAT_RATE            ; "ending_at_midnight_does_not_wrap")]
    #[test_case(WHOLE_DAY_WINDOW, 0, 0, 0, PEAK                      ; "whole_day_starts_at_midnight")]
    #[test_case(WHOLE_DAY_WINDOW, 23, LAST_MINUTE, LAST_SECOND, PEAK ; "whole_day_never_leaves_peak")]
    fn windows_price_by_the_utc_clock(
        windows: &'static [PricingWindow],
        hour: i64,
        minute: i64,
        second: i64,
        expected: f64,
    ) {
        let schedule = PricingSchedule::new(windows, PEAK);
        for day in [DAYS_SINCE_EPOCH, -DAYS_SINCE_EPOCH] {
            assert_eq!(
                schedule.multiplier_at(utc(day, hour, minute, second)),
                expected,
                "day {day}"
            );
        }
    }

    #[test]
    fn schedules_render_the_hours_they_bill() {
        assert_eq!(
            PricingSchedule::new(PEAK_WINDOWS, PEAK).to_string(),
            "2x during 01:00-04:00, 06:00-10:00 UTC"
        );
        assert_eq!(
            PricingSchedule::new(WRAPPING_WINDOW, PEAK).to_string(),
            "2x during 22:00-02:00 UTC"
        );
    }

    /// Each entry bills [`ONE_MILLION`] input tokens, so a row worth
    /// [`LIST_PRICE`] came from the price table and [`RECORDED`] came from the
    /// turn itself. A breakdown nothing can price is not a free session:
    /// `Some(0.0)` would put a confident "$0.000" on screen.
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (ALSO_UNRESOLVABLE, Some(RECORDED))], INPUT_RATE, Some(2.0 * RECORDED) ; "recorded_costs_win_over_the_price_table")]
    #[test_case(&[(CURRENT, None), (UNRESOLVABLE, None)], INPUT_RATE, Some(LIST_PRICE)                                   ; "legacy_counters_use_each_models_own_rates")]
    #[test_case(&[(UNRESOLVABLE, Some(RECORDED)), (CURRENT, None), (ALSO_UNRESOLVABLE, None)], INPUT_RATE, Some(RECORDED + LIST_PRICE) ; "mixed_eras_sum_recorded_and_listed")]
    #[test_case(&[(UNRESOLVABLE, None)], INPUT_RATE, None                                                               ; "a_breakdown_that_prices_to_nothing")]
    #[test_case(&[(CURRENT, None)], UNPRICED, None                                                                      ; "unpriced_model_with_a_breakdown")]
    #[test_case(&[], INPUT_RATE, Some(LIST_PRICE)                                                                       ; "no_breakdown_prices_the_total")]
    #[test_case(&[], UNPRICED, None                                                                                     ; "no_breakdown_on_an_unpriced_model")]
    fn session_cost_bills_every_breakdown(
        entries: &[(&str, Option<f64>)],
        input_rate: f64,
        expected: Option<f64>,
    ) {
        let current = model(CURRENT, input_rate);
        let mut by_model = breakdown(entries);
        let counted = entries.len().max(1) as u32 * ONE_MILLION;

        // With a breakdown the stored running total says nothing about the
        // bill, so the two drifting apart (compaction, a resume) must not
        // change the answer.
        let totals = if entries.is_empty() {
            vec![counted]
        } else {
            vec![counted, 0, 999 * ONE_MILLION]
        };
        for input in totals {
            let total = TokenUsage {
                input,
                ..Default::default()
            };
            assert_eq!(
                settle_session(&total, &mut by_model, &current, false),
                expected,
                "total of {input} input tokens"
            );
        }
    }

    /// An entry written before turns recorded their cost used to keep only the
    /// cost of the next turn that touched it, so every later load reported the
    /// session as costing whatever it last did.
    #[test_case(&[]                ; "counters_with_no_breakdown")]
    #[test_case(&[(CURRENT, None)] ; "a_breakdown_written_before_costs")]
    fn a_settled_estimate_survives_the_next_turn(entries: &[(&str, Option<f64>)]) {
        let current = model(CURRENT, INPUT_RATE);
        let mut by_model = breakdown(entries);
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        assert_eq!(
            settle_session(&total, &mut by_model, &current, false),
            Some(LIST_PRICE)
        );

        *by_model.entry(CURRENT.to_owned()).or_default() += stored(Some(RECORDED));

        assert_eq!(
            settle_session(&total, &mut by_model, &current, false),
            Some(LIST_PRICE + RECORDED)
        );
    }

    /// The sibling is priced differently on purpose, so passing here cannot be
    /// `current`'s rates leaking through.
    #[test]
    fn bare_ids_resolve_against_the_current_provider() {
        let current = Model::from_spec(SCHEDULED_SPEC).unwrap();
        let sibling_id = ManifestRegistry::for_slug(&current.provider)
            .expect("a builtin provider")
            .models
            .iter()
            .find_map(|e| (e.pricing.input != current.pricing.input).then_some(e.prefixes[0]))
            .expect("a sibling model the table prices differently");
        let sibling = Model::from_spec(&format!("{}/{sibling_id}", current.provider)).unwrap();
        let usage = stored(None);

        let cost = model_cost(sibling_id, &usage, &current, false);
        assert_eq!(cost, sibling.list_cost(&usage.into(), false));
        assert_ne!(cost, current.list_cost(&usage.into(), false));
    }

    /// What was paid is what was paid, even for an id the table prices
    /// differently, and even for a model that prices to nothing today.
    #[test_case(INPUT_RATE ; "priced_model")]
    #[test_case(UNPRICED   ; "unpriced_model")]
    fn a_recorded_cost_short_circuits_the_price_table(input_rate: f64) {
        let current = model(CURRENT, input_rate);
        assert_eq!(
            model_cost(CURRENT, &stored(Some(RECORDED)), &current, false),
            Some(RECORDED)
        );
    }

    #[test_case(true, &[]                => Some(FAST_LIST_PRICE) ; "fast_seeded_from_the_counters")]
    #[test_case(true, &[(CURRENT, None)] => Some(FAST_LIST_PRICE) ; "fast_from_a_breakdown")]
    #[test_case(false, &[(CURRENT, None)] => Some(LIST_PRICE)     ; "standard_from_a_breakdown")]
    fn fast_rates_reach_the_session_total(
        fast: bool,
        entries: &[(&str, Option<f64>)],
    ) -> Option<f64> {
        let mut current = model(CURRENT, INPUT_RATE);
        current.pricing.fast = Some(FastPricing {
            input: FAST_INPUT_RATE,
            output: 0.0,
        });
        let total = TokenUsage {
            input: ONE_MILLION,
            ..Default::default()
        };

        settle_session(&total, &mut breakdown(entries), &current, fast)
    }
}
