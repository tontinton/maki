/// Estimates token cost for a sample multi-turn conversation at different
/// Anthropic prompt cache breakpoint values.
///
/// Usage: cargo run --bin anthropic_cache_benchmark
///
/// This script simulates a conversation with N turns and estimates the cost
/// difference between breakpoint values 0, 1, 2, and 3. The actual optimal
/// value depends on conversation length and cache hit patterns.
use std::collections::HashMap;

const SYSTEM_TOKENS: u32 = 2000;
const TOOLS_TOKENS: u32 = 5000;
const USER_MESSAGE_TOKENS: u32 = 500;
const ASSISTANT_MESSAGE_TOKENS: u32 = 1000;
const NUM_TURNS: u32 = 10;

/// Anthropic Sonnet 4.6 pricing (per 1M tokens)
const INPUT_PRICE: f64 = 3.00;
const OUTPUT_PRICE: f64 = 15.00;
const CACHE_WRITE_PRICE: f64 = 3.75;
const CACHE_READ_PRICE: f64 = 0.30;

#[derive(Debug, Clone)]
struct TurnCost {
    input: u32,
    output: u32,
    cache_write: u32,
    cache_read: u32,
}

fn estimate_turn_cost(turn: u32, breakpoints: usize, _prev_turns: &[TurnCost]) -> TurnCost {
    let total_messages = (turn + 1) * 2;
    let breakpoints_u32 = breakpoints as u32;

    let mut cache_write = 0;
    let mut cache_read = 0;

    if turn == 0 {
        cache_write = SYSTEM_TOKENS + TOOLS_TOKENS;
    } else {
        cache_read = SYSTEM_TOKENS + TOOLS_TOKENS;
    }

    let messages_to_cache = breakpoints_u32.min(total_messages);
    let cached_messages_start = total_messages.saturating_sub(messages_to_cache);

    for msg_idx in 0..total_messages {
        let is_cached = msg_idx >= cached_messages_start;
        let is_last_in_cached = msg_idx == total_messages - 1 && is_cached;

        if is_last_in_cached {
            if turn == 0 {
                cache_write += USER_MESSAGE_TOKENS;
            } else {
                cache_read += USER_MESSAGE_TOKENS;
            }
        }
    }

    let input = if turn == 0 {
        SYSTEM_TOKENS + TOOLS_TOKENS + USER_MESSAGE_TOKENS
    } else {
        USER_MESSAGE_TOKENS
    };

    let output = ASSISTANT_MESSAGE_TOKENS;

    TurnCost {
        input,
        output,
        cache_write,
        cache_read,
    }
}

fn calculate_total_cost(costs: &[TurnCost]) -> f64 {
    let total_input: u32 = costs.iter().map(|c| c.input).sum();
    let total_output: u32 = costs.iter().map(|c| c.output).sum();
    let total_cache_write: u32 = costs.iter().map(|c| c.cache_write).sum();
    let total_cache_read: u32 = costs.iter().map(|c| c.cache_read).sum();

    (total_input as f64 * INPUT_PRICE
        + total_output as f64 * OUTPUT_PRICE
        + total_cache_write as f64 * CACHE_WRITE_PRICE
        + total_cache_read as f64 * CACHE_READ_PRICE)
        / 1_000_000.0
}

fn main() {
    println!("Anthropic Prompt Cache Breakpoint Cost Estimation");
    println!("==================================================");
    println!();
    println!("Parameters:");
    println!("  System prompt: {} tokens", SYSTEM_TOKENS);
    println!("  Tools: {} tokens", TOOLS_TOKENS);
    println!("  User message avg: {} tokens", USER_MESSAGE_TOKENS);
    println!(
        "  Assistant message avg: {} tokens",
        ASSISTANT_MESSAGE_TOKENS
    );
    println!("  Turns: {}", NUM_TURNS);
    println!();
    println!("Pricing (Sonnet 4.6 per 1M tokens):");
    println!("  Input: ${:.2}", INPUT_PRICE);
    println!("  Output: ${:.2}", OUTPUT_PRICE);
    println!("  Cache write: ${:.2}", CACHE_WRITE_PRICE);
    println!("  Cache read: ${:.2}", CACHE_READ_PRICE);
    println!();

    let mut results: HashMap<usize, f64> = HashMap::new();

    for &breakpoints in &[0, 1, 2, 3] {
        let mut costs = Vec::new();
        for turn in 0..NUM_TURNS {
            let cost = estimate_turn_cost(turn, breakpoints, &costs);
            costs.push(cost);
        }
        let total_cost = calculate_total_cost(&costs);
        results.insert(breakpoints, total_cost);

        let total_input: u32 = costs.iter().map(|c| c.input).sum();
        let total_output: u32 = costs.iter().map(|c| c.output).sum();
        let total_cache_write: u32 = costs.iter().map(|c| c.cache_write).sum();
        let total_cache_read: u32 = costs.iter().map(|c| c.cache_read).sum();

        println!("Breakpoints: {}", breakpoints);
        println!("  Total input: {} tokens", total_input);
        println!("  Total output: {} tokens", total_output);
        println!("  Total cache write: {} tokens", total_cache_write);
        println!("  Total cache read: {} tokens", total_cache_read);
        println!("  Estimated cost: ${:.6}", total_cost);
        println!();
    }

    println!("Recommendation:");
    let min_cost = results.values().cloned().fold(f64::INFINITY, f64::min);
    let mut best_breakpoints = 2;
    for (&b, &cost) in &results {
        if (cost - min_cost).abs() < 1e-9 {
            best_breakpoints = b;
            break;
        }
    }

    println!(
        "  Lowest cost: ${:.6} with {} breakpoints",
        min_cost, best_breakpoints
    );
    println!();
    println!("Note: This is a simplified model. Real-world performance depends on:");
    println!("  - Actual conversation length and message distribution");
    println!("  - Cache hit patterns (whether recent messages are reused)");
    println!("  - System prompt and tools stability across turns");
    println!("  - Model pricing differences (Haiku, Sonnet, Opus)");
}
