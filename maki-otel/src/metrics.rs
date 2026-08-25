//! Counter registry. Every maki metric is a monotonic sum, so aggregation is
//! addition into a map keyed by metric name plus attribute set.

use std::collections::{BTreeMap, HashMap};

use crate::attr::AttrSet;
use crate::settings::Temporality;

pub const UNIT_NONE: &str = "";
pub const UNIT_TOKENS: &str = "tokens";
pub const UNIT_USD: &str = "USD";
pub const UNIT_SECONDS: &str = "s";

pub struct MetricDef {
    pub name: &'static str,
    pub unit: &'static str,
    pub description: &'static str,
}

pub static SESSION_COUNT: MetricDef = MetricDef {
    name: "maki.session.count",
    unit: UNIT_NONE,
    description: "Sessions started",
};
pub static TOKEN_USAGE: MetricDef = MetricDef {
    name: "maki.token.usage",
    unit: UNIT_TOKENS,
    description: "Tokens billed, split by kind",
};
pub static COST_USAGE: MetricDef = MetricDef {
    name: "maki.cost.usage",
    unit: UNIT_USD,
    description: "Estimated cost of the session in USD",
};
pub static LINES_OF_CODE: MetricDef = MetricDef {
    name: "maki.lines_of_code.count",
    unit: UNIT_NONE,
    description: "Lines added and removed by edits",
};
pub static TOOL_DECISION: MetricDef = MetricDef {
    name: "maki.tool.decision",
    unit: UNIT_NONE,
    description: "Permission decisions, accepted or rejected",
};
pub static COMMIT_COUNT: MetricDef = MetricDef {
    name: "maki.commit.count",
    unit: UNIT_NONE,
    description: "Git commits created by the agent",
};
pub static PULL_REQUEST_COUNT: MetricDef = MetricDef {
    name: "maki.pull_request.count",
    unit: UNIT_NONE,
    description: "Pull requests created by the agent",
};
pub static ACTIVE_TIME: MetricDef = MetricDef {
    name: "maki.active_time.total",
    unit: UNIT_SECONDS,
    description: "Time the agent spent working",
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    Int(i64),
    Double(f64),
}

impl Value {
    fn add(&mut self, other: Value) {
        debug_assert!(
            std::mem::discriminant(self) == std::mem::discriminant(&other),
            "a metric must keep one value type"
        );
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => *a += b,
            (Self::Double(a), Self::Double(b)) => *a += b,
            (a, b) => *a = b,
        }
    }
}

pub struct Measurement {
    pub def: &'static MetricDef,
    pub value: Value,
    pub attrs: AttrSet,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataPoint {
    pub attrs: AttrSet,
    pub start_time_unix_nano: u64,
    pub time_unix_nano: u64,
    pub value: Value,
}

pub struct MetricData {
    pub def: &'static MetricDef,
    pub points: Vec<DataPoint>,
}

struct Streams {
    def: &'static MetricDef,
    points: HashMap<AttrSet, Value>,
}

pub struct Registry {
    temporality: Temporality,
    /// Start of the current delta window, or of the process for cumulative.
    window_start_nanos: u64,
    /// Sorted by name so exports come out in a stable order.
    metrics: BTreeMap<&'static str, Streams>,
}

impl Registry {
    pub fn new(temporality: Temporality, start_nanos: u64) -> Self {
        Self {
            temporality,
            window_start_nanos: start_nanos,
            metrics: BTreeMap::new(),
        }
    }

    pub fn record(&mut self, measurement: Measurement) {
        let Measurement { def, value, attrs } = measurement;
        self.metrics
            .entry(def.name)
            .or_insert_with(|| Streams {
                def,
                points: HashMap::new(),
            })
            .points
            .entry(attrs)
            .and_modify(|total| total.add(value))
            .or_insert(value);
    }

    pub fn temporality(&self) -> Temporality {
        self.temporality
    }

    pub fn is_empty(&self) -> bool {
        self.metrics.values().all(|s| s.points.is_empty())
    }

    /// Nothing will ever collect these, so they must not accumulate.
    pub fn clear(&mut self) {
        self.metrics.values_mut().for_each(|s| s.points.clear());
    }

    /// Delta empties the accumulator and starts a new window; cumulative keeps
    /// totals and always reports from the process start.
    pub fn collect(&mut self, now_nanos: u64) -> Vec<MetricData> {
        let (start, temporality) = (self.window_start_nanos, self.temporality);
        let collected = self
            .metrics
            .values_mut()
            .filter(|streams| !streams.points.is_empty())
            .map(|streams| {
                let points = match temporality {
                    Temporality::Delta => streams.points.drain().collect::<Vec<_>>(),
                    Temporality::Cumulative => streams
                        .points
                        .iter()
                        .map(|(attrs, value)| (attrs.clone(), *value))
                        .collect(),
                };
                MetricData {
                    def: streams.def,
                    points: points
                        .into_iter()
                        .map(|(attrs, value)| DataPoint {
                            attrs,
                            start_time_unix_nano: start,
                            time_unix_nano: now_nanos,
                            value,
                        })
                        .collect(),
                }
            })
            .collect();

        if temporality == Temporality::Delta {
            self.window_start_nanos = now_nanos;
        }
        collected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTR_TYPE: &str = "type";
    const TYPE_INPUT: &str = "input";
    const TYPE_OUTPUT: &str = "output";
    const T0: u64 = 1_000;
    const T1: u64 = 2_000;
    const T2: u64 = 3_000;

    fn measurement(def: &'static MetricDef, value: Value, attrs: AttrSet) -> Measurement {
        Measurement { def, value, attrs }
    }

    fn tokens(kind: &str, n: i64) -> Measurement {
        measurement(
            &TOKEN_USAGE,
            Value::Int(n),
            AttrSet::new().with(ATTR_TYPE, kind),
        )
    }

    fn only_point(data: &[MetricData]) -> &DataPoint {
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].points.len(), 1);
        &data[0].points[0]
    }

    #[test]
    fn measurements_with_equal_attributes_share_a_stream() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(tokens(TYPE_INPUT, 3));
        registry.record(tokens(TYPE_INPUT, 4));
        let collected = registry.collect(T1);
        assert_eq!(only_point(&collected).value, Value::Int(7));
    }

    #[test]
    fn attribute_order_does_not_split_a_stream() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(measurement(
            &TOKEN_USAGE,
            Value::Int(1),
            AttrSet::new().with("a", "1").with("b", "2"),
        ));
        registry.record(measurement(
            &TOKEN_USAGE,
            Value::Int(1),
            AttrSet::new().with("b", "2").with("a", "1"),
        ));
        assert_eq!(only_point(&registry.collect(T1)).value, Value::Int(2));
    }

    #[test]
    fn distinct_attributes_make_distinct_points() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(tokens(TYPE_INPUT, 1));
        registry.record(tokens(TYPE_OUTPUT, 2));
        let collected = registry.collect(T1);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].points.len(), 2);
    }

    #[test]
    fn delta_resets_after_every_collection() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(tokens(TYPE_INPUT, 5));
        let first = registry.collect(T1);
        assert_eq!(only_point(&first).value, Value::Int(5));
        assert_eq!(only_point(&first).start_time_unix_nano, T0);

        registry.record(tokens(TYPE_INPUT, 2));
        let second = registry.collect(T2);
        assert_eq!(only_point(&second).value, Value::Int(2));
        assert_eq!(only_point(&second).start_time_unix_nano, T1);
    }

    #[test]
    fn cumulative_keeps_totals_and_the_process_start() {
        let mut registry = Registry::new(Temporality::Cumulative, T0);
        registry.record(tokens(TYPE_INPUT, 5));
        assert_eq!(only_point(&registry.collect(T1)).value, Value::Int(5));

        registry.record(tokens(TYPE_INPUT, 2));
        let second = registry.collect(T2);
        assert_eq!(only_point(&second).value, Value::Int(7));
        assert_eq!(only_point(&second).start_time_unix_nano, T0);
    }

    #[test]
    fn doubles_accumulate_separately_from_ints() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(measurement(&COST_USAGE, Value::Double(0.5), AttrSet::new()));
        registry.record(measurement(
            &COST_USAGE,
            Value::Double(0.25),
            AttrSet::new(),
        ));
        assert_eq!(only_point(&registry.collect(T1)).value, Value::Double(0.75));
    }

    #[test]
    fn collection_output_is_ordered_by_metric_name() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        registry.record(tokens(TYPE_INPUT, 1));
        registry.record(measurement(&COMMIT_COUNT, Value::Int(1), AttrSet::new()));
        let names: Vec<&str> = registry.collect(T1).iter().map(|m| m.def.name).collect();
        assert_eq!(names, vec![COMMIT_COUNT.name, TOKEN_USAGE.name]);
    }

    #[test]
    fn emptiness_follows_recording_and_draining() {
        let mut registry = Registry::new(Temporality::Delta, T0);
        assert!(registry.is_empty());
        assert!(registry.collect(T1).is_empty());
        registry.record(tokens(TYPE_INPUT, 1));
        assert!(!registry.is_empty());
        registry.collect(T2);
        assert!(registry.is_empty());
    }

    #[test]
    fn clearing_discards_every_point() {
        let mut registry = Registry::new(Temporality::Cumulative, T0);
        registry.record(tokens(TYPE_INPUT, 1));
        registry.clear();
        assert!(registry.is_empty());
        assert!(registry.collect(T1).is_empty());
    }
}
