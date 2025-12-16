//! Network Protection Module
//!
//! Provides DDoS protection and stake-based access control:
//! - Rate limiting per peer
//! - Connection limits
//! - Minimum stake validation
//! - Blacklist management

use parking_lot::RwLock;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Minimum stake required to participate (1000 TAO in RAO)
pub const MIN_STAKE_RAO: u64 = 1_000_000_000_000; // 1000 TAO = 1000 * 10^9 RAO

/// Minimum stake in TAO
pub const MIN_STAKE_TAO: f64 = 1000.0;

/// Default rate limit (messages per second)
pub const DEFAULT_RATE_LIMIT: u32 = 100;

/// Default connection limit per IP
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: u32 = 5;

/// Default blacklist duration
pub const DEFAULT_BLACKLIST_DURATION_SECS: u64 = 3600; // 1 hour

/// Stake validation result
#[derive(Debug, Clone)]
pub enum StakeValidation {
    /// Stake is sufficient
    Valid { stake_tao: f64 },
    /// Stake is insufficient
    Insufficient { stake_tao: f64, required_tao: f64 },
    /// Hotkey not found in metagraph
    NotFound,
    /// Validation error
    Error(String),
}

impl StakeValidation {
    pub fn is_valid(&self) -> bool {
        matches!(self, StakeValidation::Valid { .. })
    }
}

/// Rate limiter for a single peer
#[derive(Debug, Clone)]
struct RateLimiter {
    /// Messages in current window
    count: u32,
    /// Window start time
    window_start: Instant,
    /// Messages per second limit
    limit: u32,
}

impl RateLimiter {
    fn new(limit: u32) -> Self {
        Self {
            count: 0,
            window_start: Instant::now(),
            limit,
        }
    }

    /// Check if request is allowed and update counter
    fn check(&mut self) -> bool {
        let now = Instant::now();

        // Reset window if expired (1 second window)
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.count = 0;
            self.window_start = now;
        }

        if self.count < self.limit {
            self.count += 1;
            true
        } else {
            false
        }
    }
}

/// Blacklist entry
#[derive(Debug, Clone)]
struct BlacklistEntry {
    /// When the entry was added
    added_at: Instant,
    /// Duration of the ban
    duration: Duration,
    /// Reason for blacklisting
    reason: String,
}

impl BlacklistEntry {
    fn is_expired(&self) -> bool {
        self.added_at.elapsed() >= self.duration
    }
}

/// Connection tracker for an IP
#[derive(Debug, Clone, Default)]
struct ConnectionTracker {
    /// Current number of connections
    count: u32,
    /// Last connection attempt
    last_attempt: Option<Instant>,
    /// Failed connection attempts (for detecting attacks)
    failed_attempts: u32,
}

/// Network protection configuration
#[derive(Debug, Clone)]
pub struct ProtectionConfig {
    /// Minimum stake required (in RAO)
    pub min_stake_rao: u64,
    /// Rate limit (messages per second per peer)
    pub rate_limit: u32,
    /// Maximum connections per IP
    pub max_connections_per_ip: u32,
    /// Blacklist duration in seconds
    pub blacklist_duration_secs: u64,
    /// Enable stake validation
    pub validate_stake: bool,
    /// Enable rate limiting
    pub rate_limiting: bool,
    /// Enable connection limiting
    pub connection_limiting: bool,
    /// Maximum failed attempts before auto-blacklist
    pub max_failed_attempts: u32,
}

impl Default for ProtectionConfig {
    fn default() -> Self {
        Self {
            min_stake_rao: MIN_STAKE_RAO,
            rate_limit: DEFAULT_RATE_LIMIT,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            blacklist_duration_secs: DEFAULT_BLACKLIST_DURATION_SECS,
            validate_stake: true,
            rate_limiting: true,
            connection_limiting: true,
            max_failed_attempts: 10,
        }
    }
}

/// Connected validator tracking info
#[derive(Debug, Clone)]
pub struct ConnectedValidator {
    /// Peer ID (libp2p)
    pub peer_id: String,
    /// Connection time
    pub connected_at: Instant,
    /// Last heartbeat
    pub last_seen: Instant,
    /// IP address
    pub ip: Option<IpAddr>,
}

/// Hotkey connection result
#[derive(Debug, Clone)]
pub enum HotkeyConnectionResult {
    /// Connection allowed (first connection for this hotkey)
    Allowed,
    /// Connection denied - hotkey already connected elsewhere
    AlreadyConnected {
        existing_peer: String,
        connected_since: Duration,
    },
    /// Connection denied - hotkey blacklisted
    Blacklisted { reason: String },
}

impl HotkeyConnectionResult {
    pub fn is_allowed(&self) -> bool {
        matches!(self, HotkeyConnectionResult::Allowed)
    }
}

/// Network protection manager
pub struct NetworkProtection {
    config: RwLock<ProtectionConfig>,
    /// Rate limiters per peer ID
    rate_limiters: RwLock<HashMap<String, RateLimiter>>,
    /// Connection counts per IP
    connections: RwLock<HashMap<IpAddr, ConnectionTracker>>,
    /// Blacklisted peers (by peer ID)
    blacklist_peers: RwLock<HashMap<String, BlacklistEntry>>,
    /// Blacklisted IPs
    blacklist_ips: RwLock<HashMap<IpAddr, BlacklistEntry>>,
    /// Validated stakes cache (hotkey hex -> stake in RAO)
    stake_cache: RwLock<HashMap<String, (u64, Instant)>>,
    /// Cache TTL
    cache_ttl: Duration,
    /// Connected validators by hotkey (CRITICAL: only one connection per hotkey globally)
    /// Maps hotkey_hex -> ConnectedValidator
    connected_hotkeys: RwLock<HashMap<String, ConnectedValidator>>,
    /// Reverse mapping: peer_id -> hotkey_hex (for cleanup on disconnect)
    peer_to_hotkey: RwLock<HashMap<String, String>>,
    /// Stake already counted (to prevent stake splitting attacks)
    /// When voting/consensus, each hotkey's stake is counted ONCE
    counted_stakes: RwLock<HashMap<String, u64>>,
}

impl NetworkProtection {
    /// Create a new protection manager
    pub fn new(config: ProtectionConfig) -> Self {
        Self {
            config: RwLock::new(config),
            rate_limiters: RwLock::new(HashMap::new()),
            connections: RwLock::new(HashMap::new()),
            blacklist_peers: RwLock::new(HashMap::new()),
            blacklist_ips: RwLock::new(HashMap::new()),
            stake_cache: RwLock::new(HashMap::new()),
            cache_ttl: Duration::from_secs(300), // 5 minute cache
            connected_hotkeys: RwLock::new(HashMap::new()),
            peer_to_hotkey: RwLock::new(HashMap::new()),
            counted_stakes: RwLock::new(HashMap::new()),
        }
    }

    /// Create with default config
    pub fn with_defaults() -> Self {
        Self::new(ProtectionConfig::default())
    }

    /// Update configuration
    pub fn update_config(&self, config: ProtectionConfig) {
        *self.config.write() = config;
    }

    /// Get current configuration
    pub fn config(&self) -> ProtectionConfig {
        self.config.read().clone()
    }

    /// Check if a peer is allowed (rate limit check)
    pub fn check_rate_limit(&self, peer_id: &str) -> bool {
        let config = self.config.read();
        if !config.rate_limiting {
            return true;
        }

        let mut limiters = self.rate_limiters.write();
        let limiter = limiters
            .entry(peer_id.to_string())
            .or_insert_with(|| RateLimiter::new(config.rate_limit));

        if limiter.check() {
            true
        } else {
            debug!("Rate limit exceeded for peer: {}", peer_id);
            false
        }
    }

    /// Check if an IP can connect
    pub fn check_connection(&self, ip: IpAddr) -> bool {
        let config = self.config.read();
        if !config.connection_limiting {
            return true;
        }

        // Check blacklist first
        if self.is_ip_blacklisted(&ip) {
            warn!("Rejected connection from blacklisted IP: {}", ip);
            return false;
        }

        let mut connections = self.connections.write();
        let tracker = connections.entry(ip).or_default();

        if tracker.count >= config.max_connections_per_ip {
            debug!("Connection limit reached for IP: {}", ip);
            tracker.failed_attempts += 1;

            // Auto-blacklist if too many failed attempts
            if tracker.failed_attempts >= config.max_failed_attempts {
                drop(connections);
                self.blacklist_ip(
                    ip,
                    Duration::from_secs(config.blacklist_duration_secs),
                    "Too many connection attempts".to_string(),
                );
            }
            return false;
        }

        tracker.count += 1;
        tracker.last_attempt = Some(Instant::now());
        true
    }

    /// Record a connection closed
    pub fn connection_closed(&self, ip: IpAddr) {
        let mut connections = self.connections.write();
        if let Some(tracker) = connections.get_mut(&ip) {
            tracker.count = tracker.count.saturating_sub(1);
        }
    }

    /// Check if a peer is blacklisted
    pub fn is_peer_blacklisted(&self, peer_id: &str) -> bool {
        let mut blacklist = self.blacklist_peers.write();

        // Check and remove expired entries
        if let Some(entry) = blacklist.get(peer_id) {
            if entry.is_expired() {
                blacklist.remove(peer_id);
                return false;
            }
            return true;
        }
        false
    }

    /// Check if an IP is blacklisted
    pub fn is_ip_blacklisted(&self, ip: &IpAddr) -> bool {
        let mut blacklist = self.blacklist_ips.write();

        if let Some(entry) = blacklist.get(ip) {
            if entry.is_expired() {
                blacklist.remove(ip);
                return false;
            }
            return true;
        }
        false
    }

    /// Blacklist a peer
    pub fn blacklist_peer(&self, peer_id: &str, duration: Duration, reason: String) {
        info!(
            "Blacklisting peer {} for {:?}: {}",
            peer_id, duration, reason
        );
        self.blacklist_peers.write().insert(
            peer_id.to_string(),
            BlacklistEntry {
                added_at: Instant::now(),
                duration,
                reason,
            },
        );
    }

    /// Blacklist an IP
    pub fn blacklist_ip(&self, ip: IpAddr, duration: Duration, reason: String) {
        info!("Blacklisting IP {} for {:?}: {}", ip, duration, reason);
        self.blacklist_ips.write().insert(
            ip,
            BlacklistEntry {
                added_at: Instant::now(),
                duration,
                reason,
            },
        );
    }

    /// Remove peer from blacklist
    pub fn unblacklist_peer(&self, peer_id: &str) {
        self.blacklist_peers.write().remove(peer_id);
    }

    /// Remove IP from blacklist
    pub fn unblacklist_ip(&self, ip: &IpAddr) {
        self.blacklist_ips.write().remove(ip);
    }

    // ==================== HOTKEY UNIQUENESS PROTECTION ====================
    // CRITICAL: Only ONE validator instance per hotkey is allowed globally.
    // This prevents:
    // 1. Double voting in consensus
    // 2. Stake multiplication attacks
    // 3. Sybil attacks using the same credentials

    /// Check if a hotkey can connect (must be unique globally)
    /// If hotkey is already connected from different peer, disconnect old and accept new
    /// (handles validator restarts with new peer_id)
    pub fn check_hotkey_connection(
        &self,
        hotkey_hex: &str,
        peer_id: &str,
        ip: Option<IpAddr>,
    ) -> HotkeyConnectionResult {
        let mut connected = self.connected_hotkeys.write();

        // Check if hotkey is already connected
        if let Some(existing) = connected.get(hotkey_hex) {
            // Same peer - just update last seen
            if existing.peer_id == peer_id {
                let mut entry = existing.clone();
                entry.last_seen = Instant::now();
                connected.insert(hotkey_hex.to_string(), entry);
                return HotkeyConnectionResult::Allowed;
            }

            // Different peer - disconnect old, accept new (validator probably restarted)
            let old_peer = existing.peer_id.clone();
            let connected_since = existing.connected_at.elapsed();

            warn!(
                "Hotkey {} reconnecting from new peer {} (was {} for {:?}) - disconnecting old",
                &hotkey_hex[..16.min(hotkey_hex.len())],
                peer_id,
                old_peer,
                connected_since
            );

            // Remove old peer from reverse mapping
            self.peer_to_hotkey.write().remove(&old_peer);
        }

        // Register new connection
        let validator = ConnectedValidator {
            peer_id: peer_id.to_string(),
            connected_at: Instant::now(),
            last_seen: Instant::now(),
            ip,
        };
        connected.insert(hotkey_hex.to_string(), validator);

        // Update reverse mapping
        self.peer_to_hotkey
            .write()
            .insert(peer_id.to_string(), hotkey_hex.to_string());

        info!(
            "Hotkey {} registered from peer {} (unique validator connection)",
            &hotkey_hex[..16.min(hotkey_hex.len())],
            peer_id
        );

        HotkeyConnectionResult::Allowed
    }

    /// Disconnect a hotkey (called when peer disconnects)
    pub fn disconnect_hotkey(&self, peer_id: &str) {
        // Find hotkey for this peer
        let hotkey = self.peer_to_hotkey.write().remove(peer_id);

        if let Some(hotkey_hex) = hotkey {
            self.connected_hotkeys.write().remove(&hotkey_hex);
            info!(
                "Hotkey {} disconnected (peer {})",
                &hotkey_hex[..16.min(hotkey_hex.len())],
                peer_id
            );
        }
    }

    /// Disconnect a hotkey by hotkey (for forced disconnect)
    pub fn disconnect_hotkey_by_key(&self, hotkey_hex: &str) {
        if let Some(validator) = self.connected_hotkeys.write().remove(hotkey_hex) {
            self.peer_to_hotkey.write().remove(&validator.peer_id);
            warn!(
                "Forced disconnect of hotkey {} (peer {})",
                &hotkey_hex[..16],
                validator.peer_id
            );
        }
    }

    /// Update heartbeat for a hotkey
    pub fn update_hotkey_heartbeat(&self, hotkey_hex: &str) {
        if let Some(validator) = self.connected_hotkeys.write().get_mut(hotkey_hex) {
            validator.last_seen = Instant::now();
        }
    }

    /// Check if a hotkey is currently connected
    pub fn is_hotkey_connected(&self, hotkey_hex: &str) -> bool {
        self.connected_hotkeys.read().contains_key(hotkey_hex)
    }

    /// Get all connected hotkeys
    pub fn get_connected_hotkeys(&self) -> Vec<String> {
        self.connected_hotkeys.read().keys().cloned().collect()
    }

    /// Get connected validator info for a hotkey
    pub fn get_connected_validator(&self, hotkey_hex: &str) -> Option<ConnectedValidator> {
        self.connected_hotkeys.read().get(hotkey_hex).cloned()
    }

    /// Cleanup stale connections (no heartbeat for timeout duration)
    pub fn cleanup_stale_hotkeys(&self, timeout: Duration) {
        let now = Instant::now();
        let mut to_remove = Vec::new();

        {
            let connected = self.connected_hotkeys.read();
            for (hotkey, validator) in connected.iter() {
                if now.duration_since(validator.last_seen) > timeout {
                    to_remove.push((hotkey.clone(), validator.peer_id.clone()));
                }
            }
        }

        for (hotkey, peer_id) in to_remove {
            self.connected_hotkeys.write().remove(&hotkey);
            self.peer_to_hotkey.write().remove(&peer_id);
            warn!(
                "Removed stale hotkey connection: {} (peer {}, no heartbeat for {:?})",
                &hotkey[..16.min(hotkey.len())],
                peer_id,
                timeout
            );
        }
    }

    /// Get number of connected validators
    pub fn connected_validator_count(&self) -> usize {
        self.connected_hotkeys.read().len()
    }

    // ==================== STAKE COUNTING PROTECTION ====================
    // CRITICAL: Each hotkey's stake must be counted ONLY ONCE in consensus.
    // This prevents stake multiplication attacks where the same entity
    // tries to gain more voting power by connecting multiple times.

    /// Register stake for consensus (returns true if this is the first registration)
    /// Use this when counting votes - each hotkey only counts once!
    pub fn register_stake_for_consensus(&self, hotkey_hex: &str, stake_rao: u64) -> bool {
        let mut counted = self.counted_stakes.write();

        if counted.contains_key(hotkey_hex) {
            // Already counted - don't count again!
            debug!(
                "SECURITY: Stake for {} already counted ({} RAO), ignoring duplicate",
                &hotkey_hex[..16],
                stake_rao
            );
            return false;
        }

        counted.insert(hotkey_hex.to_string(), stake_rao);
        true
    }

    /// Clear stake counting (call at start of new consensus round)
    pub fn reset_stake_counting(&self) {
        self.counted_stakes.write().clear();
    }

    /// Get total counted stake (for quorum calculations)
    pub fn get_total_counted_stake(&self) -> u64 {
        self.counted_stakes.read().values().sum()
    }

    /// Check if stake has been counted for a hotkey
    pub fn is_stake_counted(&self, hotkey_hex: &str) -> bool {
        self.counted_stakes.read().contains_key(hotkey_hex)
    }

    /// Validate stake for a hotkey (returns cached result if available)
    pub fn validate_stake(&self, hotkey_hex: &str, current_stake_rao: u64) -> StakeValidation {
        let config = self.config.read();
        if !config.validate_stake {
            return StakeValidation::Valid {
                stake_tao: current_stake_rao as f64 / 1_000_000_000.0,
            };
        }

        let stake_tao = current_stake_rao as f64 / 1_000_000_000.0;
        let required_tao = config.min_stake_rao as f64 / 1_000_000_000.0;

        if current_stake_rao >= config.min_stake_rao {
            // Cache the valid stake
            self.stake_cache
                .write()
                .insert(hotkey_hex.to_string(), (current_stake_rao, Instant::now()));
            StakeValidation::Valid { stake_tao }
        } else {
            StakeValidation::Insufficient {
                stake_tao,
                required_tao,
            }
        }
    }

    /// Check cached stake (for quick validation without network call)
    pub fn check_cached_stake(&self, hotkey_hex: &str) -> Option<StakeValidation> {
        let cache = self.stake_cache.read();
        if let Some((stake_rao, cached_at)) = cache.get(hotkey_hex) {
            if cached_at.elapsed() < self.cache_ttl {
                let config = self.config.read();
                let stake_tao = *stake_rao as f64 / 1_000_000_000.0;

                if *stake_rao >= config.min_stake_rao {
                    return Some(StakeValidation::Valid { stake_tao });
                } else {
                    return Some(StakeValidation::Insufficient {
                        stake_tao,
                        required_tao: config.min_stake_rao as f64 / 1_000_000_000.0,
                    });
                }
            }
        }
        None
    }

    /// Clear expired entries (call periodically)
    pub fn cleanup(&self) {
        // Clean rate limiters older than 1 minute
        let mut limiters = self.rate_limiters.write();
        limiters.retain(|_, l| l.window_start.elapsed() < Duration::from_secs(60));

        // Clean expired blacklist entries
        let mut peer_blacklist = self.blacklist_peers.write();
        peer_blacklist.retain(|_, e| !e.is_expired());

        let mut ip_blacklist = self.blacklist_ips.write();
        ip_blacklist.retain(|_, e| !e.is_expired());

        // Clean idle connection trackers
        let mut connections = self.connections.write();
        connections.retain(|_, t| {
            t.count > 0
                || t.last_attempt
                    .map(|i| i.elapsed() < Duration::from_secs(300))
                    .unwrap_or(false)
        });

        // Clean expired stake cache
        let mut cache = self.stake_cache.write();
        cache.retain(|_, (_, cached_at)| cached_at.elapsed() < self.cache_ttl);
    }

    /// Get protection statistics
    pub fn stats(&self) -> ProtectionStats {
        ProtectionStats {
            active_rate_limiters: self.rate_limiters.read().len(),
            active_connections: self
                .connections
                .read()
                .values()
                .map(|t| t.count as usize)
                .sum(),
            blacklisted_peers: self.blacklist_peers.read().len(),
            blacklisted_ips: self.blacklist_ips.read().len(),
            cached_stakes: self.stake_cache.read().len(),
        }
    }
}

/// Protection statistics
#[derive(Debug, Clone)]
pub struct ProtectionStats {
    pub active_rate_limiters: usize,
    pub active_connections: usize,
    pub blacklisted_peers: usize,
    pub blacklisted_ips: usize,
    pub cached_stakes: usize,
}

/// Helper function to validate stake meets minimum
pub fn stake_meets_minimum(stake_rao: u64) -> bool {
    stake_rao >= MIN_STAKE_RAO
}

/// Helper function to get minimum stake in TAO
pub fn minimum_stake_tao() -> f64 {
    MIN_STAKE_TAO
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter() {
        let mut limiter = RateLimiter::new(5);

        // Should allow first 5
        for _ in 0..5 {
            assert!(limiter.check());
        }

        // Should deny 6th
        assert!(!limiter.check());
    }

    #[test]
    fn test_stake_validation() {
        let protection = NetworkProtection::with_defaults();

        // 1000 TAO = 1000 * 10^9 RAO
        let valid_stake = 1_000_000_000_000u64;
        let result = protection.validate_stake("abc123", valid_stake);
        assert!(result.is_valid());

        // 500 TAO = insufficient
        let low_stake = 500_000_000_000u64;
        let result = protection.validate_stake("abc123", low_stake);
        assert!(!result.is_valid());
    }

    #[test]
    fn test_blacklist() {
        let protection = NetworkProtection::with_defaults();

        assert!(!protection.is_peer_blacklisted("peer1"));

        protection.blacklist_peer("peer1", Duration::from_secs(10), "test".to_string());

        assert!(protection.is_peer_blacklisted("peer1"));
    }

    #[test]
    fn test_min_stake_constant() {
        assert_eq!(MIN_STAKE_RAO, 1_000_000_000_000);
        assert_eq!(MIN_STAKE_TAO, 1000.0);
    }

    #[test]
    fn test_protection_config_default() {
        let config = ProtectionConfig::default();
        assert!(config.rate_limiting);
        assert!(config.connection_limiting);
        assert!(config.validate_stake);
    }

    #[test]
    fn test_stake_validation_variants() {
        let valid = StakeValidation::Valid { stake_tao: 1000.0 };
        assert!(valid.is_valid());

        let insufficient = StakeValidation::Insufficient {
            stake_tao: 500.0,
            required_tao: 1000.0,
        };
        assert!(!insufficient.is_valid());

        let not_found = StakeValidation::NotFound;
        assert!(!not_found.is_valid());

        let error = StakeValidation::Error("test".to_string());
        assert!(!error.is_valid());
    }

    #[test]
    fn test_stake_meets_minimum() {
        assert!(stake_meets_minimum(MIN_STAKE_RAO));
        assert!(stake_meets_minimum(MIN_STAKE_RAO + 1));
        assert!(!stake_meets_minimum(MIN_STAKE_RAO - 1));
        assert!(!stake_meets_minimum(0));
    }

    #[test]
    fn test_minimum_stake_tao() {
        assert_eq!(minimum_stake_tao(), 1000.0);
    }

    #[test]
    fn test_update_config() {
        let protection = NetworkProtection::with_defaults();

        let new_config = ProtectionConfig {
            min_stake_rao: MIN_STAKE_RAO,
            rate_limit: 200,
            max_connections_per_ip: 20,
            blacklist_duration_secs: 3600,
            validate_stake: false,
            rate_limiting: false,
            connection_limiting: false,
            max_failed_attempts: 10,
        };

        protection.update_config(new_config.clone());
        let retrieved = protection.config();
        assert_eq!(retrieved.rate_limiting, false);
        assert_eq!(retrieved.rate_limit, 200);
    }

    #[test]
    fn test_rate_limit_disabled() {
        let config = ProtectionConfig {
            rate_limiting: false,
            ..Default::default()
        };
        let protection = NetworkProtection::new(config);

        // Should always allow when disabled
        for _ in 0..100 {
            assert!(protection.check_rate_limit("peer1"));
        }
    }

    #[test]
    fn test_ip_blacklist() {
        let protection = NetworkProtection::with_defaults();
        let ip: IpAddr = "192.168.1.1".parse().unwrap();

        assert!(!protection.is_ip_blacklisted(&ip));

        protection.blacklist_ip(ip, Duration::from_secs(10), "test".to_string());
        assert!(protection.is_ip_blacklisted(&ip));
    }

    #[test]
    fn test_rate_limiter_window_reset() {
        let mut limiter = RateLimiter {
            count: 100,
            limit: 10,
            window_start: Instant::now() - Duration::from_secs(120),
        };

        // Should reset due to old window
        assert!(limiter.check());
        assert_eq!(limiter.count, 1);
    }

    // ==================== HOTKEY UNIQUENESS TESTS ====================

    #[test]
    fn test_hotkey_unique_connection() {
        let protection = NetworkProtection::with_defaults();
        let hotkey = "abcd1234567890abcdef";
        let peer1 = "peer1";
        let peer2 = "peer2";

        // First connection should be allowed
        let result = protection.check_hotkey_connection(hotkey, peer1, None);
        assert!(result.is_allowed());
        assert!(protection.is_hotkey_connected(hotkey));
        assert_eq!(protection.connected_validator_count(), 1);

        // Same hotkey from different peer should disconnect old and accept new
        // (handles validator restart with new peer_id)
        let result = protection.check_hotkey_connection(hotkey, peer2, None);
        assert!(result.is_allowed());
        // Verify it's now connected via peer2
        let validator = protection.get_connected_validator(hotkey).unwrap();
        assert_eq!(validator.peer_id, peer2);
        // Still only one validator connected
        assert_eq!(protection.connected_validator_count(), 1);

        // Same hotkey from same peer should be allowed (update heartbeat)
        let result = protection.check_hotkey_connection(hotkey, peer2, None);
        assert!(result.is_allowed());
    }

    #[test]
    fn test_hotkey_disconnect() {
        let protection = NetworkProtection::with_defaults();
        let hotkey = "abcd1234567890abcdef";
        let peer = "peer1";

        // Connect
        let result = protection.check_hotkey_connection(hotkey, peer, None);
        assert!(result.is_allowed());
        assert!(protection.is_hotkey_connected(hotkey));

        // Disconnect
        protection.disconnect_hotkey(peer);
        assert!(!protection.is_hotkey_connected(hotkey));
        assert_eq!(protection.connected_validator_count(), 0);

        // Should be able to reconnect from different peer now
        let result = protection.check_hotkey_connection(hotkey, "peer2", None);
        assert!(result.is_allowed());
    }

    #[test]
    fn test_multiple_validators() {
        let protection = NetworkProtection::with_defaults();

        // Connect 3 different validators
        assert!(protection
            .check_hotkey_connection("hotkey1_abcdef1234", "peer1", None)
            .is_allowed());
        assert!(protection
            .check_hotkey_connection("hotkey2_abcdef5678", "peer2", None)
            .is_allowed());
        assert!(protection
            .check_hotkey_connection("hotkey3_abcdef9012", "peer3", None)
            .is_allowed());

        assert_eq!(protection.connected_validator_count(), 3);

        // Reconnect hotkey1 from new peer (simulates validator restart)
        // Should disconnect peer1 and accept peer4
        assert!(protection
            .check_hotkey_connection("hotkey1_abcdef1234", "peer4", None)
            .is_allowed());
        let v = protection
            .get_connected_validator("hotkey1_abcdef1234")
            .unwrap();
        assert_eq!(v.peer_id, "peer4");
        // Still 3 validators
        assert_eq!(protection.connected_validator_count(), 3);

        // Disconnect one
        protection.disconnect_hotkey("peer2");
        assert_eq!(protection.connected_validator_count(), 2);

        // hotkey2 can connect from different peer
        assert!(protection
            .check_hotkey_connection("hotkey2_abcdef5678", "peer5", None)
            .is_allowed());
        assert_eq!(protection.connected_validator_count(), 3);
    }

    // ==================== STAKE COUNTING TESTS ====================

    #[test]
    fn test_stake_counting_once() {
        let protection = NetworkProtection::with_defaults();
        let hotkey = "abcd1234567890abcdef";
        let stake = 1_000_000_000_000u64; // 1000 TAO

        // First registration should succeed
        assert!(protection.register_stake_for_consensus(hotkey, stake));
        assert!(protection.is_stake_counted(hotkey));
        assert_eq!(protection.get_total_counted_stake(), stake);

        // Second registration for same hotkey should fail (no double counting)
        assert!(!protection.register_stake_for_consensus(hotkey, stake));
        // Total stake should still be the same (not doubled)
        assert_eq!(protection.get_total_counted_stake(), stake);
    }

    #[test]
    fn test_stake_counting_multiple_validators() {
        let protection = NetworkProtection::with_defaults();

        let stake1 = 1_000_000_000_000u64; // 1000 TAO
        let stake2 = 2_000_000_000_000u64; // 2000 TAO
        let stake3 = 500_000_000_000u64; // 500 TAO

        assert!(protection.register_stake_for_consensus("hotkey1_abc", stake1));
        assert!(protection.register_stake_for_consensus("hotkey2_def", stake2));
        assert!(protection.register_stake_for_consensus("hotkey3_ghi", stake3));

        // Total should be sum of all unique stakes
        assert_eq!(
            protection.get_total_counted_stake(),
            stake1 + stake2 + stake3
        );

        // Try to double-count - should fail
        assert!(!protection.register_stake_for_consensus("hotkey1_abc", stake1));
        assert!(!protection.register_stake_for_consensus("hotkey2_def", stake2));

        // Total should still be the same
        assert_eq!(
            protection.get_total_counted_stake(),
            stake1 + stake2 + stake3
        );
    }

    #[test]
    fn test_stake_counting_reset() {
        let protection = NetworkProtection::with_defaults();

        protection.register_stake_for_consensus("hotkey1_abc", 1_000_000_000_000);
        protection.register_stake_for_consensus("hotkey2_def", 2_000_000_000_000);
        assert_eq!(protection.get_total_counted_stake(), 3_000_000_000_000);

        // Reset for new round
        protection.reset_stake_counting();
        assert_eq!(protection.get_total_counted_stake(), 0);
        assert!(!protection.is_stake_counted("hotkey1_abc"));
        assert!(!protection.is_stake_counted("hotkey2_def"));

        // Can register again after reset
        assert!(protection.register_stake_for_consensus("hotkey1_abc", 1_000_000_000_000));
    }

    #[test]
    fn test_get_connected_hotkeys() {
        let protection = NetworkProtection::with_defaults();

        protection.check_hotkey_connection("hotkey1_abc123456789", "peer1", None);
        protection.check_hotkey_connection("hotkey2_def123456789", "peer2", None);

        let hotkeys = protection.get_connected_hotkeys();
        assert_eq!(hotkeys.len(), 2);
        assert!(hotkeys.contains(&"hotkey1_abc123456789".to_string()));
        assert!(hotkeys.contains(&"hotkey2_def123456789".to_string()));
    }
}
