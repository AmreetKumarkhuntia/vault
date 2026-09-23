use std::collections::HashMap;
use std::sync::Arc;

use crate::StoreDriver;

/// Maps store kinds ("postgres", "redis") to drivers. Only the CLI registers
/// concrete drivers; the engine sees `dyn` objects. Adding MySQL later is a
/// new crate plus one `register` call.
#[derive(Default)]
pub struct StoreRegistry {
    drivers: HashMap<&'static str, Arc<dyn StoreDriver>>,
}

impl StoreRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, driver: Arc<dyn StoreDriver>) {
        self.drivers.insert(driver.kind(), driver);
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn StoreDriver>> {
        self.drivers.get(kind).cloned()
    }

    pub fn kinds(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.drivers.keys().copied()
    }
}
