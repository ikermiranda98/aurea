//! MetaTrader 5 support for the trading bots (lane A3).
//!
//! Bots are written against IBKR's `ib_async`. When one trades an MT5 account
//! the engine translates its calls into bridge commands
//! (`DEAL`/`PENDING`/`CLOSEVOL`/`SLTP`/`DELETE`): [`instruments`] resolves the
//! symbol, [`lots`] keeps the bot's exposure, [`libro`] types what the bridge
//! publishes and [`ordenes`] is the translator itself. The Python bridge that
//! talks to the terminal is embedded as `puente.py`.

pub mod bots;
pub mod enlace;
pub mod escritorio;
pub mod instruments;
pub mod libro;
pub mod lots;
pub mod manual;
pub mod ordenes;
pub mod puente_cliente;

pub use enlace::{AlmacenMt5, ConfiguracionMt5, EnlaceMt5, Enlaces, SinAlmacen};
pub use instruments::{Instrumento, con_id_de, duracion_segundos, magic_por_defecto};
pub use libro::{BarraMt5, LibroMt5, OperacionMt5, PosicionMt5, ResultadoMt5};
pub use lots::{lotes, lotes_por_unidad, precio_de, unidades};
pub use manual::{PeticionCierre, RespuestaMt5, cerrar_manual, orden_mt5};
pub use ordenes::{
    Cancelacion, ContratoJson, Ejecucion, EntornoMt5, ErrorTraductor, EstadoInicial, Hello,
    Nivel, OrdenVirtual, PeticionContrato, PeticionOrden, PosicionBot, ResumenCuenta, Traductor,
};
pub use puente_cliente::PuenteMt5;
