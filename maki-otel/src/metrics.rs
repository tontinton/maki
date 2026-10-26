//! Metric definitions. Every maki metric is a monotonic counter, aggregated
//! by the SDK.

use opentelemetry::metrics::{Counter, Meter};

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

pub(crate) struct Instruments {
    pub(crate) session_count: Counter<u64>,
    pub(crate) token_usage: Counter<u64>,
    pub(crate) cost_usage: Counter<f64>,
    pub(crate) lines_of_code: Counter<u64>,
    pub(crate) tool_decision: Counter<u64>,
    pub(crate) commit_count: Counter<u64>,
    pub(crate) pull_request_count: Counter<u64>,
    pub(crate) active_time: Counter<f64>,
}

impl Instruments {
    pub(crate) fn new(meter: &Meter) -> Self {
        let int = |def: &MetricDef| {
            meter
                .u64_counter(def.name)
                .with_unit(def.unit)
                .with_description(def.description)
                .build()
        };
        let float = |def: &MetricDef| {
            meter
                .f64_counter(def.name)
                .with_unit(def.unit)
                .with_description(def.description)
                .build()
        };
        Self {
            session_count: int(&SESSION_COUNT),
            token_usage: int(&TOKEN_USAGE),
            cost_usage: float(&COST_USAGE),
            lines_of_code: int(&LINES_OF_CODE),
            tool_decision: int(&TOOL_DECISION),
            commit_count: int(&COMMIT_COUNT),
            pull_request_count: int(&PULL_REQUEST_COUNT),
            active_time: float(&ACTIVE_TIME),
        }
    }
}
