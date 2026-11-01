//! Portfolio tracker + PnL (blueprint §F2: "hargai beli, nilai sekarang, PnL").
//!
//! Posisi dicatat saat beli; nilai sekarang di-refresh dari floor price.
//! PnL = ( nilai sekarang - harga beli ) / harga beli × 100%.
//!
//! **Harga disimpan sebagai f64 ETH** — ini catatan akuntansi, bukan nilai
//! tx on-chain ( tx tetap memakai wei precision penuh di signer ).

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

/// Satu posisi NFT dalam portfolio.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PnLEntry {
    pub collection: Address,
    pub token_id: U256,
    /// Harga beli ( ETH ).
    pub buy_price_eth: f64,
    /// Nilai sekarang ( ETH ) — di-refresh dari floor/market.
    pub current_value_eth: f64,
    /// Jumlah unit ( biasanya 1 ).
    pub qty: u32,
}

impl PnLEntry {
    pub fn new(collection: Address, token_id: U256, buy_price_eth: f64, qty: u32) -> Self {
        Self {
            collection,
            token_id,
            buy_price_eth,
            current_value_eth: buy_price_eth,
            qty,
        }
    }

    /// PnL absolut ( ETH ).
    pub fn pnl_eth(&self) -> f64 {
        (self.current_value_eth - self.buy_price_eth) * self.qty as f64
    }

    /// PnL persen ( relatif terhadap harga beli ).
    pub fn pnl_pct(&self) -> f64 {
        if self.buy_price_eth <= 0.0 {
            return 0.0;
        }
        (self.current_value_eth - self.buy_price_eth) / self.buy_price_eth * 100.0
    }

    /// Harga list take-profit ( harga beli + TP% ).
    pub fn take_profit_price(&self, tp_pct: f64) -> f64 {
        self.buy_price_eth * (1.0 + tp_pct / 100.0)
    }

    /// Harga stop-loss ( harga beli - SL% ).
    pub fn stop_loss_price(&self, sl_pct: f64) -> f64 {
        self.buy_price_eth * (1.0 - sl_pct / 100.0)
    }

    /// Apakah harga sekarang sudah mencapai take-profit?
    pub fn hit_take_profit(&self, tp_pct: f64) -> bool {
        self.current_value_eth >= self.take_profit_price(tp_pct)
    }

    /// Apakah harga sekarang sudah menyentuh stop-loss?
    pub fn hit_stop_loss(&self, sl_pct: f64) -> bool {
        self.current_value_eth <= self.stop_loss_price(sl_pct)
    }
}

/// Portfolio: kumpulan posisi per koleksi.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    /// (collection, token_id) -> entry.
    entries: BTreeMap<String, PnLEntry>,
}

impl Portfolio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Daftar posisi ( snapshot untuk API / tampilan ).
    pub fn positions(&self) -> Vec<PnLEntry> {
        self.entries.values().cloned().collect()
    }

    fn key(collection: &Address, token_id: &U256) -> String {
        format!("{collection:?}:{token_id}")
    }

    /// Catat pembelian baru ( atau tambah qty bila sudah ada ).
    pub fn record_buy(
        &mut self,
        collection: Address,
        token_id: U256,
        price_eth: f64,
        qty: u32,
    ) -> &PnLEntry {
        let key = Self::key(&collection, &token_id);
        let price = if price_eth < 0.0 { 0.0 } else { price_eth };
        self.entries
            .entry(key)
            .and_modify(|e| {
                // Rata-rata tertimbang bila menambah posisi.
                let total_qty = e.qty + qty;
                e.buy_price_eth =
                    (e.buy_price_eth * e.qty as f64 + price * qty as f64) / total_qty as f64;
                e.qty = total_qty;
                e.current_value_eth = e.buy_price_eth;
            })
            .or_insert_with(|| PnLEntry::new(collection, token_id, price, qty));
        self.entries
            .get(&Self::key(&collection, &token_id))
            .expect("baru saja dimasukkan")
    }

    /// Hapus posisi ( setelah terjual ).
    pub fn remove(&mut self, collection: &Address, token_id: &U256) -> Option<PnLEntry> {
        let key = Self::key(collection, token_id);
        self.entries.remove(&key)
    }

    /// Refresh nilai sekarang satu posisi ( dari floor/market ).
    pub fn update_value(&mut self, collection: &Address, token_id: &U256, value_eth: f64) -> bool {
        let key = Self::key(collection, token_id);
        let value = if value_eth < 0.0 { 0.0 } else { value_eth };
        if let Some(e) = self.entries.get_mut(&key) {
            e.current_value_eth = value;
            true
        } else {
            false
        }
    }

    /// Refresh semua posisi koleksi ke floor baru.
    pub fn update_collection_floor(&mut self, collection: &Address, floor_eth: f64) -> usize {
        let mut updated = 0;
        for e in self.entries.values_mut() {
            if e.collection == *collection {
                e.current_value_eth = if floor_eth < 0.0 { 0.0 } else { floor_eth };
                updated += 1;
            }
        }
        updated
    }

    pub fn get(&self, collection: &Address, token_id: &U256) -> Option<&PnLEntry> {
        self.entries.get(&Self::key(collection, token_id))
    }

    pub fn entries(&self) -> impl Iterator<Item = &PnLEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total nilai sekarang ( ETH ).
    pub fn total_value_eth(&self) -> f64 {
        self.entries
            .values()
            .map(|e| e.current_value_eth * e.qty as f64)
            .sum()
    }

    /// Total modal ( ETH ).
    pub fn total_cost_eth(&self) -> f64 {
        self.entries
            .values()
            .map(|e| e.buy_price_eth * e.qty as f64)
            .sum()
    }

    /// PnL total ( ETH ).
    pub fn total_pnl_eth(&self) -> f64 {
        self.total_value_eth() - self.total_cost_eth()
    }

    /// PnL total ( % ).
    pub fn total_pnl_pct(&self) -> f64 {
        let cost = self.total_cost_eth();
        if cost <= 0.0 {
            return 0.0;
        }
        self.total_pnl_eth() / cost * 100.0
    }

    /// Ringkasan siap tampilkan ( bebas rahasia ).
    pub fn summarize(&self) -> String {
        if self.is_empty() {
            return "Portfolio kosong.".into();
        }
        let mut s = String::new();
        for e in self.entries.values() {
            s.push_str(&format!(
                "- {} #{}: beli {} ETH | nilai {} ETH | PnL {:+.2}%\n",
                short_addr(&e.collection),
                e.token_id,
                e.buy_price_eth,
                e.current_value_eth,
                e.pnl_pct()
            ));
        }
        s.push_str(&format!(
            "Total: modal {} ETH | nilai {} ETH | PnL {:+.4} ETH ({:+.2}%)",
            self.total_cost_eth(),
            self.total_value_eth(),
            self.total_pnl_eth(),
            self.total_pnl_pct()
        ));
        s
    }
}

fn short_addr(a: &Address) -> String {
    let s = format!("{a:?}");
    if s.len() <= 10 {
        s
    } else {
        format!("{}..{}", &s[..6], &s[s.len() - 4..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(b: u8) -> Address {
        Address::from_slice(&[b; 20])
    }

    fn entry(buy: f64, now: f64) -> PnLEntry {
        let mut e = PnLEntry::new(addr(1), U256::from(7), buy, 1);
        e.current_value_eth = now;
        e
    }

    #[test]
    fn pnl_positif_negatif_nol() {
        assert!((entry(1.0, 1.5).pnl_eth() - 0.5).abs() < 1e-12);
        assert!((entry(1.0, 0.75).pnl_eth() + 0.25).abs() < 1e-12);
        assert!((entry(1.0, 1.0).pnl_eth()).abs() < 1e-12);
    }

    #[test]
    fn pnl_pct_benar() {
        assert!((entry(1.0, 1.5).pnl_pct() - 50.0).abs() < 1e-9);
        assert!((entry(2.0, 1.5).pnl_pct() + 25.0).abs() < 1e-9);
        assert!((entry(0.0, 1.0).pnl_pct()).abs() < 1e-9); // bagi nol -> 0
    }

    #[test]
    fn take_profit_stop_loss_harga() {
        let e = entry(1.0, 1.0);
        assert!((e.take_profit_price(25.0) - 1.25).abs() < 1e-9);
        assert!((e.stop_loss_price(15.0) - 0.85).abs() < 1e-9);
    }

    #[test]
    fn hit_tp_sl() {
        let e = entry(1.0, 1.30);
        assert!(e.hit_take_profit(25.0));
        assert!(!e.hit_stop_loss(15.0));
        let e2 = entry(1.0, 0.80);
        assert!(e2.hit_stop_loss(15.0));
        assert!(!e2.hit_take_profit(25.0));
    }

    #[test]
    fn qty_menjumlah_pnl() {
        let e = PnLEntry::new(addr(1), U256::from(7), 1.0, 3);
        let mut e2 = e.clone();
        e2.current_value_eth = 1.5;
        assert!((e2.pnl_eth() - 1.5).abs() < 1e-12); // (1.5-1.0)*3
    }

    #[test]
    fn portfolio_record_get_remove() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        assert_eq!(p.len(), 1);
        assert!(p.get(&addr(1), &U256::from(7)).is_some());
        assert!(p.remove(&addr(1), &U256::from(7)).is_some());
        assert!(p.is_empty());
    }

    #[test]
    fn portfolio_rata_rata_tertimbang() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.record_buy(addr(1), U256::from(7), 2.0, 1);
        let e = p.get(&addr(1), &U256::from(7)).unwrap();
        assert!((e.buy_price_eth - 1.5).abs() < 1e-9);
        assert_eq!(e.qty, 2);
    }

    #[test]
    fn portfolio_update_value_dan_floor() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.record_buy(addr(1), U256::from(8), 2.0, 1);
        p.record_buy(addr(2), U256::from(1), 0.5, 1);
        assert_eq!(p.update_collection_floor(&addr(1), 1.75), 2);
        assert!((p.get(&addr(1), &U256::from(7)).unwrap().current_value_eth - 1.75).abs() < 1e-9);
        assert_eq!(
            p.get(&addr(2), &U256::from(1)).unwrap().current_value_eth,
            0.5
        );
        assert!(p.update_value(&addr(2), &U256::from(1), 0.9));
        assert!(!p.update_value(&addr(9), &U256::from(1), 1.0));
    }

    #[test]
    fn portfolio_total_pnl() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.record_buy(addr(2), U256::from(1), 3.0, 1);
        p.update_value(&addr(1), &U256::from(7), 1.5);
        p.update_value(&addr(2), &U256::from(1), 2.5);
        assert!((p.total_cost_eth() - 4.0).abs() < 1e-9);
        assert!((p.total_value_eth() - 4.0).abs() < 1e-9);
        assert!(p.total_pnl_eth().abs() < 1e-9);
        assert!(p.total_pnl_pct().abs() < 1e-9);
    }

    #[test]
    fn portfolio_total_pnl_positif() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 2.0, 1);
        p.update_value(&addr(1), &U256::from(7), 2.5);
        assert!((p.total_pnl_eth() - 0.5).abs() < 1e-9);
        assert!((p.total_pnl_pct() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn portfolio_kosong_total_nol() {
        let p = Portfolio::new();
        assert!(p.total_value_eth().abs() < 1e-9);
        assert!(p.total_pnl_pct().abs() < 1e-9);
    }

    #[test]
    fn harga_negatif_diklem_nol() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), -5.0, 1);
        assert_eq!(p.get(&addr(1), &U256::from(7)).unwrap().buy_price_eth, 0.0);
        p.update_value(&addr(1), &U256::from(7), -1.0);
        assert_eq!(
            p.get(&addr(1), &U256::from(7)).unwrap().current_value_eth,
            0.0
        );
    }

    #[test]
    fn summarize_berisi_koleksi_dan_pnl() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        p.update_value(&addr(1), &U256::from(7), 1.2);
        let s = p.summarize();
        assert!(s.contains("PnL"));
        assert!(s.contains("+20.00%"));
        assert!(s.contains("Total:"));
    }

    #[test]
    fn summarize_kosong_jelas() {
        assert_eq!(Portfolio::new().summarize(), "Portfolio kosong.");
    }

    #[test]
    fn short_addr_menyingkat() {
        let s = short_addr(&addr(0xab));
        assert!(s.contains(".."));
        assert!(s.len() < 42);
    }

    #[test]
    fn serde_roundtrip_entry() {
        let e = entry(1.5, 2.0);
        let json = serde_json::to_string(&e).unwrap();
        let back: PnLEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.token_id, U256::from(7));
        assert!((back.buy_price_eth - 1.5).abs() < 1e-9);
    }

    #[test]
    fn serde_roundtrip_portfolio() {
        let mut p = Portfolio::new();
        p.record_buy(addr(1), U256::from(7), 1.0, 1);
        let json = serde_json::to_string(&p).unwrap();
        let back: Portfolio = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 1);
    }
}
