//! Tests for Bittensor integration

#[cfg(test)]
mod tests {
    use crate::{BittensorConfig, SubtensorClient, WeightSubmitter, DEFAULT_NETUID};
    use platform_challenge_sdk::WeightAssignment;

    #[test]
    fn test_config_default() {
        let config = BittensorConfig::default();
        assert_eq!(config.netuid, DEFAULT_NETUID); // NETUID 100 by default
        assert!(config.use_commit_reveal);
        assert!(config.endpoint.contains("finney"));
    }

    #[test]
    fn test_config_testnet() {
        let config = BittensorConfig::testnet(42);
        assert_eq!(config.netuid, 42);
        assert!(config.endpoint.contains("test"));
    }

    #[test]
    fn test_config_local() {
        let config = BittensorConfig::local(1);
        assert_eq!(config.netuid, 1);
        assert!(!config.use_commit_reveal);
        assert!(config.endpoint.contains("127.0.0.1"));
    }

    #[test]
    fn test_client_creation() {
        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        assert_eq!(client.netuid(), 1);
        assert!(!client.use_commit_reveal());
    }

    #[test]
    fn test_submitter_creation() {
        let config = BittensorConfig::local(1);
        let client = SubtensorClient::new(config);
        let submitter = WeightSubmitter::new(client);
        assert!(!submitter.has_pending_commit());
    }

    #[test]
    fn test_normalize_to_u16() {
        use crate::weights::normalize_to_u16;

        let weights = vec![0.5, 0.3, 0.2];
        let normalized = normalize_to_u16(&weights);

        // Should sum to ~65535
        let sum: u32 = normalized.iter().map(|&w| w as u32).sum();
        assert!(sum > 65000 && sum <= 65535);

        // First should be largest
        assert!(normalized[0] > normalized[1]);
        assert!(normalized[1] > normalized[2]);
    }

    #[test]
    fn test_normalize_to_u16_zero() {
        use crate::weights::normalize_to_u16;

        let weights = vec![0.0, 0.0, 0.0];
        let normalized = normalize_to_u16(&weights);

        assert_eq!(normalized, vec![0, 0, 0]);
    }

    #[test]
    fn test_weight_assignment_conversion() {
        // Test that WeightAssignment can be created
        let assignment = WeightAssignment::new(
            "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY".to_string(),
            0.5,
        );
        assert_eq!(assignment.weight, 0.5);
    }

    // Integration tests (require network)
    #[tokio::test]
    #[ignore] // Run with: cargo test -- --ignored
    async fn test_connect_to_testnet() {
        let config = BittensorConfig::testnet(1);
        let mut client = SubtensorClient::new(config);

        let result = client.connect().await;
        assert!(result.is_ok(), "Failed to connect to testnet");
    }

    #[tokio::test]
    #[ignore]
    async fn test_sync_metagraph() {
        let config = BittensorConfig::testnet(1);
        let mut client = SubtensorClient::new(config);

        client.connect().await.expect("Failed to connect");
        let metagraph = client.sync_metagraph().await;

        assert!(metagraph.is_ok(), "Failed to sync metagraph");
        let mg = metagraph.unwrap();
        assert!(mg.n > 0, "Metagraph should have neurons");
    }
}
