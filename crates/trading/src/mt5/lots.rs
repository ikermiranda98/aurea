//! Lot and unit maths that keep a bot's exposure when it trades MT5.
//!
//! One bot unit is one IBKR contract. For futures and CFDs,
//! [`lotes_por_unidad`] turns its money-per-point into MT5 lots; for forex,
//! bot quantities are already units of the base currency and a lot is
//! `contract_size` of them. A direct port of `bots-mt5.ts`
//! (`lotesPorUnidad`/`lotes`/`unidades`/`precioDe`/`redondear`).

use super::instruments::Instrumento;

/// `Math.round` for the positive values traded here.
pub(crate) fn redondear(valor: f64, digitos: u32) -> f64 {
    let factor = 10f64.powi(digitos as i32);
    (valor * factor).round() / factor
}

/// MT5 lots for one bot unit (same money per point).
pub fn lotes_por_unidad(instrumento: &Instrumento) -> f64 {
    if instrumento.forex {
        return 1.0 / instrumento.contract_size.max(1e-9);
    }
    let por_lote = if instrumento.tick_size > 0.0 {
        instrumento.tick_value / instrumento.tick_size
    } else {
        0.0
    };
    if por_lote > 0.0 {
        instrumento.multiplier / por_lote
    } else {
        1.0
    }
}

/// Lots for `qty` bot units, on the symbol's volume step and inside its limits.
pub fn lotes(instrumento: &Instrumento, qty: f64) -> f64 {
    let paso = if instrumento.volume_step > 0.0 {
        instrumento.volume_step
    } else {
        0.01
    };
    let mut valor = (qty.abs() * lotes_por_unidad(instrumento) / paso).round() * paso;
    if valor < instrumento.volume_min && qty.abs() > 0.0 {
        valor = instrumento.volume_min;
    }
    if instrumento.volume_max > 0.0 {
        valor = valor.min(instrumento.volume_max);
    }
    redondear(valor, 8)
}

/// Bot units for `lots`.
pub fn unidades(instrumento: &Instrumento, lots: f64) -> f64 {
    let unidades = lots / lotes_por_unidad(instrumento);
    let entero = unidades.round();
    if (unidades - entero).abs() < 1e-6 {
        entero
    } else {
        redondear(unidades, 6)
    }
}

/// A price rounded to the symbol's tick and number of digits.
pub fn precio_de(instrumento: &Instrumento, precio: f64) -> f64 {
    let al_tick = if instrumento.tick_size > 0.0 {
        (precio / instrumento.tick_size).round() * instrumento.tick_size
    } else {
        precio
    };
    redondear(al_tick, instrumento.digits.clamp(0, 10) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instrumento(forex: bool) -> Instrumento {
        Instrumento {
            con_id: 1,
            root: if forex { "USDJPY" } else { "MNQ" }.into(),
            sec_type: "FUT".into(),
            exchange: String::new(),
            currency: "USD".into(),
            multiplier: 2.0,
            forex,
            account: "a".into(),
            mt5: "US100".into(),
            digits: 2,
            contract_size: if forex { 100_000.0 } else { 1.0 },
            tick_size: 0.01,
            tick_value: 0.01,
            volume_min: 0.01,
            volume_step: 0.01,
            volume_max: 100.0,
        }
    }

    #[test]
    fn sizes_keep_the_bots_exposure() {
        // 1 MNQ = 2 $/point = 2 lots of a 1 $/point CFD.
        assert_eq!(lotes(&instrumento(false), 1.0), 2.0);
        assert_eq!(unidades(&instrumento(false), 4.0), 2.0);
        // Forex quantities are units of the base currency.
        assert_eq!(lotes(&instrumento(true), 100_000.0), 1.0);
        assert_eq!(unidades(&instrumento(true), 0.5), 50_000.0);
        // Gold: 1 MGC = 10 $/point = 0.1 lots of a 100 oz contract (tick 0.01 → 1 $).
        let oro = Instrumento {
            root: "MGC".into(),
            multiplier: 10.0,
            tick_value: 1.0,
            ..instrumento(false)
        };
        assert_eq!(lotes(&oro, 1.0), 0.1);
    }

    #[test]
    fn lot_sizes_respect_the_symbol_limits() {
        let mut pequeno = instrumento(false);
        pequeno.multiplier = 0.001;
        assert_eq!(lotes(&pequeno, 1.0), 0.01);

        let mut grande = instrumento(false);
        grande.multiplier = 1000.0;
        assert_eq!(lotes(&grande, 10.0), 100.0);

        // No step on the symbol: the fallback is one hundredth of a lot.
        let mut sin_paso = instrumento(false);
        sin_paso.volume_step = 0.0;
        assert_eq!(lotes(&sin_paso, 1.0), 2.0);

        // A zero quantity stays at zero, never the minimum.
        assert_eq!(lotes(&instrumento(false), 0.0), 0.0);
    }

    #[test]
    fn prices_round_to_the_tick_and_the_symbol_digits() {
        let i = instrumento(false);
        assert_eq!(precio_de(&i, 17_345.678), 17_345.68);
        assert_eq!(precio_de(&i, -0.001), -0.0);
        let sin_tick = Instrumento {
            tick_size: 0.0,
            ..instrumento(false)
        };
        assert_eq!(precio_de(&sin_tick, 1.234_567_89), 1.23);
    }
}
