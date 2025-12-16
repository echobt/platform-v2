//! Anti-Cheat Weight Calculation
//!
//! Calculates weights for agents in a way that prevents validator manipulation:
//!
//! 1. **Outlier Detection**: Remove scores that deviate significantly from median
//! 2. **Consensus Requirement**: Agent must have valid scores from ≥50% of validators
//! 3. **Top Position Requirement**: Agent must be #1 on ≥50% of validators (by count, not stake)
//! 4. **Stake-Weighted Average**: Final score uses stake weights (after outlier removal)
//! 5. **Slashing Detection**: Flag validators whose scores consistently deviate
//!
//! This prevents:
//! - Single high-stake validator from manipulating results
//! - Colluding minority from pushing bad agents
//! - Validators from lying about evaluation results

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info, warn};

/// Configuration for anti-cheat weight calculation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiCheatConfig {
    /// Minimum percentage of validators that must have scored an agent (0.0 - 1.0)
    pub min_validator_coverage: f64,
    /// Minimum percentage of validators where agent must be top 1 (0.0 - 1.0)
    pub min_top_position_ratio: f64,
    /// Standard deviations from median to consider as outlier
    pub outlier_sigma: f64,
    /// Maximum allowed deviation from median score (as fraction)
    pub max_score_deviation: f64,
    /// Minimum number of validators for valid calculation
    pub min_validators: usize,
    /// Score tolerance for considering two scores "agreeing" (0.0 - 1.0)
    pub agreement_tolerance: f64,
    /// Whether to enable slashing detection
    pub enable_slashing_detection: bool,
    /// Number of consecutive outlier reports before flagging for slash
    pub slash_threshold: u32,
}

impl Default for AntiCheatConfig {
    fn default() -> Self {
        Self {
            min_validator_coverage: 0.50, // 50% of validators must score
            min_top_position_ratio: 0.50, // Must be #1 on 50% of validators
            outlier_sigma: 2.0,           // 2 standard deviations
            max_score_deviation: 0.20,    // Max 20% deviation from median
            min_validators: 3,            // At least 3 validators
            agreement_tolerance: 0.10,    // 10% tolerance for agreement
            enable_slashing_detection: true,
            slash_threshold: 3, // 3 strikes = flagged
        }
    }
}

/// Score submitted by a validator for an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorScore {
    pub validator_hotkey: String,
    pub validator_stake: u64,
    pub agent_hash: String,
    pub score: f64, // 0.0 - 1.0
    pub tasks_completed: u32,
    pub tasks_total: u32,
    pub epoch: u64,
    pub timestamp: u64,
}

/// Result of anti-cheat weight calculation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeightCalculationResult {
    /// Agent rankings with final scores
    pub rankings: Vec<AgentRanking>,
    /// Validators flagged as potential cheaters
    pub flagged_validators: Vec<FlaggedValidator>,
    /// Statistics about the calculation
    pub stats: CalculationStats,
    /// Epoch
    pub epoch: u64,
}

/// Agent ranking entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRanking {
    pub agent_hash: String,
    /// Miner UID (for weight submission)
    pub miner_uid: u16,
    /// Final score after anti-cheat processing
    pub final_score: f64,
    /// Raw average score (before anti-cheat)
    pub raw_score: f64,
    /// Number of validators who scored this agent
    pub validator_count: u32,
    /// Number of validators where this agent is #1
    pub top_position_count: u32,
    /// Stake coverage (sum of stake of validators who scored)
    pub stake_coverage: u64,
    /// Whether agent passed all anti-cheat checks
    pub is_valid: bool,
    /// Reasons if not valid
    pub rejection_reasons: Vec<String>,
    /// Normalized weight [0, 65535] for Bittensor
    pub weight: u16,
}

/// Flagged validator info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlaggedValidator {
    pub hotkey: String,
    pub reason: FlagReason,
    pub deviation_count: u32,
    pub average_deviation: f64,
    pub should_slash: bool,
}

/// Reason for flagging a validator
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FlagReason {
    /// Scores consistently deviate from median
    ConsistentOutlier { deviation_pct: f64 },
    /// Scores favor specific agent suspiciously
    SuspiciousFavoritism { agent_hash: String },
    /// Scores are always at extreme (0 or 1)
    ExtremeScoring,
    /// Scores don't match reproducible results
    ResultMismatch,
}

/// Calculation statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalculationStats {
    pub total_agents: u32,
    pub valid_agents: u32,
    pub total_validators: u32,
    pub active_validators: u32,
    pub outliers_removed: u32,
    pub total_scores_processed: u32,
}

/// Anti-cheat weight calculator
pub struct AntiCheatWeightCalculator {
    config: AntiCheatConfig,
    /// Track validator outlier history for slashing detection
    validator_outlier_count: HashMap<String, u32>,
}

impl AntiCheatWeightCalculator {
    pub fn new(config: AntiCheatConfig) -> Self {
        Self {
            config,
            validator_outlier_count: HashMap::new(),
        }
    }

    /// Calculate weights with anti-cheat measures
    pub fn calculate(
        &mut self,
        scores: &[ValidatorScore],
        validator_stakes: &HashMap<String, u64>,
        agent_uids: &HashMap<String, u16>,
        epoch: u64,
    ) -> WeightCalculationResult {
        let total_validators = validator_stakes.len();
        let total_stake: u64 = validator_stakes.values().sum();

        if total_validators < self.config.min_validators {
            warn!(
                "Not enough validators: {} < {}",
                total_validators, self.config.min_validators
            );
            return WeightCalculationResult {
                rankings: vec![],
                flagged_validators: vec![],
                stats: CalculationStats {
                    total_agents: 0,
                    valid_agents: 0,
                    total_validators: total_validators as u32,
                    active_validators: 0,
                    outliers_removed: 0,
                    total_scores_processed: 0,
                },
                epoch,
            };
        }

        // Group scores by agent
        let mut agent_scores: HashMap<String, Vec<&ValidatorScore>> = HashMap::new();
        for score in scores {
            agent_scores
                .entry(score.agent_hash.clone())
                .or_default()
                .push(score);
        }

        // For each validator, determine which agent is their #1
        let mut validator_top_agent: HashMap<String, String> = HashMap::new();
        let validators: HashSet<_> = scores.iter().map(|s| &s.validator_hotkey).collect();

        for validator in &validators {
            let validator_scores: Vec<_> = scores
                .iter()
                .filter(|s| &s.validator_hotkey == *validator)
                .collect();

            if let Some(top) = validator_scores.iter().max_by(|a, b| {
                a.score
                    .partial_cmp(&b.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                validator_top_agent.insert(validator.to_string(), top.agent_hash.clone());
            }
        }

        let mut rankings = Vec::new();
        let mut total_outliers = 0u32;
        let mut flagged_validators = Vec::new();

        // Process each agent
        for (agent_hash, agent_validator_scores) in &agent_scores {
            let result = self.process_agent(
                agent_hash,
                agent_validator_scores,
                validator_stakes,
                &validator_top_agent,
                total_validators,
                total_stake,
                agent_uids.get(agent_hash).copied().unwrap_or(0),
            );

            total_outliers += result.outliers_removed;

            // Track outlier validators
            for outlier in &result.outlier_validators {
                *self
                    .validator_outlier_count
                    .entry(outlier.clone())
                    .or_default() += 1;
            }

            rankings.push(result.ranking);
        }

        // Check for validators to flag
        if self.config.enable_slashing_detection {
            for (validator, count) in &self.validator_outlier_count {
                if *count >= self.config.slash_threshold {
                    flagged_validators.push(FlaggedValidator {
                        hotkey: validator.clone(),
                        reason: FlagReason::ConsistentOutlier {
                            deviation_pct: (*count as f64 / agent_scores.len() as f64) * 100.0,
                        },
                        deviation_count: *count,
                        average_deviation: 0.0, // Deviation calculation pending
                        should_slash: true,
                    });
                }
            }
        }

        // Sort rankings by final score (descending)
        rankings.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Calculate normalized weights
        let valid_rankings: Vec<_> = rankings.iter().filter(|r| r.is_valid).collect();
        let total_score: f64 = valid_rankings.iter().map(|r| r.final_score).sum();

        for ranking in &mut rankings {
            if ranking.is_valid && total_score > 0.0 {
                let normalized = ranking.final_score / total_score;
                ranking.weight = (normalized * 65535.0).round() as u16;
            } else {
                ranking.weight = 0;
            }
        }

        let valid_count = rankings.iter().filter(|r| r.is_valid).count();

        info!(
            "Calculated weights for {} agents ({} valid), {} outliers removed, {} validators flagged",
            rankings.len(),
            valid_count,
            total_outliers,
            flagged_validators.len()
        );

        WeightCalculationResult {
            rankings,
            flagged_validators,
            stats: CalculationStats {
                total_agents: agent_scores.len() as u32,
                valid_agents: valid_count as u32,
                total_validators: total_validators as u32,
                active_validators: validators.len() as u32,
                outliers_removed: total_outliers,
                total_scores_processed: scores.len() as u32,
            },
            epoch,
        }
    }

    /// Process a single agent's scores
    fn process_agent(
        &self,
        agent_hash: &str,
        scores: &[&ValidatorScore],
        validator_stakes: &HashMap<String, u64>,
        validator_top_agent: &HashMap<String, String>,
        total_validators: usize,
        _total_stake: u64,
        miner_uid: u16,
    ) -> AgentProcessingResult {
        let mut rejection_reasons = Vec::new();
        let mut outlier_validators = Vec::new();

        // Step 1: Calculate median and standard deviation
        let mut score_values: Vec<f64> = scores.iter().map(|s| s.score).collect();
        score_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let median = if score_values.is_empty() {
            0.0
        } else {
            let mid = score_values.len() / 2;
            if score_values.len() % 2 == 0 {
                (score_values[mid - 1] + score_values[mid]) / 2.0
            } else {
                score_values[mid]
            }
        };

        let variance: f64 = score_values
            .iter()
            .map(|s| (s - median).powi(2))
            .sum::<f64>()
            / score_values.len().max(1) as f64;
        let std_dev = variance.sqrt();

        // Step 2: Filter outliers
        let outlier_threshold = self.config.outlier_sigma * std_dev;
        let max_deviation = self.config.max_score_deviation;

        let valid_scores: Vec<&ValidatorScore> = scores
            .iter()
            .filter(|s| {
                let deviation = (s.score - median).abs();
                let is_outlier = deviation > outlier_threshold
                    || (median > 0.0 && deviation / median > max_deviation);

                if is_outlier {
                    outlier_validators.push(s.validator_hotkey.clone());
                    debug!(
                        "Outlier detected: {} gave {} score={:.4} (median={:.4}, dev={:.4})",
                        s.validator_hotkey, agent_hash, s.score, median, deviation
                    );
                }

                !is_outlier
            })
            .copied()
            .collect();

        let outliers_removed = (scores.len() - valid_scores.len()) as u32;

        // Step 3: Check validator coverage
        let validator_coverage = valid_scores.len() as f64 / total_validators as f64;
        if validator_coverage < self.config.min_validator_coverage {
            rejection_reasons.push(format!(
                "Insufficient validator coverage: {:.1}% < {:.1}%",
                validator_coverage * 100.0,
                self.config.min_validator_coverage * 100.0
            ));
        }

        // Step 4: Check top position ratio
        let top_position_count = validator_top_agent
            .iter()
            .filter(|(_, top_agent)| *top_agent == agent_hash)
            .count();
        let top_position_ratio = top_position_count as f64 / total_validators as f64;

        if top_position_ratio < self.config.min_top_position_ratio {
            rejection_reasons.push(format!(
                "Not top on enough validators: {:.1}% < {:.1}%",
                top_position_ratio * 100.0,
                self.config.min_top_position_ratio * 100.0
            ));
        }

        // Step 5: Calculate stake-weighted average (from valid scores only)
        let mut weighted_sum = 0.0;
        let mut total_weight = 0u64;

        for score in &valid_scores {
            let stake = validator_stakes
                .get(&score.validator_hotkey)
                .copied()
                .unwrap_or(0);
            weighted_sum += score.score * stake as f64;
            total_weight += stake;
        }

        let final_score = if total_weight > 0 {
            weighted_sum / total_weight as f64
        } else {
            0.0
        };

        // Raw score (simple average, no stake weighting)
        let raw_score = if scores.is_empty() {
            0.0
        } else {
            scores.iter().map(|s| s.score).sum::<f64>() / scores.len() as f64
        };

        // Stake coverage
        let stake_coverage: u64 = valid_scores
            .iter()
            .filter_map(|s| validator_stakes.get(&s.validator_hotkey))
            .sum();

        let is_valid = rejection_reasons.is_empty();

        AgentProcessingResult {
            ranking: AgentRanking {
                agent_hash: agent_hash.to_string(),
                miner_uid,
                final_score,
                raw_score,
                validator_count: valid_scores.len() as u32,
                top_position_count: top_position_count as u32,
                stake_coverage,
                is_valid,
                rejection_reasons,
                weight: 0, // Calculated later
            },
            outliers_removed,
            outlier_validators,
        }
    }

    /// Get weights in Bittensor format: Vec<(uid, weight)>
    pub fn to_bittensor_weights(result: &WeightCalculationResult) -> Vec<(u16, u16)> {
        result
            .rankings
            .iter()
            .filter(|r| r.is_valid && r.weight > 0)
            .map(|r| (r.miner_uid, r.weight))
            .collect()
    }

    /// Reset outlier tracking for new epoch
    pub fn reset_epoch(&mut self) {
        self.validator_outlier_count.clear();
    }
}

/// Internal result for agent processing
struct AgentProcessingResult {
    ranking: AgentRanking,
    outliers_removed: u32,
    outlier_validators: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_score(validator: &str, stake: u64, agent: &str, score: f64) -> ValidatorScore {
        ValidatorScore {
            validator_hotkey: validator.to_string(),
            validator_stake: stake,
            agent_hash: agent.to_string(),
            score,
            tasks_completed: 10,
            tasks_total: 10,
            epoch: 1,
            timestamp: 0,
        }
    }

    #[test]
    fn test_outlier_detection() {
        let config = AntiCheatConfig {
            min_validators: 3,
            outlier_sigma: 2.0,
            ..Default::default()
        };
        let mut calc = AntiCheatWeightCalculator::new(config);

        let scores = vec![
            make_score("v1", 1000, "agent1", 0.85),
            make_score("v2", 1000, "agent1", 0.83),
            make_score("v3", 1000, "agent1", 0.84),
            make_score("v4", 10000, "agent1", 0.95), // Outlier - high stake cheater
            make_score("v5", 1000, "agent1", 0.82),
        ];

        let stakes: HashMap<_, _> = vec![
            ("v1".to_string(), 1000u64),
            ("v2".to_string(), 1000),
            ("v3".to_string(), 1000),
            ("v4".to_string(), 10000),
            ("v5".to_string(), 1000),
        ]
        .into_iter()
        .collect();

        let uids: HashMap<_, _> = vec![("agent1".to_string(), 1u16)].into_iter().collect();

        let result = calc.calculate(&scores, &stakes, &uids, 1);

        assert_eq!(result.stats.outliers_removed, 1);
        assert_eq!(result.rankings.len(), 1);

        // Final score should be close to 0.835 (avg of non-outliers), not 0.858 (with outlier)
        let agent1 = &result.rankings[0];
        assert!(
            (agent1.final_score - 0.835).abs() < 0.01,
            "Score should be ~0.835, got {}",
            agent1.final_score
        );
    }

    #[test]
    fn test_top_position_requirement() {
        let config = AntiCheatConfig {
            min_validators: 3,
            min_top_position_ratio: 0.50,
            min_validator_coverage: 0.50,
            ..Default::default()
        };
        let mut calc = AntiCheatWeightCalculator::new(config);

        // Agent1 is top on 3/5 validators (60%), Agent2 on 2/5 (40%)
        let scores = vec![
            // V1: Agent1 top
            make_score("v1", 1000, "agent1", 0.90),
            make_score("v1", 1000, "agent2", 0.85),
            // V2: Agent1 top
            make_score("v2", 1000, "agent1", 0.88),
            make_score("v2", 1000, "agent2", 0.80),
            // V3: Agent1 top
            make_score("v3", 1000, "agent1", 0.87),
            make_score("v3", 1000, "agent2", 0.75),
            // V4: Agent2 top
            make_score("v4", 1000, "agent1", 0.70),
            make_score("v4", 1000, "agent2", 0.95),
            // V5: Agent2 top
            make_score("v5", 1000, "agent1", 0.72),
            make_score("v5", 1000, "agent2", 0.92),
        ];

        let stakes: HashMap<_, _> = vec![
            ("v1".to_string(), 1000u64),
            ("v2".to_string(), 1000),
            ("v3".to_string(), 1000),
            ("v4".to_string(), 1000),
            ("v5".to_string(), 1000),
        ]
        .into_iter()
        .collect();

        let uids: HashMap<_, _> = vec![("agent1".to_string(), 1u16), ("agent2".to_string(), 2u16)]
            .into_iter()
            .collect();

        let result = calc.calculate(&scores, &stakes, &uids, 1);

        // Agent1 should be valid (top on 60% >= 50%)
        let agent1 = result
            .rankings
            .iter()
            .find(|r| r.agent_hash == "agent1")
            .unwrap();
        assert!(
            agent1.is_valid,
            "Agent1 should be valid: {:?}",
            agent1.rejection_reasons
        );
        assert_eq!(agent1.top_position_count, 3);

        // Agent2 should be invalid (top on 40% < 50%)
        let agent2 = result
            .rankings
            .iter()
            .find(|r| r.agent_hash == "agent2")
            .unwrap();
        assert!(!agent2.is_valid, "Agent2 should be invalid");
        assert_eq!(agent2.top_position_count, 2);
    }

    #[test]
    fn test_stake_weighted_but_fair() {
        let config = AntiCheatConfig {
            min_validators: 3,
            outlier_sigma: 2.0,
            ..Default::default()
        };
        let mut calc = AntiCheatWeightCalculator::new(config);

        // High stake validator has slightly different score but within tolerance
        let scores = vec![
            make_score("v1", 1000, "agent1", 0.80),
            make_score("v2", 1000, "agent1", 0.82),
            make_score("v3", 5000, "agent1", 0.85), // Higher stake, slightly higher score (but valid)
        ];

        let stakes: HashMap<_, _> = vec![
            ("v1".to_string(), 1000u64),
            ("v2".to_string(), 1000),
            ("v3".to_string(), 5000),
        ]
        .into_iter()
        .collect();

        let uids: HashMap<_, _> = vec![("agent1".to_string(), 1u16)].into_iter().collect();

        let result = calc.calculate(&scores, &stakes, &uids, 1);

        // All scores should be valid (within tolerance)
        assert_eq!(result.stats.outliers_removed, 0);

        // But stake weighting means v3's score counts more
        // Expected: (0.80*1000 + 0.82*1000 + 0.85*5000) / 7000 = 0.837
        let agent1 = &result.rankings[0];
        assert!((agent1.final_score - 0.837).abs() < 0.01);
    }

    #[test]
    fn test_validator_flagging() {
        let mut config = AntiCheatConfig {
            min_validators: 3,
            enable_slashing_detection: true,
            slash_threshold: 2,
            ..Default::default()
        };
        let mut calc = AntiCheatWeightCalculator::new(config);

        let stakes: HashMap<_, _> = vec![
            ("v1".to_string(), 1000u64),
            ("v2".to_string(), 1000),
            ("v3".to_string(), 1000),
            ("cheater".to_string(), 5000),
        ]
        .into_iter()
        .collect();

        let uids: HashMap<_, _> = vec![("agent1".to_string(), 1u16), ("agent2".to_string(), 2u16)]
            .into_iter()
            .collect();

        // First calculation - cheater is outlier for agent1
        let scores1 = vec![
            make_score("v1", 1000, "agent1", 0.80),
            make_score("v2", 1000, "agent1", 0.82),
            make_score("v3", 1000, "agent1", 0.81),
            make_score("cheater", 5000, "agent1", 0.99), // Way too high
        ];
        calc.calculate(&scores1, &stakes, &uids, 1);

        // Second calculation - cheater is outlier again for agent2
        let scores2 = vec![
            make_score("v1", 1000, "agent2", 0.70),
            make_score("v2", 1000, "agent2", 0.72),
            make_score("v3", 1000, "agent2", 0.71),
            make_score("cheater", 5000, "agent2", 0.98), // Way too high again
        ];
        let result = calc.calculate(&scores2, &stakes, &uids, 1);

        // Cheater should be flagged
        assert!(!result.flagged_validators.is_empty());
        let flagged = result
            .flagged_validators
            .iter()
            .find(|f| f.hotkey == "cheater");
        assert!(flagged.is_some(), "Cheater should be flagged");
        assert!(flagged.unwrap().should_slash);
    }
}
