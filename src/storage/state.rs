use crate::domain::{position::Position, trade::{Fill, OrderRecord, OrderState}, wallet::WalletStats};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::{fs, path::Path, sync::Mutex};
use thiserror::Error;

#[derive(Debug, Error)] pub enum StorageError { #[error("sqlite: {0}")] Sql(#[from] rusqlite::Error), #[error("serialization: {0}")] Json(#[from] serde_json::Error), #[error("filesystem: {0}")] Io(#[from] std::io::Error), #[error("invalid order accounting: {0}")] InvalidOrder(String), #[error("storage mutex poisoned")] Poisoned }
pub struct StateStore { conn: Mutex<Connection> }
impl StateStore {
    pub fn open(path: impl AsRef<Path>)->Result<Self,StorageError>{let path=path.as_ref();if path != Path::new(":memory:"){if let Some(parent)=path.parent(){fs::create_dir_all(parent)?;}}let c=Connection::open(path)?;c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS orders (id TEXT PRIMARY KEY, idempotency_key TEXT UNIQUE NOT NULL, state TEXT NOT NULL, payload TEXT NOT NULL, updated_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS fills (order_id TEXT PRIMARY KEY, payload TEXT NOT NULL, created_at TEXT NOT NULL); CREATE TABLE IF NOT EXISTS observations (id INTEGER PRIMARY KEY, kind TEXT NOT NULL, observed_at TEXT NOT NULL, received_at TEXT NOT NULL, payload TEXT NOT NULL);")?;for (name, ty) in [("order_kind", "TEXT"), ("position_id", "TEXT"), ("side", "TEXT")] { let exists=c.prepare("PRAGMA table_info(orders)")?.query_map([],|r|r.get::<_,String>(1))?.collect::<Result<Vec<_>,_>>()?.iter().any(|n|n==name);if !exists{c.execute(&format!("ALTER TABLE orders ADD COLUMN {name} {ty}"),[])?;}}Ok(Self{conn:Mutex::new(c)})}
    fn conn(&self)->Result<std::sync::MutexGuard<'_,Connection>,StorageError>{self.conn.lock().map_err(|_|StorageError::Poisoned)}
    pub fn save_wallet(&self,w:&WalletStats)->Result<(),StorageError>{self.put(&format!("wallet:{}",w.wallet),w)} pub fn save_position(&self,p:&Position)->Result<(),StorageError>{self.put(&format!("position:{}",p.mint),p)}
    pub fn positions(&self)->Result<Vec<Position>,StorageError>{let c=self.conn()?;let mut st=c.prepare("SELECT value FROM kv WHERE key LIKE 'position:%'")?;st.query_map([],|r|r.get::<_,String>(0))?.map(|v|serde_json::from_str(&v?).map_err(StorageError::from)).collect()}
    pub fn save_fill(&self,fill:&Fill)->Result<(),StorageError>{let c=self.conn()?;c.execute("INSERT INTO fills(order_id,payload,created_at) VALUES(?1,?2,?3) ON CONFLICT(order_id) DO UPDATE SET payload=excluded.payload",params![fill.order_id,serde_json::to_string(fill)?,Utc::now().to_rfc3339()])?;Ok(())}
    pub fn record_observation<T:serde::Serialize>(&self,kind:&str,observed_at:chrono::DateTime<Utc>,received_at:chrono::DateTime<Utc>,value:&T)->Result<(),StorageError>{if observed_at>received_at{return Err(StorageError::Sql(rusqlite::Error::InvalidQuery));}let c=self.conn()?;c.execute("INSERT INTO observations(kind,observed_at,received_at,payload) VALUES(?1,?2,?3,?4)",params![kind,observed_at.to_rfc3339(),received_at.to_rfc3339(),serde_json::to_string(value)?])?;Ok(())}
    pub fn put<T:serde::Serialize>(&self,key:&str,value:&T)->Result<(),StorageError>{let c=self.conn()?;c.execute("INSERT INTO kv(key,value,updated_at) VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![key,serde_json::to_string(value)?,Utc::now().to_rfc3339()])?;Ok(())}
    pub fn get<T:serde::de::DeserializeOwned>(&self,key:&str)->Result<Option<T>,StorageError>{let c=self.conn()?;let raw:Option<String>=c.query_row("SELECT value FROM kv WHERE key=?1",params![key],|r|r.get(0)).optional()?;raw.map(|v|serde_json::from_str(&v)).transpose().map_err(Into::into)}
    pub fn reserve_order(&self,o:&OrderRecord)->Result<bool,StorageError>{if matches!(o.kind,crate::domain::trade::OrderKind::Entry|crate::domain::trade::OrderKind::Exit)&&(o.position_id.is_none()||o.input_mint.is_none()||o.output_mint.is_none()||o.input_amount_atomic.is_none()){return Err(StorageError::InvalidOrder("new entry and exit orders require position, route mints, and atomic input amount".into()))}if matches!(o.kind,crate::domain::trade::OrderKind::Entry)&&!matches!(o.side,crate::domain::trade::OrderSide::Buy){return Err(StorageError::InvalidOrder("entry order must be buy side".into()))}if matches!(o.kind,crate::domain::trade::OrderKind::Exit)&&!matches!(o.side,crate::domain::trade::OrderSide::Sell){return Err(StorageError::InvalidOrder("exit order must be sell side".into()))}let c=self.conn()?;let changed=c.execute("INSERT OR IGNORE INTO orders(id,idempotency_key,state,payload,order_kind,position_id,side,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![o.id,o.idempotency_key,format!("{:?}",o.state),serde_json::to_string(o)?,format!("{:?}",o.kind),o.position_id,format!("{:?}",o.side),Utc::now().to_rfc3339()])?;Ok(changed==1)}
    pub fn update_order(&self,o:&OrderRecord)->Result<(),StorageError>{let c=self.conn()?;c.execute("UPDATE orders SET state=?2,payload=?3,order_kind=?4,position_id=?5,side=?6,updated_at=?7 WHERE id=?1",params![o.id,format!("{:?}",o.state),serde_json::to_string(o)?,format!("{:?}",o.kind),o.position_id,format!("{:?}",o.side),Utc::now().to_rfc3339()])?;Ok(())}
    pub fn incomplete_orders(&self)->Result<Vec<OrderRecord>,StorageError>{let c=self.conn()?;let mut st=c.prepare("SELECT payload FROM orders WHERE state IN ('Pending','Submitted','Unknown')")?;st.query_map([],|r|r.get::<_,String>(0))?.map(|x|serde_json::from_str(&x?).map_err(StorageError::from)).collect()}
    pub fn orders(&self)->Result<Vec<OrderRecord>,StorageError>{let c=self.conn()?;let mut st=c.prepare("SELECT payload FROM orders ORDER BY updated_at")?;st.query_map([],|r|r.get::<_,String>(0))?.map(|x|serde_json::from_str(&x?).map_err(StorageError::from)).collect()}
    pub fn fill_for_order(&self,order_id:&str)->Result<Option<Fill>,StorageError>{let c=self.conn()?;let raw:Option<String>=c.query_row("SELECT payload FROM fills WHERE order_id=?1",params![order_id],|r|r.get(0)).optional()?;raw.map(|v|serde_json::from_str(&v)).transpose().map_err(Into::into)}
    /// Persistent emergency stop: when latched, no new trades are allowed
    /// anywhere in the system, but explicit manual exits remain possible.
    pub fn set_emergency_stop(&self,reason:&str)->Result<(),StorageError>{self.put("risk:emergency_stop",&serde_json::json!({"reason":reason,"at":Utc::now().to_rfc3339()}))}
    pub fn clear_emergency_stop(&self)->Result<(),StorageError>{let c=self.conn()?;c.execute("DELETE FROM kv WHERE key='risk:emergency_stop'",[])?;Ok(())}
    pub fn emergency_stop(&self)->Result<Option<String>,StorageError>{let c=self.conn()?;let raw:Option<String>=c.query_row("SELECT value FROM kv WHERE key='risk:emergency_stop'",[],|r|r.get(0)).optional()?;raw.map(|v|serde_json::from_str::<serde_json::Value>(&v).map(|x|x["reason"].as_str().unwrap_or("unspecified").to_string())).transpose().map_err(Into::into)}
    pub fn latch_kill_switch(&self,reason:&str)->Result<(),StorageError>{self.put("risk:kill_switch",&reason)} pub fn kill_switch_reason(&self)->Result<Option<String>,StorageError>{self.get("risk:kill_switch")}
}
#[cfg(test)] mod tests {
    use super::*;
    use crate::{domain::{position::Position, trade::{OrderKind, OrderSide}}, economics::{BreakEvenInputs, CostModel}};
    use rust_decimal_macros::dec;

    fn order(kind: OrderKind, side: OrderSide, position_id: Option<&str>) -> OrderRecord { OrderRecord { id: uuid::Uuid::new_v4().to_string(), signal_id: "signal".into(), mint: "token".into(), kind, position_id: position_id.map(str::to_owned), side, input_mint: Some("base".into()), output_mint: Some("token".into()), input_amount_atomic: Some(1_000_000), input_value_usd: Some(dec!(10)), output_mint_decimals: Some(6), state: OrderState::Pending, idempotency_key: uuid::Uuid::new_v4().to_string(), created_at: Utc::now(), signature: None, error: None } }
    fn position() -> Position { Position { mint: "token".into(), position_id: Some("position-1".into()), token_mint: Some("token".into()), base_mint: Some("base".into()), entry_input_amount_atomic: Some(1_000_000), entry_output_amount_atomic: Some(2_000_000), token_decimals: Some(6), base_mint_decimals: Some(9), entry_fees_usd: Some(dec!(0.02)), entry_slippage_bps: Some(75), entry_cost_model: Some(CostModel { observed_at: Utc::now(), source: "test".into(), is_live_snapshot: false, input: BreakEvenInputs { position_size_usd: dec!(1), avg_priority_fee_usd: dec!(0), avg_swap_fee_bps: dec!(1), avg_slippage_bps: dec!(1), avg_price_impact_bps: dec!(1), failed_tx_rate: dec!(0), avg_failed_tx_cost_usd: dec!(0), assumed_win_loss_ratio: dec!(1), assumed_avg_loss_pct: dec!(1) } }), quantity: dec!(2000000), remaining_quantity_atomic: Some(2_000_000), entry_cost_usd: Some(dec!(10)), base_entry_price_usd: Some(dec!(1)), state: crate::domain::position::PositionState::Open, reconciliation_status: crate::domain::position::ReconciliationStatus::Reconciled, last_reconciled_at: None, exit_signature: None, exit_fees_usd: None, exit_time: None, entry_price_usd: dec!(1), entry_time: Utc::now(), entry_signature: "sig".into(), high_water_price_usd: dec!(1), realized_pnl_usd: dec!(0), unrealized_pnl_usd: dec!(0), fees_usd: dec!(0.02), current_value_usd: dec!(1), signal_id: "signal".into(), exit_reason: None } }
    #[test] fn duplicate_order_is_refused_after_restart() { let s=StateStore::open(":memory:").unwrap(); let o=order(OrderKind::Entry, OrderSide::Buy, Some("position-1"));assert!(s.reserve_order(&o).unwrap());assert!(!s.reserve_order(&o).unwrap()); }
    #[test] fn entry_and_exit_side_mismatch_is_rejected() { let s=StateStore::open(":memory:").unwrap();assert!(matches!(s.reserve_order(&order(OrderKind::Entry,OrderSide::Sell,Some("position-1"))),Err(StorageError::InvalidOrder(_))));assert!(matches!(s.reserve_order(&order(OrderKind::Exit,OrderSide::Buy,Some("position-1"))),Err(StorageError::InvalidOrder(_)))); }
    #[test] fn position_accounting_round_trips_without_unit_assumptions() { let s=StateStore::open(":memory:").unwrap(); let p=position();s.save_position(&p).unwrap();let loaded=s.positions().unwrap().pop().unwrap();assert_eq!(loaded.position_id.as_deref(),Some("position-1"));assert_eq!(loaded.entry_input_amount_atomic,Some(1_000_000));assert_eq!(loaded.entry_output_amount_atomic,Some(2_000_000));assert_eq!(loaded.token_decimals,Some(6));assert_eq!(loaded.base_mint_decimals,Some(9));assert_eq!(loaded.token_mint.as_deref(),Some("token"));assert_eq!(loaded.base_mint.as_deref(),Some("base"));assert!(loaded.entry_cost_model.is_some()); }
    #[test] fn exit_linkage_survives_restart_and_is_distinct_from_entry() { let path=std::env::temp_dir().join(format!("solana-bot-state-{}.sqlite",uuid::Uuid::new_v4()));let entry=order(OrderKind::Entry,OrderSide::Buy,Some("position-1"));let exit=order(OrderKind::Exit,OrderSide::Sell,Some("position-1"));{let s=StateStore::open(&path).unwrap();assert!(s.reserve_order(&entry).unwrap());assert!(s.reserve_order(&exit).unwrap());}let s=StateStore::open(&path).unwrap();let incomplete=s.incomplete_orders().unwrap();let reloaded_exit=incomplete.iter().find(|o|o.id==exit.id).unwrap();assert_eq!(reloaded_exit.kind,OrderKind::Exit);assert_eq!(reloaded_exit.side,OrderSide::Sell);assert_eq!(reloaded_exit.position_id.as_deref(),Some("position-1"));assert_ne!(entry.kind,reloaded_exit.kind);assert_ne!(entry.side,reloaded_exit.side);drop(s);let _=std::fs::remove_file(&path);let _=std::fs::remove_file(path.with_extension("sqlite-wal"));let _=std::fs::remove_file(path.with_extension("sqlite-shm")); }
    #[test] fn legacy_position_remains_explicitly_incomplete() { let s=StateStore::open(":memory:").unwrap();let legacy=serde_json::json!({"mint":"legacy","quantity":"1","entry_price_usd":"1","entry_time":Utc::now(),"entry_signature":"sig","high_water_price_usd":"1","realized_pnl_usd":"0","unrealized_pnl_usd":"0","fees_usd":"0","current_value_usd":"1","signal_id":"signal","exit_reason":null});let c=s.conn().unwrap();c.execute("INSERT INTO kv(key,value,updated_at) VALUES(?1,?2,?3)",params!["position:legacy",legacy.to_string(),Utc::now().to_rfc3339()]).unwrap();drop(c);let loaded=s.positions().unwrap().pop().unwrap();assert!(loaded.position_id.is_none());assert!(loaded.entry_input_amount_atomic.is_none());assert!(loaded.token_decimals.is_none());assert!(loaded.entry_cost_model.is_none()); }
    #[test] fn legacy_incomplete_order_is_migrated_as_unknown_not_entry_or_exit() { let path=std::env::temp_dir().join(format!("solana-bot-legacy-{}.sqlite",uuid::Uuid::new_v4()));let legacy=serde_json::json!({"id":"legacy-order","signal_id":"signal","mint":"token","state":"Unknown","idempotency_key":"legacy-key","created_at":Utc::now(),"signature":null,"error":null});{let c=Connection::open(&path).unwrap();c.execute_batch("CREATE TABLE orders (id TEXT PRIMARY KEY, idempotency_key TEXT UNIQUE NOT NULL, state TEXT NOT NULL, payload TEXT NOT NULL, updated_at TEXT NOT NULL);").unwrap();c.execute("INSERT INTO orders(id,idempotency_key,state,payload,updated_at) VALUES(?1,?2,?3,?4,?5)",params!["legacy-order","legacy-key","Unknown",legacy.to_string(),Utc::now().to_rfc3339()]).unwrap();}let s=StateStore::open(&path).unwrap();let order=s.incomplete_orders().unwrap().pop().unwrap();assert_eq!(order.kind,OrderKind::LegacyUnknown);assert_eq!(order.side,OrderSide::Unknown);assert!(order.position_id.is_none());drop(s);let _=std::fs::remove_file(&path);let _=std::fs::remove_file(path.with_extension("sqlite-wal"));let _=std::fs::remove_file(path.with_extension("sqlite-shm")); }
    #[test]
    fn emergency_stop_survives_restart_and_clears() {
        let path = std::env::temp_dir().join(format!("solana-bot-estop-{}.sqlite", uuid::Uuid::new_v4()));
        {
            let s = StateStore::open(&path).unwrap();
            assert!(s.emergency_stop().unwrap().is_none());
            s.set_emergency_stop("operator halt").unwrap();
        }
        let s = StateStore::open(&path).unwrap();
        assert_eq!(s.emergency_stop().unwrap().as_deref(), Some("operator halt"));
        s.clear_emergency_stop().unwrap();
        assert!(s.emergency_stop().unwrap().is_none());
        drop(s);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("sqlite-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite-shm"));
    }
    #[test]
    fn order_and_fill_round_trip_preserves_new_accounting_fields() {
        let s = StateStore::open(":memory:").unwrap();
        let mut o = order(OrderKind::Entry, OrderSide::Buy, Some("p1"));
        o.state = OrderState::Submitted;
        o.signature = Some("sig-1".into());
        assert!(s.reserve_order(&o).unwrap());
        o.transition(OrderState::Confirmed).unwrap();
        s.update_order(&o).unwrap();
        let fill = Fill { order_id: o.id.clone(), signature: "sig-1".into(), input_amount: 1_000_000, output_amount: 2_000_000, price_usd: dec!(0.5), fees_usd: dec!(0.01), slippage_bps: 40, confirmed_at: Utc::now(), latency_ms: 120, fee_lamports: 9_000, input_value_usd: Some(dec!(10)), expected_output_amount: Some(2_100_000) };
        s.save_fill(&fill).unwrap();
        let loaded = s.fill_for_order(&o.id).unwrap().unwrap();
        assert_eq!(loaded.fee_lamports, 9_000);
        assert_eq!(loaded.input_value_usd, Some(dec!(10)));
        assert_eq!(loaded.expected_output_amount, Some(2_100_000));
        let orders = s.orders().unwrap();
        let stored = orders.iter().find(|x| x.id == o.id).unwrap();
        assert_eq!(stored.state, OrderState::Confirmed);
        assert_eq!(stored.input_value_usd, Some(dec!(10)));
        assert_eq!(stored.output_mint_decimals, Some(6));
        assert!(s.fill_for_order("missing").unwrap().is_none());
    }
    #[test]
    fn incomplete_orders_excludes_terminal_states() {
        let s = StateStore::open(":memory:").unwrap();
        let mut o = order(OrderKind::Entry, OrderSide::Buy, Some("p1"));
        o.state = OrderState::Expired;
        assert!(s.reserve_order(&o).unwrap());
        assert!(s.incomplete_orders().unwrap().is_empty());
        assert_eq!(s.orders().unwrap().len(), 1);
    }
}
