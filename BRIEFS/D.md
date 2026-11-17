# BRIEF Lane D — Cliente TWS nativo en Rust (DeepSeek V4.1, sesión 2)

## Contexto
Eres la **Lane D** de **Aurea** (fork de Zeron). Construyes `crates/tws`: un **cliente del socket de IBKR
TWS escrito en Rust**, equivalente funcional del `@stoqey/ib` (Node) que usa Openbot (con su patch). Lee primero:
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\PLAN.md` (§3, §10.4, §8 riesgo TWS).
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\SPECS\D1.md … D4.md` — tus specs (LongCat).
- Código original: `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main\packages\core\src\trading\registro-ib.ts`
  (26 LOC de fábrica de conexión con `IBApiNext`) y el patch `patches/@stoqey%2Fib@1.6.13.patch` (lee QUÉ corrige —
  esos fixes son requisitos funcionales del port). El uso real del cliente está en `trading.ts` (búsqueda:
  `ib.`, `IBApiNext`, `openPosition`, `closePosition`, `account`, `position`).

## Identidad y zona
- Modelo: **deepseek-v4.1-flash-free** (sesión 2 — la sesión 1 hace la Lane A; no os cruzáis).
- **Tu zona exclusiva: `crates/tws/**`** (la creas tú). Si `crates/trading` necesita hooks (callbacks,
  tipos compartidos): coordina vía reporte al orquestador — no toques su carpeta.

## Elección de base (decisión tuya, justifícala en el reporte)
- **Opción preferida**: crate existente `ibapi` (client socket API de TWS en Rust) como dependencia, con
  wrappers propios en `crates/tws` para la superficie que Aurea usa (conexión persistente, contratos,
  posiciones, órdenes con OCA, cuentas). Si la crate no cubre algo, lo completas dentro de `crates/tws`.
- **Alternativa**: protocolo propio del socket TWS (mensajes de longitud prefijada) — solo si `ibapi`
  se queda corto; es más trabajo y más riesgo (§8 del plan).

## Entorno
- Windows. Repo: `C:\Users\WDAGUtilityAccount\Desktop\zeron` (git; NO hagas commit).
- `cargo` en `%USERPROFILE%\.cargo\bin`; `cargo check -p zeron-tws` y `cargo test -p zeron-tws`.
- **TWS paper**: el orquestador te dirá si hay un TWS/gateway corriendo en 127.0.0.1:7497 (paper) o 7496
  (live — PROHIBIDO en pruebas). Sin TWS disponible, tus tests de integración se marcan `#[ignore]` con
  env-gate (`AUREA_TWS_E2E=1`) y tu criterio D3 queda "validado contra TWS paper real" cuando el
  orquestador confirme entorno. Diseña TODO con la API async de tokio (connect/read/write con backoff).

## Cola de tareas (criterios en PLAN §10.4, comportamiento en SPECS/D1-D4)
- **D1** Conexión + persistencia + heartbeat + client ids 1000–9999 (asignados por el engine, persistidos
  en `trading_meta` — la persistencia la hace la Lane A; tú aceptas el id como parámetro). Reconexión limpia
  tras caída (backoff, sin duplicar órdenes). *Criterio: reconexión sin estado corrupto.*
- **D2** Qualify de contratos + lectura de posiciones/cuentas (incl. multi-account si TWS lo expone).
  *Criterio: paridad con `@stoqey/ib` en TWS paper.*
- **D3** Órdenes: cierre a mercado; fallback = dos órdenes límite OCA GTC ±4 ticks **en el grid del tick del
  contrato**; cancelación de órdenes propias; **abort limpio si detecta órdenes ajenas** (no son tuyas).
  *Criterio: escenarios D3 contra TWS paper real (o fixtures + validación posterior del orquestador).*
- **D4** Selección de cuenta activa + exposición agregada por cuenta (para el gate de cierre de la app).
  *Criterio: el gate podrá consultar exposición total antes de salir.*

## Reglas de dinero (innegociables)
- Ningún envío de orden contra 7496 (live). Paper (7497) o fixtures.
- Todas las rutas de órdenes: errores tipados, sin unwrap, timeouts en toda espera de respuesta del socket.
- Idempotencia: reintentar una orden jamás duplica (ids de orden gestionados y registrados).
- Documenta con `archivo:línea` del TS original cada invariante que portes.

## Reporte
Por tarea: qué implementaste, base elegida y por qué, tests, hallazgos del protocolo, peticiones al
orquestador (hooks con Lane A). Español, conciso.
