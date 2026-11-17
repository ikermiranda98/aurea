# BRIEF Lane A — Motor de trading en Rust (DeepSeek V4.1)

## Contexto (léelo completo antes de empezar)
Eres la **Lane A** de **Aurea**, un dashboard de trading nativo construido como fork del codebase
Zeron (Rust + GPUI + engine daemon). El proyecto portá el motor de trading de Openbot (TypeScript)
a Rust. Lee primero:
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\PLAN.md` — el plan aprobado (§3 arquitectura, §10.4 tu cola).
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\SPECS\A1.md … A6.md` — tus specs de port (LongCat las escribió leyendo el código TS original; son tu fuente de verdad de comportamiento).
- Código original de referencia: `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main\packages\core\src\trading.ts` (+ `trading/*`) y sus tests en `packages/core/test/`.

## Identidad y zona
- Modelo: **deepseek-v4.1-flash-free**. Eres la lane de mayor confianza para lógica de dinero.
- **Tu zona exclusiva: `crates/trading/**`** (la crearás tú). Nada fuera de ahí, salvo: puedes PEDIR al
  orquestador (en tu reporte) cambios en `crates/engine`/`crates/proto` — no los hagas tú.
- Trabajo secuencial: A1 → A2 → A3 → A4 → A5 → A6. No avances de tarea sin cumplir su criterio.

## Entorno
- Windows. Repo: `C:\Users\WDAGUtilityAccount\Desktop\zeron` (git; NO hagas commit).
- Toolchain: `cargo` en `%USERPROFILE%\.cargo\bin` (se instaló en background; verifica con `cargo --version`).
- Toolchain de Zeron: `stable` + rustfmt + clippy (`rust-toolchain.toml`). Usa `cargo check -p zeron-trading`
  durante el desarrollo y `cargo test -p zeron-trading` al cerrar cada tarea.
- La app es **tokio** por todas partes. Persistencia: `rusqlite` (o `sqlx` sqlite síncrono) — decide tú y
  sé consistente. Sigue el estilo del codebase Zeron (lee 2-3 módulos de `crates/engine/src/` primero).

## GATE
No empieces **A3/A4** (dinero) hasta que exista `C:\Users\WDAGUtilityAccount\Desktop\zeron\SPECS\CONTRACTS.md`
(el orquestador lo escribe al congelar los contratos proto/RPC). A1 y A2 puedes empezar ya.

## Cola de tareas (criterios completos en PLAN §10.4, comportamiento en SPECS/A1-A6)
- **A1** Pool de sistemas (`NN_name`: `config.py`/`reglas.json`/`system.json`, `META`) + `snapshot()` + store SQLite (7 tablas: trading_task/trade/link/meta/account/bot_order/bot_fill/close, migraciones equivalentes). *Criterio: snapshot reproduce los fixtures de Openbot; tests de sql portados.*
- **A2** Ciclo de vida de bots: spawn `pythonw bot.py` + adopción vía `panel.pid` + parada con pid-guard de Windows (`Win32_Process` CommandLine contiene `bot.py` en la carpeta del sistema; respuestas de tres vías nunca dejan bots vivos colgando) + servidor-bots HTTP local `127.0.0.1` con rutas `/bot/*` y header por sistema. *Criterio: sección lifecycle del test-oráculo pasa en Rust.*
- **A3** Translator MT5: semántica `ib_async` → MT5 (DEAL/pending/SL/TP, OCA, fills, matemática de lotes que preserva exposición) + codificación de órdenes. *Criterio: `trading-bots-mt5.test.ts` (141 LOC) pasa portado; e2e OCA tras flag.*
- **A4** Ciclos de cierre: cancelar órdenes propias primero (órdenes ajenas abortan), relectura de posición, mercado opuesto, fallback = dos límites OCA GTC ±4 ticks en grid de tick; `closeAll`; `panic`; **claims** (id `cierre:<requestId>`, lock único por contrato, validez 5 min, replay 24 h, fail-closed con DB caída) + veredictos (Cierre/Vestigio/Interrumpido/Ocupado/DbCaida). *Criterio: tests de claims/replay idénticos a Openbot.*
- **A5** Tasks programadas (add/cancel, expiry loop), links persistidos (host:port IBKR / mt5-<login>@<server>), detección de programas TWS/MT5 (registro + tasklist), stub de registro-ib hacia `crates/tws` (D lane). *Criterio: detección funciona en Windows real.*
- **A6** Superficie de acciones: firma exacta de `state/system_start/system_stop/task_add/task_cancel/position_close/positions_close_all/panic/connect/disconnect/account_select` como API Rust limpia (`crates/trading`), lista para el wiring RPC/MCP del orquestador. *Criterio: coincide con SPECS/CONTRACTS.md.*

## Reglas de dinero (innegociables)
- Los tests de Openbot son el oráculo: portarlos ANTES de dar una tarea por hecha.
- Fail-closed siempre en cierres: DB caída ⇒ no cerrar, veredicto DbCaida.
- Ningún `unwrap()` en rutas de órdenes/cierres; errores tipados.
- No toques el gateway de dinero real (TWS live/MT5 real) en pruebas: paper/demo o fixtures.

## Reporte (al terminar cada tarea y al final)
Tarea completada + criterio cumplido (qué test lo prueba) + diffs tocados + peticiones al orquestador
(cambios en proto/engine) + dudas. Español, conciso.
