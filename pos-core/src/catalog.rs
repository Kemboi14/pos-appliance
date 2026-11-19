//! Product catalog with barcode and PLU indexes.

use crate::barcode::{self, BarcodeError};
use crate::config::{Config, TaxClassId};
use crate::money::Money;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub sku: String,
    /// Canonical barcode (see [`barcode::normalize_code`]).
    pub barcode: Option<String>,
    /// 5-digit PLU for variable-measure (price/weight-embedded) codes.
    pub plu: Option<String>,
    pub name: String,
    /// Unit price (per piece, or per kg when `weighable`).
    pub price: Money,
    pub tax_class: TaxClassId,
    pub weighable: bool,
    /// Position on the quick-key grid (0-based), if any.
    pub quick_key: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    BadSku,
    BadName,
    NegativePrice,
    UnknownTaxClass,
    Barcode(BarcodeError),
    BadPlu,
    DuplicateBarcode(String),
    DuplicatePlu(String),
    DuplicateQuickKey(u8),
    DuplicateSku(String),
    NotFound,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    items: BTreeMap<String, Item>,
    by_barcode: BTreeMap<String, String>,
    by_plu: BTreeMap<String, String>,
}

pub const MAX_ITEMS: usize = 100_000;

impl Catalog {
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn get(&self, sku: &str) -> Option<&Item> {
        self.items.get(sku)
    }
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.items.values()
    }
    pub fn by_barcode(&self, code: &str) -> Option<&Item> {
        self.by_barcode.get(code).and_then(|s| self.items.get(s))
    }
    pub fn by_plu(&self, plu: &str) -> Option<&Item> {
        self.by_plu.get(plu).and_then(|s| self.items.get(s))
    }
    pub fn quick_keys(&self) -> Vec<&Item> {
        let mut v: Vec<&Item> = self
            .items
            .values()
            .filter(|i| i.quick_key.is_some())
            .collect();
        v.sort_by_key(|i| i.quick_key);
        v
    }

    /// Case-insensitive substring match on name, or prefix match on SKU.
    pub fn search(&self, q: &str, limit: usize) -> Vec<&Item> {
        let q = q.trim().to_ascii_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        self.items
            .values()
            .filter(|i| {
                i.sku.to_ascii_lowercase().starts_with(&q)
                    || i.name.to_ascii_lowercase().contains(&q)
            })
            .take(limit)
            .collect()
    }

    /// Normalise and validate an item against `cfg`.
    pub fn validate_item(item: &Item, cfg: &Config) -> Result<Item, CatalogError> {
        let mut it = item.clone();
        it.sku = it.sku.trim().into();
        it.name = it.name.trim().into();
        if it.sku.is_empty() || it.sku.len() > 32 || !it.sku.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(CatalogError::BadSku);
        }
        if it.name.is_empty()
            || it.name.chars().count() > 40
            || it.name.chars().any(char::is_control)
        {
            return Err(CatalogError::BadName);
        }
        if it.price.is_negative() {
            return Err(CatalogError::NegativePrice);
        }
        if cfg.tax_class(it.tax_class).is_none() {
            return Err(CatalogError::UnknownTaxClass);
        }
        if let Some(b) = &it.barcode {
            it.barcode = Some(barcode::normalize_code(b).map_err(CatalogError::Barcode)?);
        }
        if let Some(p) = &it.plu
            && (p.len() != 5 || !p.bytes().all(|c| c.is_ascii_digit())) {
                return Err(CatalogError::BadPlu);
            }
        Ok(it)
    }

    fn check_unique(&self, it: &Item) -> Result<(), CatalogError> {
        if let Some(b) = &it.barcode
            && let Some(owner) = self.by_barcode.get(b)
                && *owner != it.sku {
                    return Err(CatalogError::DuplicateBarcode(b.clone()));
                }
        if let Some(p) = &it.plu
            && let Some(owner) = self.by_plu.get(p)
                && *owner != it.sku {
                    return Err(CatalogError::DuplicatePlu(p.clone()));
                }
        if let Some(q) = it.quick_key
            && self
                .items
                .values()
                .any(|o| o.quick_key == Some(q) && o.sku != it.sku)
            {
                return Err(CatalogError::DuplicateQuickKey(q));
            }
        Ok(())
    }

    /// Insert or replace one item (already validated).
    pub fn upsert(&mut self, it: Item) -> Result<(), CatalogError> {
        self.check_unique(&it)?;
        if !self.items.contains_key(&it.sku) && self.items.len() >= MAX_ITEMS {
            return Err(CatalogError::BadSku);
        }
        self.remove(&it.sku);
        if let Some(b) = &it.barcode {
            self.by_barcode.insert(b.clone(), it.sku.clone());
        }
        if let Some(p) = &it.plu {
            self.by_plu.insert(p.clone(), it.sku.clone());
        }
        self.items.insert(it.sku.clone(), it);
        Ok(())
    }

    pub fn remove(&mut self, sku: &str) -> Option<Item> {
        let old = self.items.remove(sku)?;
        if let Some(b) = &old.barcode {
            self.by_barcode.remove(b);
        }
        if let Some(p) = &old.plu {
            self.by_plu.remove(p);
        }
        Some(old)
    }

    /// Build a whole new catalog (atomic import). Fails without side effects.
    pub fn from_items(items: &[Item], cfg: &Config) -> Result<Catalog, (usize, CatalogError)> {
        if items.len() > MAX_ITEMS {
            return Err((MAX_ITEMS, CatalogError::BadSku));
        }
        let mut c = Catalog::default();
        for (i, it) in items.iter().enumerate() {
            let v = Self::validate_item(it, cfg).map_err(|e| (i, e))?;
            if c.items.contains_key(&v.sku) {
                return Err((i, CatalogError::DuplicateSku(v.sku)));
            }
            c.upsert(v).map_err(|e| (i, e))?;
        }
        Ok(c)
    }
}
