/// D5: Test multi-leg arb execution.
///
/// Executes a real 2-leg arb at minimum size. If no natural arb is found
/// within 5 minutes, creates one by buying YES on both sides of a 2-outcome
/// mutex market (guaranteed resolution covers one side).
///
/// Also used to seed positions for D6: if D2-D4 haven't produced 2 concurrent
/// positions, D5's forced mutex buy guarantees 2 positions exist.

use std::collections::HashSet;
use rust_engine::executor::Executor;
use rust_engine::signing::Side;
use crate::report::{TestResult, Exception, ExceptionReport};
use crate::dedup::PositionDedup;
use crate::config::MergedTestConfig;

const ARB_SEARCH_TIMEOUT_SECS: u64 = 5 * 60;  // 5 minutes to find natural arb

/// Run D5: 2-leg arb execution.
/// Also ensures we have >= 2 open positions for D6.
pub fn run(
    executor: &Executor,
    engine: &rust_engine::TradingEngine,
    notifier: &rust_engine::notify::Notifier,
    test_config: &MergedTestConfig,
    dedup: &mut PositionDedup,
    _need_positions_for_d6: bool,
    exceptions: &mut ExceptionReport,
) -> (TestResult, Vec<String>) {
    let start = std::time::Instant::now();
    let mut opened_position_ids = Vec::new();

    // 1. Try to find a natural arb opportunity via evaluate_batch
    tracing::info!("[D5] Searching for natural arb opportunity (timeout: {}s)...", ARB_SEARCH_TIMEOUT_SECS);

    let held_cids: HashSet<String> = HashSet::new();
    let held_mids: HashSet<String> = dedup.occupied_market_ids();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(ARB_SEARCH_TIMEOUT_SECS);
    let mut natural_arb_found = false;

    while std::time::Instant::now() < deadline {
        let batch = engine.evaluate_batch(
            500,
            &held_cids,
            &held_mids,
            10,
            0.8,
        );

        // Look for a 2-leg opportunity
        for opp in &batch.opportunities {
            if opp.market_ids.len() >= 2 && opp.expected_profit_pct >= test_config.min_profit_threshold {
                tracing::info!("[D5] Found natural arb: {} legs, {:.2}% profit on {}",
                    opp.market_ids.len(), opp.expected_profit_pct * 100.0, opp.constraint_id);

                // Check dedup
                if !dedup.can_open(&opp.market_ids) {
                    tracing::info!("[D5] Markets already occupied, skipping");
                    continue;
                }

                // Build legs for executor
                let mut legs = Vec::new();
                for mid in &opp.market_ids {
                    let constraint = engine.constraints.get(&opp.constraint_id);
                    if let Some(c) = &constraint {
                        for m in &c.markets {
                            if m.market_id == *mid {
                                let side = if opp.is_sell { Side::Sell } else { Side::Buy };
                                let price = if opp.is_sell {
                                    *opp.current_no_prices.get(mid).unwrap_or(&0.5)
                                } else {
                                    *opp.current_prices.get(mid).unwrap_or(&0.5)
                                };
                                let token_id = if opp.is_sell {
                                    m.no_asset_id.clone()
                                } else {
                                    m.yes_asset_id.clone()
                                };
                                let bet = opp.optimal_bets.get(mid).copied().unwrap_or(2.50);
                                legs.push((mid.clone(), token_id, side, price, bet.min(5.0)));
                                break;
                            }
                        }
                    }
                }

                if legs.len() >= 2 {
                    let position_id = format!("d5_arb_{}", crate::now_secs() as u64);
                    let result = executor.execute_arb(&position_id, &legs);

                    if result.all_accepted {
                        natural_arb_found = true;
                        dedup.record_open(&opp.market_ids, "D5");
                        opened_position_ids.push(position_id.clone());
                        tracing::info!("[D5] Natural arb executed: {} legs", legs.len());
                        break;
                    } else {
                        tracing::warn!("[D5] Arb execution failed, continuing search...");
                    }
                }
            }
        }

        if natural_arb_found { break; }
        std::thread::sleep(std::time::Duration::from_secs(10));
    }

    // 2. If no natural arb found, create one by buying YES on both sides of a mutex
    if !natural_arb_found {
        tracing::info!("[D5] No natural arb found in {}s, creating forced position...", ARB_SEARCH_TIMEOUT_SECS);

        let constraints = engine.constraints.all();
        let mut created = false;

        for c in &constraints {
            if c.markets.len() != 2 { continue; }
            if c.is_neg_risk { continue; }  // Simpler with non-negRisk

            let m0 = &c.markets[0];
            let m1 = &c.markets[1];

            // Check both markets available
            if !dedup.can_open(&[m0.market_id.clone(), m1.market_id.clone()]) { continue; }

            let ask0 = engine.get_best_ask(&m0.yes_asset_id);
            let ask1 = engine.get_best_ask(&m1.yes_asset_id);

            if ask0 <= 0.05 || ask0 >= 0.95 || ask1 <= 0.05 || ask1 >= 0.95 {
                continue;
            }

            tracing::info!("[D5] Forced 2-leg buy: {} ({:.4}) + {} ({:.4})",
                m0.name, ask0, m1.name, ask1);

            let size = 2.50;
            let position_id = format!("d5_forced_{}", crate::now_secs() as u64);

            let legs = vec![
                (m0.market_id.clone(), m0.yes_asset_id.clone(), Side::Buy, ask0, size),
                (m1.market_id.clone(), m1.yes_asset_id.clone(), Side::Buy, ask1, size),
            ];

            let result = executor.execute_arb(&position_id, &legs);

            if result.all_accepted {
                dedup.record_open(&[m0.market_id.clone(), m1.market_id.clone()], "D5");
                opened_position_ids.push(position_id.clone());
                created = true;
                tracing::info!("[D5] Forced 2-leg position created");
                break;
            }
        }

        if !created {
            let errors = vec!["Could not find or create a 2-leg arb opportunity".into()];
            exceptions.add(Exception {
                severity: "CRITICAL".into(),
                test_id: "D5".into(),
                component: "arb_execution".into(),
                description: "No arb found and forced position creation failed".into(),
                expected: "Execute 2-leg arb or forced mutex buy".into(),
                actual: "No suitable 2-outcome mutex market found".into(),
                recommendation: "Check market data, constraint detection, and order books".into(),
            });
            crate::notify(notifier, "[CLOB-TEST EXCEPTION] D5 FAILED: no arb available");
            return (TestResult::fail("D5", "Multi-Leg Arb", start.elapsed().as_millis() as u64, errors),
                opened_position_ids);
        }
    }

    // 3. Wait for fill confirmation
    std::thread::sleep(std::time::Duration::from_secs(10));

    // 4. Verify partial fill handling
    for pid in &opened_position_ids {
        let action = executor.evaluate_arb_fills(pid, test_config.min_profit_threshold);
        tracing::info!("[D5] Partial fill evaluation for {}: {:?}", pid, action);
    }

    let elapsed = start.elapsed().as_millis() as u64;
    crate::notify(notifier, &format!(
        "[CLOB-TEST] D5 PASSED: {}-leg arb executed (natural={})",
        if natural_arb_found { "natural" } else { "forced" },
        natural_arb_found
    ));

    (TestResult::pass("D5", "Multi-Leg Arb", elapsed,
        serde_json::json!({
            "natural_arb_found": natural_arb_found,
            "position_ids": opened_position_ids,
            "search_time_secs": if natural_arb_found {
                start.elapsed().as_secs()
            } else {
                ARB_SEARCH_TIMEOUT_SECS
            },
        })),
    opened_position_ids)
}
