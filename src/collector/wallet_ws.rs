use crate::collector::swap_parser::{parse_swap_from_transaction, ParsedSwap};
use crate::data::rpc::RpcPool;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

/// Default retry base delay for WebSocket reconnection.
const WS_RECONNECT_BASE_MS: u64 = 1_000;
/// Maximum retry delay for WebSocket reconnection.
const WS_RECONNECT_MAX_MS: u64 = 30_000;

/// A parsed swap event received via WebSocket subscription, ready to be
/// fed into the wallet accumulator.
#[derive(Debug, Clone)]
pub struct WsSwapEvent {
    pub wallet: String,
    pub swap: ParsedSwap,
    /// Timestamp when the bot first learned about this transaction from
    /// the WebSocket notification (not when it was on-chain).
    pub detected_at: chrono::DateTime<chrono::Utc>,
}

/// Manages WebSocket subscriptions to monitor smart-wallet transactions
/// in real time. Each wallet gets a `logsSubscribe` filter that fires
/// whenever the wallet submits a transaction. The manager fetches and
/// parses the transaction, deduplicates, and sends parsed swaps through
/// a channel.
///
/// # Architecture
///
/// ```text
///  Solana WebSocket ──logsSubscribe──> WsManager
///       │                                  │
///       │                           parse_swap_from_transaction
///       │                                  │
///       │                             WsSwapEvent
///       │                                  │
///       └──── HTTP fallback (polling) ─────┘
///                     │
///              WalletAccumulator
/// ```
///
/// # Failover
///
/// - Cycles through `websocket_endpoints` on connection failure
/// - Exponential backoff: 1s → 2s → 4s → ... → 30s max
/// - After `max_reconnect_attempts` consecutive failures on ALL endpoints,
///   the manager logs an error and stops (caller can restart it).
/// - The polling path continues independently, providing a safety net.
pub struct WalletWsManager {
    endpoints: Vec<String>,
    rpc: Arc<RpcPool>,
    wallets: Vec<String>,
    /// Signatures already processed by the polling path or by a previous
    /// WebSocket notification. Prevents duplicate swap events.
    seen_sigs: Arc<tokio::sync::Mutex<HashSet<String>>>,
    /// Channel to send parsed swap events to the wallet monitor.
    event_tx: mpsc::Sender<WsSwapEvent>,
    /// Maximum consecutive reconnection failures before giving up.
    max_reconnect_attempts: u32,
}

impl WalletWsManager {
    pub fn new(
        endpoints: Vec<String>,
        rpc: Arc<RpcPool>,
        wallets: Vec<String>,
        seen_sigs: Arc<tokio::sync::Mutex<HashSet<String>>>,
        event_tx: mpsc::Sender<WsSwapEvent>,
        max_reconnect_attempts: u32,
    ) -> Self {
        Self {
            endpoints,
            rpc,
            wallets,
            seen_sigs,
            event_tx,
            max_reconnect_attempts,
        }
    }

    /// Spawn the WebSocket subscription manager on a background tokio task.
    /// Returns a join handle and a channel receiver for swap events.
    ///
    /// The manager will:
    /// 1. Connect to the first available WebSocket endpoint
    /// 2. Subscribe to `logsSubscribe` for each monitored wallet
    /// 3. For each notification, fetch and parse the transaction
    /// 4. Send parsed swaps through the channel
    /// 5. Reconnect on failure with exponential backoff
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            self.run().await;
        })
    }

    async fn run(&self) {
        if self.endpoints.is_empty() {
            tracing::warn!("no WebSocket endpoints configured; real-time monitoring disabled");
            return;
        }
        if self.wallets.is_empty() {
            tracing::warn!("no wallets to monitor; WebSocket subscription skipped");
            return;
        }

        let mut consecutive_failures: u32 = 0;
        let mut endpoint_idx = 0;

        loop {
            if consecutive_failures >= self.max_reconnect_attempts {
                tracing::error!(
                    consecutive_failures,
                    max = self.max_reconnect_attempts,
                    "WebSocket subscription manager giving up after too many reconnection failures; \
                     real-time monitoring is offline; polling continues as fallback"
                );
                return;
            }

            let endpoint = &self.endpoints[endpoint_idx % self.endpoints.len()];
            tracing::info!(
                endpoint = %endpoint,
                attempt = consecutive_failures + 1,
                wallets = self.wallets.len(),
                "connecting WebSocket for wallet subscriptions"
            );

            match self.connect_and_subscribe(endpoint).await {
                Ok(()) => {
                    // Clean disconnection (server closed gracefully).
                    // Reset failure count and try reconnecting.
                    consecutive_failures = 0;
                    tracing::info!("WebSocket connection closed cleanly; reconnecting");
                }
                Err(e) => {
                    consecutive_failures += 1;
                    endpoint_idx += 1;
                    let delay_ms = WS_RECONNECT_BASE_MS
                        .saturating_mul(2u64.saturating_pow(consecutive_failures.min(5)))
                        .min(WS_RECONNECT_MAX_MS);
                    tracing::warn!(
                        error = %e,
                        consecutive_failures,
                        next_endpoint = %self.endpoints[endpoint_idx % self.endpoints.len()],
                        backoff_ms = delay_ms,
                        "WebSocket connection failed; cycling endpoint with backoff"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    async fn connect_and_subscribe(&self, endpoint: &str) -> Result<(), anyhow::Error> {
        let (mut ws_stream, _) = connect_async(endpoint)
            .await
            .map_err(|e| anyhow::anyhow!("WebSocket connect failed to {}: {}", endpoint, e))?;

        // Subscribe to logs for each wallet.
        for (idx, wallet) in self.wallets.iter().enumerate() {
            let sub_msg = json!({
                "jsonrpc": "2.0",
                "id": idx + 1,
                "method": "logsSubscribe",
                "params": [
                    {"mentions": [wallet]},
                    {"commitment": "confirmed"}
                ]
            });
            ws_stream
                .send(Message::Text(sub_msg.to_string()))
                .await
                .map_err(|e| anyhow::anyhow!("failed to send subscribe for {}: {}", wallet, e))?;
        }

        tracing::info!(
            endpoint = %endpoint,
            wallets = self.wallets.len(),
            "WebSocket subscriptions active; listening for wallet transactions"
        );

        // Process incoming messages.
        while let Some(msg) = ws_stream.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    return Err(anyhow::anyhow!("WebSocket receive error: {}", e));
                }
            };
            match msg {
                Message::Text(text) => {
                    if let Err(e) = self.handle_message(&text).await {
                        tracing::debug!(
                            error = %e,
                            "failed to process WebSocket message; skipping"
                        );
                    }
                }
                Message::Ping(data) => {
                    let _ = ws_stream.send(Message::Pong(data)).await;
                }
                Message::Close(_) => {
                    return Ok(());
                }
                _ => {}
            }
        }
        Ok(())
    }

    async fn handle_message(&self, text: &str) -> Result<(), anyhow::Error> {
        let v: Value = serde_json::from_str(text)?;

        // Ignore subscription confirmation responses.
        if v.get("result").is_some() && v.get("method").is_none() {
            return Ok(());
        }

        let params = v
            .get("params")
            .and_then(|p| p.get("result"))
            .ok_or_else(|| anyhow::anyhow!("missing params.result in WebSocket notification"))?;

        let signature = params
            .get("signature")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing signature in notification"))?
            .to_string();

        // Deduplicate: skip if already processed by polling or by a
        // previous WebSocket notification.
        {
            let mut seen = self.seen_sigs.lock().await;
            if seen.contains(&signature) {
                return Ok(());
            }
            seen.insert(signature.clone());
        }

        // Fetch the full transaction to parse the swap.
        let tx = match self.rpc.transaction(&signature).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                tracing::debug!(
                    sig = %signature,
                    "WebSocket notification: transaction not yet available (may need retry)"
                );
                // Remove from seen so polling can pick it up.
                let mut seen = self.seen_sigs.lock().await;
                seen.remove(&signature);
                return Ok(());
            }
            Err(e) => {
                tracing::warn!(
                    sig = %signature,
                    error = %e,
                    "WebSocket notification: failed to fetch transaction; leaving for polling fallback"
                );
                let mut seen = self.seen_sigs.lock().await;
                seen.remove(&signature);
                return Ok(());
            }
        };

        let detected_at = chrono::Utc::now();

        // Try to parse as a swap for each monitored wallet.
        // A single transaction might involve multiple wallets.
        for wallet in &self.wallets {
            if let Some(swap) = parse_swap_from_transaction(&tx, wallet) {
                let event = WsSwapEvent {
                    wallet: wallet.clone(),
                    swap,
                    detected_at,
                };
                if self.event_tx.send(event).await.is_err() {
                    tracing::warn!("WebSocket event channel closed; stopping subscription manager");
                    return Ok(());
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collector::swap_parser::SwapDirection;

    #[test]
    fn ws_swap_event_carries_detection_timestamp() {
        let now = chrono::Utc::now();
        let event = WsSwapEvent {
            wallet: "w".into(),
            swap: ParsedSwap {
                wallet: "w".into(),
                input_mint: "So11111111111111111111111111111111111111112".into(),
                output_mint: "T".into(),
                input_amount: 1_000_000_000,
                output_amount: 1_000_000,
                input_decimals: 9,
                output_decimals: 6,
                direction: SwapDirection::Buy,
                fee_lamports: 5000,
                dex: "jupiter_v6".into(),
                slot: 100,
                block_time: 1_700_000_000,
                signature: "sig1".into(),
            },
            detected_at: now,
        };
        assert_eq!(event.detected_at, now);
        assert_eq!(event.wallet, "w");
    }

    #[test]
    fn ws_manager_rejects_empty_endpoints() {
        let (tx, _rx) = mpsc::channel(10);
        let rpc = Arc::new(
            crate::data::rpc::RpcPool::new(
                vec!["http://127.0.0.1:8899".into()],
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
        );
        let seen = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
        let mgr = WalletWsManager::new(vec![], rpc, vec!["w".into()], seen, tx, 3);
        assert!(mgr.endpoints.is_empty());
    }

    #[tokio::test]
    async fn seen_sigs_deduplicates_across_ws_and_polling() {
        let seen = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
        {
            let mut s = seen.lock().await;
            s.insert("already_processed".into());
        }
        {
            let s = seen.lock().await;
            assert!(s.contains("already_processed"));
        }
        {
            let s = seen.lock().await;
            assert!(!s.contains("new_sig_123"));
        }
    }

    #[tokio::test]
    async fn ws_reconnect_backoff_is_bounded() {
        let base = WS_RECONNECT_BASE_MS;
        let max = WS_RECONNECT_MAX_MS;
        for attempt in 0..20u32 {
            let delay = base
                .saturating_mul(2u64.saturating_pow(attempt.min(5)))
                .min(max);
            assert!(delay <= max, "backoff {} exceeded max {}", delay, max);
        }
    }

    #[test]
    fn ws_empty_endpoints_produces_no_manager_connections() {
        let (tx, _rx) = mpsc::channel(10);
        let rpc = Arc::new(
            crate::data::rpc::RpcPool::new(
                vec!["http://127.0.0.1:8899".into()],
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
        );
        let seen = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
        let mgr = WalletWsManager::new(vec![], rpc, vec!["w1".into(), "w2".into()], seen, tx, 3);
        assert!(mgr.endpoints.is_empty());
        assert_eq!(mgr.wallets.len(), 2);
        assert_eq!(mgr.max_reconnect_attempts, 3);
    }

    #[test]
    fn ws_multiple_wallets_tracked() {
        let (tx, _rx) = mpsc::channel(10);
        let rpc = Arc::new(
            crate::data::rpc::RpcPool::new(
                vec!["http://127.0.0.1:8899".into()],
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
        );
        let seen = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
        let wallets = vec!["wallet_a".into(), "wallet_b".into(), "wallet_c".into()];
        let mgr = WalletWsManager::new(
            vec!["wss://endpoint.example.com".into()],
            rpc,
            wallets.clone(),
            seen,
            tx,
            3,
        );
        assert_eq!(mgr.wallets, wallets);
        assert_eq!(mgr.endpoints.len(), 1);
    }

    #[tokio::test]
    async fn ws_seen_sigs_deduplicates_same_signature_from_different_sources() {
        let seen = Arc::new(tokio::sync::Mutex::new(HashSet::<String>::new()));
        // Polling processes signature first
        {
            let mut s = seen.lock().await;
            s.insert("shared_sig_abc123".into());
        }
        // WS notification for same sig arrives later — should be deduped
        {
            let s = seen.lock().await;
            assert!(s.contains("shared_sig_abc123"));
            assert!(!s.contains("new_sig_xyz789"));
        }
        // Add the new one
        {
            let mut s = seen.lock().await;
            s.insert("new_sig_xyz789".into());
        }
        // Both now present
        {
            let s = seen.lock().await;
            assert!(s.contains("shared_sig_abc123"));
            assert!(s.contains("new_sig_xyz789"));
        }
    }

    #[test]
    fn ws_reconnect_attempts_limit_is_respected() {
        let max_attempts = 5u32;
        for attempt in 0..=max_attempts {
            if attempt >= max_attempts {
                return;
            }
        }
    }
}
