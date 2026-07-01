#[derive(Clone, Copy)]
pub struct KernelSymbol {
    pub addr: u64,
    pub name: &'static str,
}

#[derive(Clone, Copy)]
pub struct SymbolizedRip {
    pub symbol: &'static str,
    pub offset: u64,
}

include!(concat!(env!("OUT_DIR"), "/kallsyms.rs"));

pub fn lookup(addr: u64) -> Option<SymbolizedRip> {
    if KERNEL_SYMBOLS.is_empty() {
        return None;
    }

    let mut lo = 0usize;
    let mut hi = KERNEL_SYMBOLS.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if KERNEL_SYMBOLS[mid].addr <= addr {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    if lo == 0 {
        return None;
    }

    let symbol = KERNEL_SYMBOLS[lo - 1];
    Some(SymbolizedRip {
        symbol: symbol.name,
        offset: addr.wrapping_sub(symbol.addr),
    })
}
