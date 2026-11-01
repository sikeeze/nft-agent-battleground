//! Engine trading: evaluasi rule terhadap data market (blueprint §F2).
//!
//! Engine **tidak memicu tx sendiri** — setiap keputusan dikembalikan
//! sebagai `TradeAction` yang wajib dilewatkan Policy Engine oleh
//! pemanggil ( aturan keras #2 ). Engine murni deterministik.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::pnl::Portfolio;
use crate::rules::{Rule, RuleKind, RulesConfig};

/// Data market satu koleksi ( ditarik adapter `nft-market` ).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionMarket {
    pub collection: Address,
    /// Floor price saat ini ( ETH ).
    pub floor_eth: f64,
    /// Listing termurah ( ETH ), bila ada.
    pub cheapest_listing_eth: Option<f64>,
    /// jumlah listing aktif.
    pub listings_count: u32,
}

/// Aksi yang diusulkan engine. Pemanggil wajib lewat Policy Engine sebelum
/// mengeksekusi ( aturan keras #2, §F5 ).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TradeAction {
    /// Beli listing termurah yang memenuhi rule.
    Buy {
        rule: String,
        collection: Address,
        /// Order id listing di marketplace.
        order_id: String,
        token_id: U256,
        /// Harga beli ( ETH ).
        price_eth: f64,
    },
    /// List token untuk dijual ( take-profit ).
    ListForSale {
        rule: String,
        collection: Address,
        token_id: U256,
        /// Harga list ( ETH ).
        price_eth: f64,
        duration_secs: u64,
    },
    /// Stop loss: list di bawah harga beli atau cancel listing lama.
    StopLoss {
        rule: String,
        collection: Address,
        token_id: U256,
        price_eth: f64,
    },
    /// Cancel listing basi.
    CancelStaleListing {
        rule: String,
        collection: Address,
        order_id: String,
    },
}

impl TradeAction {
    /// Nama rule yang memicu aksi.
    pub fn rule(&self) -> &str {
        match self {
            Self::Buy { rule, .. }
            | Self::ListForSale { rule, .. }
            | Self::StopLoss { rule, .. }
            | Self::CancelStaleListing { rule, .. } => rule,
        }
    }

    /// Koleksi target aksi.
    pub fn collection(&self) -> Address {
        match self {
            Self::Buy { collection, .. }
            | Self::ListForSale { collection, .. }
            | Self::StopLoss { collection, .. }
            | Self::CancelStaleListing { collection, .. } => *collection,
        }
    }

    /// Estimasi nilai aksi ( ETH ) untuk cek policy.
    pub fn value_eth(&self) -> Option<f64> {
        match self {
            Self::Buy { price_eth, .. } => Some(*price_eth),
            Self::ListForSale { price_eth, .. } => Some(*price_eth),
            Self::StopLoss { price_eth, .. } => Some(*price_eth),
            Self::CancelStaleListing { .. } => None,
        }
    }

    /// Apakah aksi ini butuh konfirmasi manusia? ( §F5: nominal besar /
    /// koleksi baru ). Pemanggil tetap memutuskan via Policy Engine.
    pub fn needs_review(&self, threshold_eth: f64) -> bool {
        self.value_eth().is_some_and(|v| v > threshold_eth)
    }
}

/// Konfigurasi engine.
#[derive(Debug, Clone, Copy)]
pub struct TradeEngineConfig {
    /// Listing dianggap basi setelah ini ( jam ).
    pub stale_listing_hours: u64,
    /// Batas nominal untuk needs_review.
    pub review_threshold_eth: f64,
}

impl Default for TradeEngineConfig {
    fn default() -> Self {
        Self {
            stale_listing_hours: 72,
            review_threshold_eth: 1.0,
        }
    }
}

/// Engine trading deterministik.
pub struct TradeEngine {
    rules: RulesConfig,
    config: TradeEngineConfig,
}

impl TradeEngine {
    pub fn new(rules: RulesConfig, config: TradeEngineConfig) -> Self {
        Self { rules, config }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules.rule
    }

    /// Evaluasi rule buy terhadap market satu koleksi.
    /// Mengembalikan aksi beli bila ada listing yang memenuhi kriteria.
    pub fn evaluate_buy(
        &self,
        market: &CollectionMarket,
        listings: &[crate::ListingInfo],
        bought_today: u32,
    ) -> Vec<TradeAction> {
        let mut actions = Vec::new();
        for rule in self.rules.rule.iter().filter(|r| r.enabled) {
            if !matches!(rule.kind, RuleKind::Buy) {
                continue;
            }
            if rule.collection_addr() != market.collection {
                continue;
            }
            // Batas beli per hari.
            if let Some(max) = rule.max_buy_per_day {
                if bought_today >= max {
                    continue;
                }
            }
            for l in listings
                .iter()
                .filter(|l| l.collection == market.collection)
            {
                let price = l.price_eth;
                // Kriteria 1: di bawah max_price_eth.
                if let Some(max) = rule.max_price_eth {
                    if price > max {
                        continue;
                    }
                }
                // Kriteria 2: below_floor_pct% di bawah floor.
                if let Some(pct) = rule.below_floor_pct {
                    let threshold = market.floor_eth * (1.0 - pct / 100.0);
                    if price > threshold {
                        continue;
                    }
                }
                actions.push(TradeAction::Buy {
                    rule: rule.name.clone(),
                    collection: l.collection,
                    order_id: l.order_id.clone(),
                    token_id: l.token_id,
                    price_eth: price,
                });
                // Satu aksi per rule per evaluasi.
                break;
            }
        }
        actions
    }

    /// Evaluasi rule sell: take-profit / stop-loss terhadap portfolio.
    pub fn evaluate_sell(&self, portfolio: &Portfolio) -> Vec<TradeAction> {
        let mut actions = Vec::new();
        for rule in self.rules.rule.iter().filter(|r| r.enabled) {
            if !matches!(rule.kind, RuleKind::Sell) {
                continue;
            }
            let collection = rule.collection_addr();
            for entry in portfolio.entries().filter(|e| e.collection == collection) {
                // Take profit: list di harga TP.
                if let Some(tp) = rule.take_profit_pct {
                    if entry.hit_take_profit(tp) {
                        let duration = rule.list_duration_hours.unwrap_or(72) * 3600;
                        actions.push(TradeAction::ListForSale {
                            rule: rule.name.clone(),
                            collection,
                            token_id: entry.token_id,
                            price_eth: entry.take_profit_price(tp),
                            duration_secs: duration,
                        });
                        continue;
                    }
                }
                // Stop loss.
                if let Some(sl) = rule.stop_loss_pct {
                    if entry.hit_stop_loss(sl) {
                        actions.push(TradeAction::StopLoss {
                            rule: rule.name.clone(),
                            collection,
                            token_id: entry.token_id,
                            price_eth: entry.stop_loss_price(sl),
                        });
                    }
                }
            }
        }
        actions
    }

    /// Apakah suatu aksi butuh review manusia ( §F5 )?
    pub fn needs_review(&self, action: &TradeAction) -> bool {
        action.needs_review(self.config.review_threshold_eth)
    }

    /// Berapa lama listing dianggap basi ( untuk auto-cancel ).
    pub fn stale_after_secs(&self) -> u64 {
        self.config.stale_listing_hours * 3600
    }
}

/// Info listing ringkas ( ditarik dari `nft_market::Listing` ).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListingInfo {
    pub order_id: String,
    pub collection: Address,
    pub token_id: U256,
    /// Harga ( ETH ).
    pub price_eth: f64,
    /// Listing dibuat pada ( unix detik ).
    pub created_at: i64,
}

impl ListingInfo {
    /// Apakah listing sudah basi?
    pub fn is_stale(&self, now_unix: i64, stale_secs: u64) -> bool {
        now_unix.saturating_sub(self.created_at) > stale_secs as i64
    }

    /// Bangun aksi cancel listing.
    pub fn cancel_action(&self, rule: &str) -> TradeAction {
        TradeAction::CancelStaleListing {
            rule: rule.into(),
            collection: self.collection,
            order_id: self.order_id.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse_rules;

    fn addr(b: u8) -> Address {
        Address::from_slice(&[b; 20])
    }

    fn market(floor: f64) -> CollectionMarket {
        CollectionMarket {
            collection: addr(1),
            floor_eth: floor,
            cheapest_listing_eth: Some(floor),
            listings_count: 5,
        }
    }

    fn listing(price: f64) -> ListingInfo {
        ListingInfo {
            order_id: "0xabc".into(),
            collection: addr(1),
            token_id: U256::from(7),
            price_eth: price,
            created_at: 0,
        }
    }

    const RULES: &str = r#"
        [[rule]]
        name = "snipe"
        kind = "buy"
        collection = "0x0101010101010101010101010101010101010101"
        action = "buy"
        max_price_eth = 8.0
        below_floor_pct = 10
        max_buy_per_day = 2

        [[rule]]
        name = "exit"
        kind = "sell"
        collection = "0x0101010101010101010101010101010101010101"
        action = "list"
        take_profit_pct = 25
        stop_loss_pct = 15
        list_duration_hours = 48
    "#;

    fn engine() -> TradeEngine {
        let rules = parse_rules(RULES).unwrap();
        TradeEngine::new(rules, TradeEngineConfig::default())
    }

    #[test]
    fn buy_di_bawah_max_dan_floor() {
        let e = engine();
        // floor 10, below_floor 10% -> beli bila <= 9.0 dan <= 8.0.
        let actions = e.evaluate_buy(&market(10.0), &[listing(8.0)], 0);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TradeAction::Buy {
                price_eth, rule, ..
            } => {
                assert_eq!(rule, "snipe");
                assert!((price_eth - 8.0).abs() < 1e-9);
            }
            _ => panic!("harus Buy"),
        }
    }

    #[test]
    fn buy_di_atas_max_ditolak() {
        let e = engine();
        let actions = e.evaluate_buy(&market(10.0), &[listing(8.5)], 0);
        // 8.5 <= 9.0 (below floor) tapi > 8.0 (max_price) → tidak ada aksi.
        assert!(actions.is_empty());
    }

    #[test]
    fn buy_di_atas_threshold_floor_ditolak() {
        let e = engine();
        // floor 5 → below_floor 10% → threshold 4.5; listing 4.6 lolos max 8
        // tapi di atas threshold.
        let actions = e.evaluate_buy(&market(5.0), &[listing(4.6)], 0);
        assert!(actions.is_empty());
    }

    #[test]
    fn buy_max_per_day_tercapai_dilewati() {
        let e = engine();
        let actions = e.evaluate_buy(&market(10.0), &[listing(8.0)], 2);
        assert!(actions.is_empty());
    }

    #[test]
    fn buy_max_per_day_masih_kuat() {
        let e = engine();
        let actions = e.evaluate_buy(&market(10.0), &[listing(8.0)], 1);
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn buy_koleksi_lain_diabaikan() {
        let e = engine();
        let mut m = market(10.0);
        m.collection = addr(2);
        let actions = e.evaluate_buy(&m, &[listing(8.0)], 0);
        assert!(actions.is_empty());
    }

    #[test]
    fn buy_rule_dimatikan_diabaikan() {
        let rules = parse_rules(&RULES.replace(
            "max_buy_per_day = 2",
            "max_buy_per_day = 2\nenabled = false",
        ))
        .unwrap();
        let e = TradeEngine::new(rules, TradeEngineConfig::default());
        let actions = e.evaluate_buy(&market(10.0), &[listing(8.0)], 0);
        assert!(actions.is_empty());
    }

    #[test]
    fn sell_take_profit() {
        let e = engine();
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.update_value(&addr(1), &U256::from(7), 1.30);
        let actions = e.evaluate_sell(&p);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TradeAction::ListForSale {
                price_eth,
                duration_secs,
                ..
            } => {
                assert!((price_eth - 1.25).abs() < 1e-9);
                assert_eq!(*duration_secs, 48 * 3600);
            }
            _ => panic!("harus ListForSale"),
        }
    }

    #[test]
    fn sell_stop_loss() {
        let e = engine();
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.update_value(&addr(1), &U256::from(7), 0.80);
        let actions = e.evaluate_sell(&p);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            TradeAction::StopLoss { price_eth, .. } => {
                assert!((price_eth - 0.85).abs() < 1e-9);
            }
            _ => panic!("harus StopLoss"),
        }
    }

    #[test]
    fn sell_tidur_saat_harga_tengah() {
        let e = engine();
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.update_value(&addr(1), &U256::from(7), 1.05);
        assert!(e.evaluate_sell(&p).is_empty());
    }

    #[test]
    fn sell_koleksi_tidak_ada_rule_diabaikan() {
        let e = engine();
        let mut p = Portfolio::new();
        p.record_buy(addr(9), U256::from(1), 1.0, 1);
        p.update_value(&addr(9), &U256::from(1), 5.0);
        assert!(e.evaluate_sell(&p).is_empty());
    }

    #[test]
    fn take_profit_didahulukan_dari_stop_loss() {
        let e = engine();
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.update_value(&addr(1), &U256::from(7), 2.0);
        let actions = e.evaluate_sell(&p);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], TradeAction::ListForSale { .. }));
    }

    #[test]
    fn needs_review_berdasarkan_threshold() {
        let e = engine();
        let a = TradeAction::Buy {
            rule: "x".into(),
            collection: addr(1),
            order_id: "o".into(),
            token_id: U256::from(1),
            price_eth: 2.0,
        };
        assert!(e.needs_review(&a));
        let b = TradeAction::Buy {
            rule: "x".into(),
            collection: addr(1),
            order_id: "o".into(),
            token_id: U256::from(1),
            price_eth: 0.5,
        };
        assert!(!e.needs_review(&b));
    }

    #[test]
    fn cancel_stale_listing_action() {
        let l = listing(1.0);
        let a = l.cancel_action("janitor");
        match a {
            TradeAction::CancelStaleListing { rule, order_id, .. } => {
                assert_eq!(rule, "janitor");
                assert_eq!(order_id, "0xabc");
            }
            _ => panic!("harus CancelStaleListing"),
        }
    }

    #[test]
    fn listing_stale_detection() {
        let l = listing(1.0);
        assert!(!l.is_stale(1000, 3600));
        assert!(l.is_stale(5000, 3600));
        // created_at=0, now=0 → tidak basi.
        assert!(!l.is_stale(0, 3600));
    }

    #[test]
    fn stale_after_secs_dari_config() {
        let e = engine();
        assert_eq!(e.stale_after_secs(), 72 * 3600);
    }

    #[test]
    fn trade_action_accessors() {
        let a = TradeAction::Buy {
            rule: "r".into(),
            collection: addr(1),
            order_id: "o".into(),
            token_id: U256::from(3),
            price_eth: 1.5,
        };
        assert_eq!(a.rule(), "r");
        assert_eq!(a.collection(), addr(1));
        assert_eq!(a.value_eth(), Some(1.5));
        assert!(a.needs_review(1.0));
        assert!(!a.needs_review(2.0));
        let c = TradeAction::CancelStaleListing {
            rule: "r".into(),
            collection: addr(1),
            order_id: "o".into(),
        };
        assert_eq!(c.value_eth(), None);
        assert!(!c.needs_review(0.0));
    }

    #[test]
    fn rule_action_custom_serde() {
        use crate::rules::RuleAction;
        let json = serde_json::to_string(&RuleAction::Custom("x".into())).unwrap();
        let back: RuleAction = serde_json::from_str(&json).unwrap();
        assert_eq!(back, RuleAction::Custom("x".into()));
    }
}
