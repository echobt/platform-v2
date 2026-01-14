//! WebSocket event types and broadcasting

use crate::models::WsEvent;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;
use uuid::Uuid;

pub type EventSender = broadcast::Sender<WsEvent>;
pub type EventReceiver = broadcast::Receiver<WsEvent>;

#[derive(Clone)]
pub struct WsConnection {
    pub id: Uuid,
    pub hotkey: Option<String>,
}

pub struct EventBroadcaster {
    sender: EventSender,
    connections: Arc<RwLock<HashMap<Uuid, WsConnection>>>,
}

impl EventBroadcaster {
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.sender.subscribe()
    }

    pub fn broadcast(&self, event: WsEvent) {
        let _ = self.sender.send(event);
    }

    pub fn add_connection(&self, conn: WsConnection) {
        self.connections.write().insert(conn.id, conn);
    }

    pub fn remove_connection(&self, id: &Uuid) {
        self.connections.write().remove(id);
    }

    pub fn connection_count(&self) -> usize {
        self.connections.read().len()
    }

    pub fn get_connections(&self) -> Vec<WsConnection> {
        self.connections.read().values().cloned().collect()
    }
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new(1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{SubmissionEvent, WsEvent};

    // =========================================================================
    // WsConnection tests
    // =========================================================================

    #[test]
    fn test_ws_connection_clone() {
        let conn = WsConnection {
            id: Uuid::new_v4(),
            hotkey: Some("5GrwvaEF...".to_string()),
        };
        let cloned = conn.clone();

        assert_eq!(conn.id, cloned.id);
        assert_eq!(conn.hotkey, cloned.hotkey);
    }

    #[test]
    fn test_ws_connection_with_none_hotkey() {
        let conn = WsConnection {
            id: Uuid::new_v4(),
            hotkey: None,
        };

        assert!(conn.hotkey.is_none());
    }

    // =========================================================================
    // EventBroadcaster::new tests
    // =========================================================================

    #[test]
    fn test_event_broadcaster_new() {
        let broadcaster = EventBroadcaster::new(100);
        assert_eq!(broadcaster.connection_count(), 0);
    }

    #[test]
    fn test_event_broadcaster_default() {
        let broadcaster = EventBroadcaster::default();
        assert_eq!(broadcaster.connection_count(), 0);
    }

    #[test]
    #[should_panic(expected = "broadcast channel capacity cannot be zero")]
    fn test_event_broadcaster_new_with_zero_capacity_panics() {
        // Zero capacity should panic
        let _broadcaster = EventBroadcaster::new(0);
    }

    #[test]
    fn test_event_broadcaster_new_with_small_capacity() {
        // Small but valid capacity should work
        let broadcaster = EventBroadcaster::new(1);
        assert_eq!(broadcaster.connection_count(), 0);
    }

    // =========================================================================
    // Connection management tests
    // =========================================================================

    #[test]
    fn test_add_connection() {
        let broadcaster = EventBroadcaster::new(100);
        let conn = WsConnection {
            id: Uuid::new_v4(),
            hotkey: Some("validator1".to_string()),
        };

        broadcaster.add_connection(conn);
        assert_eq!(broadcaster.connection_count(), 1);
    }

    #[test]
    fn test_add_multiple_connections() {
        let broadcaster = EventBroadcaster::new(100);

        for i in 0..5 {
            let conn = WsConnection {
                id: Uuid::new_v4(),
                hotkey: Some(format!("validator{}", i)),
            };
            broadcaster.add_connection(conn);
        }

        assert_eq!(broadcaster.connection_count(), 5);
    }

    #[test]
    fn test_remove_connection() {
        let broadcaster = EventBroadcaster::new(100);
        let conn_id = Uuid::new_v4();
        let conn = WsConnection {
            id: conn_id,
            hotkey: Some("validator1".to_string()),
        };

        broadcaster.add_connection(conn);
        assert_eq!(broadcaster.connection_count(), 1);

        broadcaster.remove_connection(&conn_id);
        assert_eq!(broadcaster.connection_count(), 0);
    }

    #[test]
    fn test_remove_nonexistent_connection() {
        let broadcaster = EventBroadcaster::new(100);
        let conn = WsConnection {
            id: Uuid::new_v4(),
            hotkey: Some("validator1".to_string()),
        };

        broadcaster.add_connection(conn);

        // Try to remove a different connection
        let nonexistent_id = Uuid::new_v4();
        broadcaster.remove_connection(&nonexistent_id);

        // Original should still be there
        assert_eq!(broadcaster.connection_count(), 1);
    }

    #[test]
    fn test_get_connections() {
        let broadcaster = EventBroadcaster::new(100);
        let conn1 = WsConnection {
            id: Uuid::new_v4(),
            hotkey: Some("validator1".to_string()),
        };
        let conn2 = WsConnection {
            id: Uuid::new_v4(),
            hotkey: Some("validator2".to_string()),
        };

        broadcaster.add_connection(conn1.clone());
        broadcaster.add_connection(conn2.clone());

        let connections = broadcaster.get_connections();
        assert_eq!(connections.len(), 2);

        let hotkeys: Vec<_> = connections
            .iter()
            .filter_map(|c| c.hotkey.as_ref())
            .collect();
        assert!(hotkeys.contains(&&"validator1".to_string()));
        assert!(hotkeys.contains(&&"validator2".to_string()));
    }

    #[test]
    fn test_get_connections_empty() {
        let broadcaster = EventBroadcaster::new(100);
        let connections = broadcaster.get_connections();
        assert!(connections.is_empty());
    }

    #[test]
    fn test_connection_count() {
        let broadcaster = EventBroadcaster::new(100);
        assert_eq!(broadcaster.connection_count(), 0);

        let conn = WsConnection {
            id: Uuid::new_v4(),
            hotkey: None,
        };
        broadcaster.add_connection(conn.clone());
        assert_eq!(broadcaster.connection_count(), 1);

        broadcaster.remove_connection(&conn.id);
        assert_eq!(broadcaster.connection_count(), 0);
    }

    // =========================================================================
    // Subscribe tests
    // =========================================================================

    #[test]
    fn test_subscribe() {
        let broadcaster = EventBroadcaster::new(100);
        let _receiver = broadcaster.subscribe();
        // Just verify it doesn't panic
    }

    #[test]
    fn test_multiple_subscribers() {
        let broadcaster = EventBroadcaster::new(100);
        let _rx1 = broadcaster.subscribe();
        let _rx2 = broadcaster.subscribe();
        let _rx3 = broadcaster.subscribe();
        // All should work without issues
    }

    // =========================================================================
    // Broadcast tests
    // =========================================================================

    #[test]
    fn test_broadcast_with_subscriber() {
        let broadcaster = EventBroadcaster::new(100);
        let mut rx = broadcaster.subscribe();

        let event = WsEvent::SubmissionReceived(SubmissionEvent {
            submission_id: "sub1".to_string(),
            agent_hash: "hash1".to_string(),
            miner_hotkey: "miner1".to_string(),
            name: Some("Test Agent".to_string()),
            epoch: 1,
        });

        broadcaster.broadcast(event.clone());

        // Receiver should get the event
        let received = rx.try_recv();
        assert!(received.is_ok());
    }

    #[test]
    fn test_broadcast_without_subscribers() {
        let broadcaster = EventBroadcaster::new(100);

        // Should not panic even with no subscribers
        let event = WsEvent::Ping;
        broadcaster.broadcast(event);
    }

    #[test]
    fn test_broadcast_multiple_events() {
        let broadcaster = EventBroadcaster::new(100);
        let mut rx = broadcaster.subscribe();

        broadcaster.broadcast(WsEvent::Ping);
        broadcaster.broadcast(WsEvent::Pong);

        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
    }

    // =========================================================================
    // Concurrent access tests
    // =========================================================================

    #[test]
    fn test_concurrent_add_remove() {
        use std::thread;

        let broadcaster = std::sync::Arc::new(EventBroadcaster::new(100));
        let mut handles = vec![];

        // Spawn threads that add connections
        for i in 0..10 {
            let bc = broadcaster.clone();
            handles.push(thread::spawn(move || {
                let conn = WsConnection {
                    id: Uuid::new_v4(),
                    hotkey: Some(format!("validator{}", i)),
                };
                bc.add_connection(conn);
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(broadcaster.connection_count(), 10);
    }

    #[test]
    fn test_duplicate_connection_id() {
        let broadcaster = EventBroadcaster::new(100);
        let conn_id = Uuid::new_v4();

        let conn1 = WsConnection {
            id: conn_id,
            hotkey: Some("validator1".to_string()),
        };
        let conn2 = WsConnection {
            id: conn_id,
            hotkey: Some("validator2".to_string()),
        };

        broadcaster.add_connection(conn1);
        broadcaster.add_connection(conn2);

        // HashMap should overwrite, so count is still 1
        assert_eq!(broadcaster.connection_count(), 1);

        // The second connection should have overwritten the first
        let connections = broadcaster.get_connections();
        assert_eq!(connections[0].hotkey, Some("validator2".to_string()));
    }
}
