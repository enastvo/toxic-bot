use async_trait::async_trait;
use std::sync::{Arc, Mutex};

#[async_trait]
pub trait SignalTransport: Send + Sync {
    async fn send(&self, room_id: &str, is_group: bool, text: &str) -> anyhow::Result<()>;
}

#[derive(Clone, Default)]
pub struct MockSignal { pub sent: Arc<Mutex<Vec<(String, String)>>> }
impl MockSignal { pub fn new() -> Self { Self::default() } }

#[async_trait]
impl SignalTransport for MockSignal {
    async fn send(&self, room_id: &str, _is_group: bool, text: &str) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push((room_id.to_string(), text.to_string()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_records_sends() {
        let m = MockSignal::new();
        m.send("g1", true, "hello").await.unwrap();
        assert_eq!(m.sent.lock().unwrap().clone(), vec![("g1".to_string(), "hello".to_string())]);
    }
}
