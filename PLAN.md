# Plan de mudanza: Openbot-v2 → **Aurea**

> Estado: **BORRADOR PARA VALIDACIÓN — no se ha tocado código.**
> Fecha: 2026-11-17 · Autor: plan elaborado con OpenCode sobre inventarios verificados de ambos repos.

---

## 1. Qué es Aurea

**Aurea** (sin tilde) es el sucesor de Openbot-v2: una app de escritorio **Windows-first de trading nativo** con:

- **Motor de trading integrado**: bots sobre **IBKR TWS** y **MT5**, ciclos de cierre con claims duraderos por terminal y símbolo, gate de cierre que avisa de exposición de todas las cuentas antes de salir, tareas programadas, panic.
- **Agentes IA de apoyo** (no protagonistas): sesiones con los modelos **free de OpenCode, filtrados a DeepSeek v4.1 y GLM 5.3**, para el chat del desk y análisis de mercado.
- **Estética de Zeron**: tema oscuro/glass con kit de animaciones GPUI, más **paneles de trading** propios (KPIs, curva de equity, posiciones, sistemas, registro de veredictos) y **acento dorado** propio de la marca.
- **Local-first**: sin login ni sync en v1 (la infraestructura de sync queda en el código, dormida, para una "edición sync" futura).
- **Actualizador propio** transaccional con releases en GitHub.

| | Openbot-v2 (hoy) | Aurea (destino) |
|---|---|---|
| Base | Fork de OpenCode V2 (TypeScript/Bun) | Fork de `zeronsh/zeron` (Rust + GPUI) |
| Shell | Electron 44 + SolidJS | Binario nativo GPUI (una sola app + engine) |
| Motor trading | TS, integrado en el core de OpenCode | **Rust, integrado en el engine** |
| Bots (estrategias) | Python `ib_async` (`NN_name/bot.py`) | **Iguales** — se conservan tal cual |
| MT5 | Bridge Python (JSON stdio) | **Iguales** — mismo bridge, re-hospedado |
| TWS | Socket Node (`@stoqey/ib` parcheado) | Cliente TWS **nativo en Rust** |
| Agentes | El harness es OpenCode entero | Harness **opencode** de Zeron + catálogo filtrado |
| WARP ring | `ghost.ts` en el core TS | Módulo Rust en el engine |
| Updater | electron-updater + patches | `crates/update` de Zeron (transaccional) |
| Sync móvil | No | Infra en código, desactivada (fase futura) |
| Nombre visible | OpenBot / opencode | **Aurea** (interno `zeron-*` se mantiene) |

---

## 2. Decisiones cerradas (registradas contigo)

1. **Nombre**: **Aurea**, sin tilde — la proporción áurea (base de los retrocesos de Fibonacci) y el oro como símbolo de inversión; sin tilde para binarios, repo y dominio limpios.
2. **Motor de trading**: integrado en el engine de Zeron — port TS→Rust (decisión tuya, con plan B de emergencia documentado en §8).
3. **Agentes**: solo harness OpenCode; catálogo filtrado a **DeepSeek v4.1 + GLM 5.3**. El dashboard es el protagonista; el chat es panel secundario.
4. **Multi-dispositivo**: infra de sync **se conserva en el código** (edge TypeScript, `crates/client`); v1 **sin móvil, sin login, local-first**.
5. **Estética**: look Zeron + paneles de trading; tema por defecto Aurea (oscuro glass + dorado).
6. **Renombrado**: **solo lo visible**. Interno (`zeron-*` en crates, `ZERON_*` en env vars, puertos, `.zeron`, edge) **se queda como está** — ~1.845 archivos / ~10.000 referencias se quedan intactas.
7. **Plugins (apps/zgx)**: **fuera de la v1**. Están aislados del core; retorno posible en fases futuras sin rediseño.

---

## 3. Arquitectura destino

```
aurea.exe (GPUI, headed) ── IPC WebSocket ── aurea-engine (daemon Rust)
                                              │
        ┌─────────────────────────────────────┼──────────────────────────────────┐
        │                                     │                                  │
  MÓDULO TRADING (nuevo, Rust)         NÚCLEO SESIONES (upstream)          SERVICIOS (upstream)
  · pool de sistemas NN_name           · run journal + recovery            · terminales PTY
    (config.py / reglas.json)          · steering + cola duradera          · notificaciones
  · bots: spawn/adopt/kill             · doc host Loro CRDT (SQLite)       · uploads
    (pythonw bot.py, pid-guard)        · registro de espacios              · identidad de device
  · servidor-bots HTTP local                                               · update transaccional
    127.0.0.1 /bot/* token             HARNESS (adapter único)             · voice (dictado local)
  · TWS: socket Rust (7497/7496,          └─ opencode driver (HTTP/SSE)
    client ids 1000–9999)                    → `opencode serve` sidecar
  · MT5: bridge Python re-hospedado          → catálogo filtrado:
    (JSON stdio) + translator Rust              DeepSeek v4.1 · GLM 5.3
  · ciclos de cierre + claims              · MCP del engine: herramientas
    SQLite (8 tablas) + gate                  `trading.*` para los agentes
  · tasks · links · snapshot · panic
  · WARP ring (fase 4): warp-cli, rotación por rechazo, egress proxy
```

**Flujo de datos (idéntico a Openbot, nuevos materiales):**
`bot.py → pyshim → servidor-bots HTTP → engine (Rust) → TWS (socket Rust) / MT5 (bridge Python)`.

**Chat del desk:** sesión con metadata `aurea.trading` + contexto permanente del desk; los agentes actúan sobre el motor vía **MCP tools** servidas por el engine (equivalente de `tool/plugin/trading.ts` de Openbot).

---

## 4. Matriz ZERON: mantener / quitar / cambiar

### 4.1 MANTENER sin cambios (el 80 % del valor)

| Pieza | Rutas | Por qué |
|---|---|---|
| Núcleo de sesiones | `crates/engine/src/{sessions,runner,run_journal,sessions_activity,wake_*}` | Journal reanudable, recovery, watchdog de stall — sirve igual para chats de trading |
| Doc host + CRDT | `crates/doc`, `crates/sync`, `crates/engine/src/doc_host*` | Persistencia local-first; el modelo de doc aguanta el estado de trading |
| RPC | `crates/rpc` (patrón: constante + match arm + `forwardable()`) | Añadir RPCs de trading es barato y está documentado |
| Shell UI | `crates/ui/src/{shell,transcript,composer,pickers}` + `crates/ui/src/motion.rs` | La columna vertebral visual que quieres conservar |
| Tema | `crates/theme` (30 familias, claro+oscuro, frost/opaque) | Base del look; añadiremos la familia **Aurea** |
| Terminales | `crates/engine/src/terminals*`, `crates/term` | Consolas de bots / REPLs gratis |
| Notificaciones | `crates/notify`, `crates/engine/src/desktop_notify.rs` | Banners de fills/panic/margin con la app cerrada |
| Uploads | `crates/engine/src/uploads.rs` | CSVs de trades, capturas, adjuntos |
| Voice (dictado) | `crates/voice` | Barato de mantener, se desactiva por defecto |
| Harness opencode | `crates/harness/src/opencode/` | **Ya habla el protocolo de OpenCode** (1.18 y 2.x); es el camino a tus modelos free |
| MCP | `crates/mcp` | Vía para exponer `trading.*` a los agentes |
| Visual (charts) | `crates/visual` | Primitivas nativas de charts/tablas/canvas para los paneles |
| Markdown/latex/syntax/text/orb | `crates/{markdown,zatex,syntax,text,orb}` | Render del transcript y detalles visuales |
| Update transaccional | `crates/update` | Sustituye al updater de Openbot con mejor garantía |
| Engine management | `crates/engine/src/{managed,instance_lock}` + `apps/zeron` | Headed/headless daemon, arranque en background |
| Identidad/privacidad | workspace local-first (`WorkspaceScope::Local`) | Sin login en v1: perfil Local por defecto |

### 4.2 QUITAR de la v1 (neutralizado, sin reescritura)

| Pieza | Rutas | Nota |
|---|---|---|
| Plugins/zgx | `apps/zgx`, `crates/engine/src/plugins/`, RPCs `*Plugin*` | Decisión tuya; retorno futuro posible (está aislado) |
| Harnesses no usados | `crates/harness/src/{claude,codex,cursor,pi,muse,acp*}` | Se conservan `opencode` y `mock` (tests). El trait queda intacto |
| Cuentas de agentes + vault | `crates/engine/src/agent_accounts*`, `edge/vault` | Sustituido por credenciales de brokers (patrón reutilizable) |
| Git/repos/worktrees/diffs | `crates/engine/src/{repos,git_tree,diff_sync*}`, `crates/ui/src/{changes,source_control,pull_request_*}` | Codificación-pura; quedan los RPC genéricos de carpetas (`ListFolders`, `SearchFiles`) |
| Cloud + billing | `crates/engine/src/cloud_*`, `edge/src/cloud/`, Stripe, `commit_credit` | Sandbox hospedado no aplica |
| Preview/dev-servers | `crates/preview*` | Codificación |
| Landing/móvil/cards | `apps/{landing,card-web,ios,android,www-redirect}` | Sin builds en v1; `apps/ios`/`apps/android` pueden quedarse dormidos en repo |
| Login/WorkOS UI | pantallas Gate/OrgGate | v1 arranca local-first sin login (código edge intacto, sin deploy) |

### 4.3 CAMBIAR / AÑADIR (el trabajo real)

| Trabajo | Origen | Destino | Tamaño aprox. |
|---|---|---|---|
| **Módulo trading Rust** | `packages/core/src/trading.ts` (4.445 LOC) + `trading/*` (2.7k) | Nueva `crates/trading` integrada en engine | ~7k LOC TS → ~9-11k LOC Rust |
| Cliente TWS Rust | `@stoqey/ib` parcheado (Node) | Cliente socket nativo (crate `ibapi` o propio) | Nuevo, ~2-4k LOC |
| Translator MT5 Rust | `trading/bots-mt5.ts` (912) | Port 1:1 a Rust (semántica de órdenes, OCA, fills) | ~1.2k LOC Rust |
| RPCs de trading | 17 endpoints `experimental.trading.*` | RPCs nativos: `WatchTradingSnapshot` (stream), `TradingAction*` | ~15-25 métodos |
| **Dashboard GPUI** | `packages/app/src/trading/*` (~4.7k SolidJS) | `crates/ui/src/trading/` nueva surface | ~5-6k LOC Rust |
| Anillo WARP | `core/src/ghost.ts` (733) + egress (211) | Módulo engine + widget en titlebar (port de `free-daily-ring*`) | ~1k LOC Rust |
| Gate de cierre | `desktop/src/main/windows/trading-close.ts` (107) | Diálogo GPUI al cerrar app + `desktop_notify` | ~300 LOC |
| Credenciales broker | patrón de `agent_accounts` | slots IBKR paper/live + MT5 login@server | ~500 LOC |
| i18n es/en | ~400 claves `trading.*` × 68 idiomas | Infra i18n mínima en `crates/ui` (upstream es EN-only) + solo es/en | ~600 claves |
| Tema Aurea | — | Familia builtin propia (glass oscuro + acento dorado) | ~200 LOC |
| Renombrado visible | — | Ver §6 | ~60 toques |

---

## 5. Matriz OPENBOT: port / re-hospedar / reescribir / descartar

### 5.1 PORT a Rust (lógica exacta, tests como oráculo)

| Origen | LOC | Notas |
|---|---|---|
| `packages/core/src/trading.ts` | 4.445 | Pool scan, ciclo de vida de bots, TWS, cierres con fallback OCA ±4 ticks en grid de tick, panic, tasks, links, snapshot, claims `cierre:<requestId>` con valides 5 min y replay 24 h, fail-closed con DB caída |
| `packages/core/src/trading/sql.ts` | 127 | 8 tablas drizzle → `rusqlite` con migraciones equivalentes |
| `packages/core/src/trading/servidor-bots.ts` | 104 | API local `/bot/*` con header `X-OpenBot-Bot` por sistema |
| `packages/core/src/trading/bots-mt5.ts` | 912 | Semántica `ib_async` → MT5 DEAL/pending/SL/TP, OCA, matemática de lotes que preserva exposición |
| `packages/core/src/trading/mt5-ordenes.ts` | 133 | Codificación de órdenes MT5 |
| `packages/core/src/trading/programas.ts` | 113 | Detección TWS/MT5 (registro, `tasklist`, `abrir()` bajo demanda) |
| `packages/core/src/tool/plugin/trading.ts` | 264 | Herramientas `state/system_start/system_stop/task_add/task_cancel/position_close/positions_close_all/panic/connect/disconnect/account_select` → **MCP tools del engine** |
| `packages/core/src/ghost.ts` + `ghost-egress.ts` | 944 | Anillo WARP: control `warp-cli`, renovación día UTC, memoria /64, rotación por rechazo (fase 4) |

**Oráculo de comportamiento:** `trading-engine.test.ts` (922), `trading-bots-mt5.test.ts` (141 + OCA e2e), `tool-trading.test.ts` (99), fixtures `fixtures/bots-mt5/` (carpetas falsas 96..99) y `fixtures/mt5-falso/`.

### 5.2 RE-HOSPEDAR sin portar (recursos externos al binario)

| Recurso | Origen | Tratamiento |
|---|---|---|
| Bots Python de usuario | carpetas `NN_name/` con `bot.py` (`ib_async`) | **Se conservan tal cual** — son estrategia del usuario, no código de la app |
| Pyshim `ib_async` | `trading/bots-py.ts` (576, fuente Python) | Se escribe a `%APPDATA%\Aurea\bots-mt5\pyshim`; el Rust la lleva como recurso embebido |
| Bridge MT5 (PUENTE_PY) | `trading/mt5.ts` (698) | Mismo proceso Python JSON-stdio; detección de terminales portada a Rust; fuente como recurso embebido |
| Runtime MT5 | `packages/desktop/script/build-mt5-runtime.py` | Mismo script de build (Python 3.14 + `MetaTrader5`) → `resources/mt5-runtime` |
| Sidecar opencode | binario `opencode` de OpenCode | Descarga/canal estable; Aurea gestiona versión como hoy el hex fetch |

### 5.3 REESCRIBIR (nativa GPUI)

| Origen | LOC | Destino |
|---|---|---|
| `packages/app/src/trading/route.tsx` | 3.093 | Surface dashboard: strip KPIs, curva equity, tabla posiciones, panel sistemas (arranque masivo gating), tasks, registro, wizard de cuenta (knock 7497/7496 / detección MT5), editor de prompts |
| `packages/app/src/trading/{nav,nav-estado}.tsx` | 747 | Nav strip de links (tabs por servidor) |
| `packages/app/src/trading/registro.tsx` | 372 | Registro con veredictos i18n |
| `packages/app/src/trading/motor.ts` | 486 | Cliente de engine (poll 3 s → stream RPC) |
| `packages/app/src/usage/free-daily-ring*` | ~900 | Widget del anillo en la titlebar |
| `desktop/.../trading-close.ts` | 107 | Diálogo de gate al cerrar la app |

### 5.4 DESCARTAR

- Shell Electron completa (`packages/desktop`, ~23k LOC) y su IPC/preload/sidecar-cli.
- `gui-plugins/updater` + patches de electron-builder/updater → lo sustituye `crates/update`.
- i18n × 68 idiomas → solo **es/en**.
- `services/*` (site Cloudflare, update/data/files/legacy/www), `salesforce/`, `nix/`, `spikes/tauri/`.
- Todo lo upstream-OpenCode que no sea el lado agéntico (tui, ui, theme, session-ui, etc. de packages/*).

---

## 6. Renombrado visible (único renombrado que se hace)

**Regla:** si el usuario lo ve → se renombra a **Aurea**. Si es interno → se queda `zeron`.

| Visible | Acción |
|---|---|
| Binarios/procesos (`apps/zeron` → binario `aurea`; nombre de proceso del engine en Task Manager) | Renombrar binario y display |
| Título de ventana, splash de arranque, About, cabeceras de Settings | Renombrar cadenas |
| Notificaciones de escritorio ("Aurea") | Renombrar |
| Iconos/logos (mark propio dorado) | Nuevos assets |
| Cadena "Zeron" en strings de UI que queden visibles | Sustituir |
| Repo propio y releases | `ikermiranda98/aurea` |
| Carpeta de datos visible en docs/soporte (`%APPDATA%\Aurea`) | Nueva ruta para datos de trading; datos del engine internos siguen su curso |

**Se queda como está (invisible):** nombres de crates (`zeron-proto`, `zeron-engine`…), `ZERON_*` env vars, puerto IPC 27654, protocolo Loro/chat2, edge, `.zeron` interno del engine, módulos Rust, tests.

---

## 7. Fases (orden por riesgo)

> **Duraciones en jornada humana de referencia** (trabajo diurno, un lane = una persona-sesión).
> Con el **modo de esfuerzo máximo 24/7** (§10.7) las Fases 0-5 se comprimen a **~2-4 días de reloj**; el desglose hora a hora está en §10.7.

**Fase 0 — Fork y cimientos** (~3-5 días)
Nuevo repo `aurea` desde `zeronsh/zeron`; aplicar quita (§4.2); árbol compila; binario `aurea`; rename visible mínimo; tema default con acento dorado; CI Windows; arranque local-first sin login. **Criterio de salida:** la app abre, el shell luce, sesiones de prueba con DeepSeek/GLM funcionan vía harness opencode con catálogo filtrado.

**Fase 1 — Motor trading en Rust + dashboard mínimo** (~4-6 semanas)
`crates/trading`: pool, ciclo de vida de bots, servidor-bots, snapshot, DB 8 tablas, **MT5 completo** (bridge re-hospedado + translator Rust), RPCs `WatchTradingSnapshot`/acciones; UI: nav strip, KPIs, posiciones, sistemas, registro, wizard de link MT5. **Criterio:** arrancar/parar bots MT5 y cerrar posiciones desde el dashboard, con claims y veredictos traducidos.

**Fase 2 — Agentes y gate** (~2-3 semanas)
Desk chat con contexto permanente; MCP tools `trading.*` para los agentes; filtrado del catálogo confirmado (DeepSeek v4.1 + GLM 5.3); gate de cierre al salir; notificaciones de fills/panic. **Criterio:** pedir al agente "cierra la posición de X" y verlo ejecutarse; el gate avisa con bots vivos.

**Fase 3 — TWS nativo en Rust** (~3-4 semanas)
Cliente TWS (crate `ibapi` o propio): puertos 7497/7496, client ids 1000-9999, cuentas paper/live, cierres con fallback OCA, gate de exposición multi-cuenta, tasks programadas. **Criterio:** paridad con el motor IBKR de Openbot (oráculo: tests 922 LOC).

**Fase 4 — Anillo WARP** (~1-2 semanas)
Port de `ghost.ts`: servicio WARP + modo proxy + `HTTPS_PROXY` para el sidecar; rotación por rechazo (los retries del harness ya se surfacen); memoria /64; widget del anillo en titlebar. **Criterio:** el límite diario free sobrevive a las denegaciones del gateway como en Openbot.

**Fase 5 — Pulido, updater, release** (~2 semanas)
Tema Aurea definitivo + motion; iconos; updater transaccional con releases en GitHub; instalador Windows; `warp.nsh` opcional en el instalador. **Criterio:** `aurea-win-x64.exe` en Releases con auto-actualización.

**Fase 6 (futura, fuera de v1) — Edición sync:** deploy del edge, login, `crates/client` como visor móvil.

---

## 8. Riesgos y mitigaciones

| Riesgo | Nivel | Mitigación |
|---|---|---|
| Port TS→Rust del motor (~10k LOC) | **Alto** | Port módulo a módulo con los tests de Openbot como oráculo de comportamiento; **plan B de emergencia** (solo si el port se estanca): puente temporal — el CLI de Openbot ya trae el motor TS montado; el engine Rust lo consume vía HTTP y la UI no cambia. Se descarta como objetivo; queda documentado como paracaídas |
| Cliente TWS en Rust | Medio-alto | Crate `ibapi` madura pero protocolo delicado (qualify de contratos, OCA, grid de ticks); si falla, puente TS mínimo solo para TWS (`@stoqey/ib` en proceso aparte) |
| Curva Rust + GPUI | Medio | Codebase madura como referencia + `docs/` de Zeron + parity docs |
| Divergencia upstream Zeron | Medio-bajo | Re-sync periódico del core (engine/doc/sync/rpc) con merge; nuestras capas (trading, ui/trading) están aisladas |
| i18n inexistente en `crates/ui` | Bajo | Infra mínima es/en nueva, ~600 claves portadas |
| WARP sin hosts-pin (proxy mode) | Medio-bajo | Validación temprana en fase 4; fallback al pin de hosts de Openbot |
| Gateway de modelos free (externo) | Medio | Mismo modelo de riesgo que hoy en Openbot; el anillo lo gestiona |
| MT5 runtime bundling | Bajo | Script existente de Openbot reutilizable |

---

## 9. Puntos menores que se deciden sobre la marcha (no bloquean)

- Idioma por defecto de la UI: **español**, con inglés como opción (port de claves `trading.*`).
- Ubicación de datos de trading: `%APPDATA%\Aurea\…` (bots, pyshim, runtime MT5, DB).
- Exactitud del tema Aurea (dorado sobre glass oscuro) durante la fase 5.
- Nombre exacto de los RPCs de trading en el engine.
- Si el instalador ofrece Cloudflare WARP como hoy (`warp.nsh`).

---

## 10. Fuerza de trabajo: modelos free de OpenCode (trabajo en paralelo)

> Catálogo real del provider `opencode` (11 modelos free). **Un lane = un worktree = una sesión de OpenCode.**
> Los modelos de fortaleza desconocida reciben tareas **acotadas y verificables** con criterio de aceptación estricto;
> todo pasa por mi revisión antes de integrar. El código que toca dinero (órdenes, cierres, claims) **solo** lo escribe
> DeepSeek V4.1, GLM-5.3 o yo.
>
> **Ojo, dos rosters distintos que no confundir:** los modelos de ESTA sección **construyen** la app (build-time, sesiones
> de OpenCode con briefs). Los modelos que la app **usa en ejecución** como agentes son solo **DeepSeek v4.1 + GLM 5.3**
> vía harness opencode (§2.3) — que casualmente también figuran aquí como constructores, pero son usos distintos.

### 10.1 Roster y asignación

| Modelo | Fortaleza | Rol en Aurea | Lanes |
|---|---|---|---|
| **deepseek-v4.1-flash-free** | Porting preciso, código algorítmico largo, semántica delicada | Núcleo del motor y del cliente TWS | **A** (motor trading) · **D** (TWS Rust) |
| **glm-5.3-flash-free** *(yo)* | Orquestación, contratos, revisión, integración | Dueño del plan: contratos proto/RPC, wiring del engine, merges, QA de releases | **Orquestador** |
| **big-pickle** | Stealth de OpenCode, sólido y estable | Port completo del anillo WARP (engine + egress + RPCs) | **C** |
| **nemotron-3.5-super-vl-free** | **Visión** (VL) + código fuerte | QA visual por capturas del dashboard + port de fixtures + docs ilustradas | **E** |
| **longcat-2.5-free** | Contexto muy largo | Extracción de **specs de port módulo a módulo** desde el código de Openbot (alimenta A/B/C/D) | **F** |
| **iceman-free** | Mecánica precisa | Infra i18n (es/en) + port de schemas drizzle→rusqlite | **G** |
| **flint-40b-free** | Chores pequeños fiables | Fixtures, datos DEMO, escenarios mock | **H** |
| **beam-free** | Glue/infra | CI, build Windows, updater, instalador | **I** |
| **hy4-flash-free** | Iteración rápida | Pool de arreglos rápidos y widgets menores | **J** (pony express) |
| **mimo-v2.6-flash-free** | Rápido y barato | Lint, runner de tests, refactors menores | **J** |
| **inkling-2-small-free** | Texto corto | Changelog, checklist de renombrado visible, docs de soporte | **J** |

### 10.2 Reglas de paralelismo (sin superposiciones)

1. **Dentro de un lane: secuencial. Entre lanes: paralelo.** Un lane nunca toca archivos de otro.
2. **Contratos primero** (yo): tipos de trading en `crates/proto` + constantes RPC + mock con datos de muestra → desbloquea A, B, C, D en paralelo contra interfaces congeladas.
3. **Propiedad exclusiva de archivos**:
   - `crates/trading/**` → A · `crates/tws/**` → D · `crates/warp/**` → C
   - `crates/ui/src/trading/**` → B · `crates/ui/src/i18n/**` → G
   - `scripts/`, `.github/` → I · `crates/*/test/fixtures*` → H/E
   - `crates/engine/src/rpc.rs` (match arms) y wiring → **solo yo**, entre merges
4. **Integración centralizada**: cada lane entrega en su worktree; yo reviso, mezclo a `main`, Beam valida en CI. Conflicto → el lane rebasea contra `main`, nunca edita fuera de su zona.
5. **Sin secretos**: credenciales de brokers las maneja el módulo de credenciales del engine (mío). Los lanes no ven claves.

### 10.3 Grafo de dependencias

```
Fase 0: fork + quitas + rename visible (yo + Iceman + Inkling)
        │
        ▼
Contratos: proto types + RPC + mock (YO) ──┬──► A: motor trading (DeepSeek) ──┐
        │                                  ├──► B: UI dashboard (GLM-5.3) ────┤
        │                                  ├──► C: anillo WARP (Big Pickle) ──┤
F specs (LongCat) alimenta A/B/C/D         └──► D: TWS Rust (DeepSeek #2) ────┤
        │                                                                     │
E: fixtures → QA visual (Nemotron VL)   G: i18n (Iceman)   H: fixtures demo (Flint)
I: CI/updater (Beam)                    J: pony express (Hy4/MiMo/Inkling)      │
        │                                                                     │
        └────────────── ciclos de integración (YO revisa + mezcla + CI) ◄──────┘
                                                        │
                                              Fase 5 release (todos)
```

### 10.4 Colas de tareas por lane (entrega + criterio de aceptación)

**Lane A — Motor trading (DeepSeek V4.1) → `crates/trading/`**
- **A1** Pool de sistemas (`config.py`/`reglas.json`/`system.json`, patrón `NN_name`, `META`) + snapshot síncrono + store SQLite (8 tablas, migraciones). *Criterio: `snapshot()` reproduce los fixtures de Openbot; tests de sql portados.*
- **A2** Ciclo de vida de bots: `arrancar/arrancarNucleo` (spawn `pythonw bot.py`, adopción vía `panel.pid`), `pararBot/detener/detenerTodos` con pid-guard (CommandLine contiene `bot.py` en la carpeta del sistema), servidor-bots HTTP `/bot/*` con header por sistema. *Criterio: sección lifecycle de `trading-engine.test.ts` pasa en Rust.*
- **A3** Translator MT5 (`bots-mt5.ts`→Rust): semántica `ib_async` → DEAL/pending/SL/TP, OCA, matemática de lotes que preserva exposición + `mt5-ordenes`. *Criterio: `trading-bots-mt5.test.ts` (141 LOC) pasa; test OCA e2e con flag.*
- **A4** Ciclos de cierre: `closePosition` (cancelar órdenes propias primero; órdenes ajenas abortan; relectura; mercado opuesto; fallback dos límites OCA GTC ±4 ticks en grid de tick) + `closeAllPositionsInterno` + `panic` + **claims** (`cierre:<requestId>`, lock único, validez 5 min, replay 24 h, fail-closed con DB caída) + veredictos de cierre (los constructores son `Vestigio/Interrumpido/Ocupado/DbCaida`; el veredicto de cierre normal se construye inline en `cerrarUna` con su `message_key`). *Criterio: tests de claims/replay idénticos a Openbot.*
- **A5** Tasks programadas (`taskAdd/taskCancel`, expiry loop), links persistidos, `programas.ts` (detección TWS/MT5 en registro + `tasklist`), stub de `registro-ib` hacia el crate `tws`. *Criterio: detección encuentra instalaciones reales en Windows.*
- **A6** Superficie de acciones del engine (firma exacta de `state/system_start/system_stop/task_add/task_cancel/position_close/positions_close_all/panic/connect/disconnect/account_select`) para mi wiring RPC+MCP. *Criterio: contratos cumplen el doc de contratos (mío).*

**Lane B — UI dashboard (GLM-5.3) → `crates/ui/src/trading/`**
- **B1** Montaje de la surface Trading + nav strip de links (tabs por servidor) + entrada en sidebar. *Criterio: navegable con datos mock.*
- **B2** Strip de KPIs + tabla de posiciones contra `WatchTradingSnapshot` (stream), estados cargando/error/DEMO. *Criterio: paridad de datos con mock; sin bloqueos de UI.*
- **B3** Panel de sistemas (start/stop con gating de arranque masivo) + tasks. *Criterio: acciones llegan por RPC y se reflejan en snapshot.*
- **B4** Registro de veredictos traducidos (claves i18n de G) + panel de log. *Criterio: claves `trading.close/panic/log.*` renderizadas es/en.*
- **B5** Wizards de link (knock puertos 7497/7496, detección MT5) + pool select + editor de prompts (`DialogPrompto`). *Criterio: alta de cuenta IBKR paper y MT5 real.*
- **B6** Curva de equity + sparklines (primitivas de `crates/visual` o gpui propio). *Criterio: 60 fps con streaming.*
- **B7** Diálogo gate al cerrar la app + wiring de notificaciones. *Criterio: con bots vivos y posiciones abiertas, el cierre avisa (fail-closed si engine no responde 1,5 s).*
- *Criterio global: `prefers-reduced-motion` respetado; animaciones del kit de motion; cero colisiones con archivos de G.*

**Lane C — Anillo WARP (Big Pickle) → `crates/warp/`**
- **C1** Port de `ghost.ts`: control `warp-cli`, renovación por día UTC, memoria /64 (`opencode-warp-ips.log`), locks compartidos (`ghost.lock`, `ghost-eventos.log`, `ghost-dia.txt`, `ghost-historial.jsonl`, `ghost-pin.log`). *Criterio: suite de tests propia con locks simulados.*
- **C2** Egress: modo proxy de WARP + `HTTPS_PROXY` para el sidecar `opencode serve`; detección de rechazo vía retries del harness (los frames `session.status{retry}` ya se surfacen). *Criterio: rechazo simulado → rotación; tráfico del sidecar sale por el proxy.*
- **C3** RPCs `WarpStatus/WarpRotate/WarpHistory` (yo los wiring) + widget del anillo en titlebar (con B, tras B1). *Criterio: el widget refleja estado real.*

**Lane D — Cliente TWS Rust (DeepSeek, sesión 2) → `crates/tws/`**
- **D1** Conexión + persistencia + heartbeat + client ids 1000–9999 persistidos en `trading_meta`. *Criterio: reconexión limpia tras caída de TWS.*
- **D2** Qualify de contratos + lectura de posiciones/cuentas. *Criterio: paridad con `@stoqey/ib` en TWS paper.*
- **D3** Órdenes: cierre a mercado + fallback OCA GTC ±4 ticks en grid de tick; cancelación de órdenes propias; abort con órdenes ajenas. *Criterio: escenarios D3 validados contra TWS paper real.*
- **D4** Multi-cuenta: selección de cuenta activa + exposición agregada para el gate. *Criterio: el gate ve exposición de todas las cuentas antes de salir.*

**Lane E — QA visual y fixtures (Nemotron 3.5 Super VL)**
- **E1** Port de fixtures `bots-mt5` falsos (96..99) + `mt5-falso/MetaTrader5/`. *Criterio: A3 los consume.*
- **E2** QA visual por capturas en cada hito de B (checklist de paridad, anomalías de layout/contraste). *Criterio: informe por hito con capturas anotadas.*
- **E3** Docs con capturas (guía visual de fases para el repo).

**Lane F — Especificador (LongCat 2.5)**
- **F1** Specs de port módulo a módulo leyendo el código fuente de Openbot (briefs para A1–A5, D1–D4, C1–C2). *Criterio: cada spec lista comportamiento, edge cases y test de referencia.*
- **F2** Checklist completo de paridad Openbot→Aurea por endpoint RPC. *Criterio: 17 endpoints `experimental.trading.*` mapeados a RPCs destino.*
- **F3** Notas de divergencia upstream Zeron (qué zonas del core deben re-sincronizarse en el futuro).

**Lane G — i18n (Iceman)**
- **G1** Módulo `crates/ui/src/i18n` (es/en, carga previa, cambio en caliente, fallback). *Criterio: sin claves faltantes en ninguno de los dos idiomas (script verificador).*
- **G2** Port de ~400 claves `trading.*` (en 407 / es 398) + cadenas visibles renombradas a Aurea. *Criterio: B4/B7 consumen sin huecos.*

**Lane H — Fixtures y demo (Flint 40B)**
- **H1** Datos DEMO del snapshot (fallback del dashboard sin engine). *Criterio: B2 muestra DEMO fiel.*
- **H2** Escenarios mock (`ZERON_MOCK`) para e2e de UI. *Criterio: e2e de B corre contra mock.*
- **H3** Carpetas de bots de prueba nuevas (`NN_name` ficticias seguras). *Criterio: arrancar/parar en local sin dinero.*

**Lane I — Infra (Beam)**
- **I1** CI Windows: build, clippy, tests por crate, caché. *Criterio: verde por lane.*
- **I2** Config del updater transaccional + releases GitHub (canales stable/beta). *Criterio: artifact firmado y verificable desde CI.*
- **I3** Instalador Windows + `warp.nsh` opcional (oferta de WARP en setup, herencia de Openbot). *Criterio: instalación limpia en máquina virgen.*

**Lane J — Pony express (Hy4 flash / MiMo flash / Inkling)**
- Checklist de renombrado visible (título, splash, About, Settings, notificaciones, iconos) · changelog por ciclo · lint y fixes rápidos · widgets menores que B delegue · tabla de soporte. *Criterio: tareas de <1 h, una a la vez, sin tocar zonas A–I.*

### 10.5 Cadencia de integración

- **Modo estándar**: ciclo corto cada 2-3 días o al hito de lane. **Modo esfuerzo máximo 24/7**: ciclos de merge cada **4-6 horas** (ver §10.7).
- En ambos: yo reviso el diff del lane → merge ordenado (contratos → leafs → wiring → CI) → Beam da verde → changelog (Inkling).
- **Checklist de estado** en este plan (§11) se actualiza en cada ciclo.
- **Regla de conflicto**: rebase del lane contra `main`; el wiring del engine solo lo toco yo.
- **Paracaídas** (§8) sigue vigente: si el port de A se estanca, el puente con el CLI de Openbot se activa como emergencia, no como objetivo.

### 10.6 Arranque (tras tu aprobación del plan)

1. **Yo**: Fase 0 (fork + quitas + rename visible mínimo) + **contratos** (proto/RPC/mock) + carpeta `BRIEFS/` con 12 archivos de briefing (uno por lane, autocontenidos: contexto, entrega, criterio).
2. **Tú**: abres las sesiones de OpenCode con el modelo asignado a cada lane, pegando su brief (`@BRIEFS/A.md`, etc.).
3. **Yo**: integro, reviso y mantengo el board del plan.

### 10.7 Modo de esfuerzo máximo + cronograma 24/7

**Qué significa "esfuerzo máximo" (decisión tuya):**
1. Cada modelo trabaja con su **esfuerzo de razonamiento al máximo**, contexto completo y sin recorte de tokens ni presupuesto — sin downsampling, salidas largas completas.
2. **N sesiones simultáneas por modelo** donde el grafo lo permite: DeepSeek ×2 (A y D ya previstas), GLM-5.3 ×2 (B se parte en **B1-B4** y **B5-B7**, archivos distintos), y E/F/G/H/I/J cada una en su sesión.
3. **Protocolo de profundidad**: ninguna tarea se marca hecha sin (a) haber leído el spec de LongCat de ese módulo, (b) portado el test-oráculo correspondiente, y (c) pasado `cargo check + clippy + tests` de su crate. Análisis primero, código después.
4. Tokens/dinero/limites: sin restricción (decisión tuya). El único ritmo externo que queda es el gateway de modelos free, que gestiona el anillo (C).

**Cronograma hora a hora (arranque T=0, reloj continuo):**

| Horas | Qué ocurre | Hito de salida |
|---|---|---|
| 0-2 | Yo: fork + lanzo 3 sesiones de quitas en paralelo (harnesses+cuentas / git+cloud / preview+zgx+apps) · LongCat arranca F1 (specs) · Beam monta CI | fork vivo |
| 2-6 | Quitas consolidadas · Iceman rename visible + tema dorado base · Yo: contratos (proto/RPC/mock) · specs F1 llegan | Fase 0 verde: app abre |
| 6-8 | Contratos congelados → lanzo A+D (DeepSeek ×2), B (GLM ×2), C (Big Pickle) con brief+spec | 4 lanes grandes en marcha |
| 8-20 | Gran bloque de construcción · merges cada 4-6 h · E fixtures · H DEMO/mock · G infra i18n | **H20**: A1+A2, D1, B1-B3 integrados |
| 20-36 | A3 translator MT5 + A4 cierres/claims · B4-B6 · D2-D3 (TWS paper real) · C1-C2 WARP | **H36**: dashboard MT5 end-to-end (bots, cierres, veredictos) |
| 36-52 | A5-A6 · D3-D4 TWS completo + exposición multi-cuenta · B7 gate · C3 widget · wiring MCP | **H52**: paridad funcional esencial |
| 52-64 | Estabilización: QA visual (E2) sobre todo · fixes pony express · clippy/lint · i18n completa | **H64**: cero rojos |
| 64-72 | Fase 5: updater + instalador + release `aurea-win-x64.exe` + smoke test instalación limpia | **release** |

**Escenarios (honestos):**

| Escenario | Total | Condición |
|---|---|---|
| Óptimo | **~48-60 h (2-2,5 días)** | TWS/MT5/GPUI no ponen pegas, merges limpios, auto-merge aprobado de antemano |
| **Realista** | **~70-90 h (3-4 días)** | Fricciones normales de compilación y validación real |
| Prudente | ~5 días | Rework medio en TWS/claims + tu disponibilidad con huecos |

**Por qué 2 meses se comprimen ~10× (y qué NO se comprime):**
- Se comprime porque el port es mecánico con **tests como oráculo** (no hay diseño que inventar), 10+ lanes van en paralelo, no hay fatiga humana, y los specs de LongCat (F1) reducen el rework a la mitad.
- **No se comprime**: (1) los tiempos de compilación Rust del workspace GPUI (ui ≈ 255k LOC; cada iteración 1-3 min, cientos de iteraciones); (2) la validación con procesos reales — TWS paper, terminal MT5, WARP — que corren a su propio reloj (y los tests de claims duran 5 min *por diseño*); (3) los merges de dinero son seriales bajo mi revisión (seguridad, no velocidad); (4) **tus momentos humanos** — abrir sesiones, aprobar merges del núcleo y del instalador; si duermes sin auto-merge aprobado, el reloj se pausa contigo.

**Reglas para que el reloj no se pare (te las pido de antemano):**
1. **Auto-merge pre-aprobado** para lanes hoja (A1-A3, B1-B6, C, E-J) tras mi revisión: yo mezclo de noche sin preguntarte.
2. **Puerta de dinero siempre contigo**: primera orden real a mercado, primer cierre real, primera release — tú le das al botón.
3. Mantén TWS **paper** (no live) y un terminal MT5 demo disponibles desde la hora 8.
4. Si una sesión se cae, la reabres con el mismo brief (`@BRIEFS/X.md` es autocontenido — el lane no pierde contexto).

---

## 11. Estado actual

- ✅ Investigación completa (ambos repos mapeados a nivel de archivo/LOC).
- ✅ Decisiones de producto y alcance cerradas (§2).
- ✅ Este plan redactado.
- ✅ Fuerza de trabajo paralela diseñada (§10) con el catálogo real de modelos free de OpenCode.
- ✅ **Modo de esfuerzo máximo + cronograma 24/7 definido (§10.7)**: escenario realista ~3-4 días de reloj continuo; óptimo 2-2,5 días.
- ⬜ **Tu validación de este plan** → desbloquea Fase 0.
- ⬜ Código: **sin tocar** (carpeta de trabajo vacía a propósito; las copias de análisis de ambos repos viven en la carpeta temporal, nunca en el proyecto).

---

## Anexo A — Notas de investigación verificada (base del plan)

Hechos comprobados leyendo el código de ambos repos, en los que se apoyan las decisiones:

**Zeron (`zeronsh/zeron`, MIT):**
- Arquitectura: engine Rust daemon (headed/headless, `managed.rs`) + UI GPUI como viewport + edge TypeScript (Cloudflare Worker + Durable Objects: SessionRoom/chat2, DeviceRoom, registry rooms, R2 attachments, WorkOS auth) + `crates/client` como visor sin engine.
- Persistencia: sesiones = Loro CRDT (`crates/doc`), local-first con `WorkspaceScope::{Local,Synced}`; el perfil Local funciona sin cuenta.
- **Harness opencode nativo** (`crates/harness/src/opencode/`): habla el protocolo HTTP+SSE de `opencode serve` (generaciones 1.18 y 2.x), descubrimiento de modelos vía `/api/model` y `/api/agent`, permisos, questions, steering, retries visibles (`session.status{retry}`). Es la vía a los modelos free.
- Trait `Harness` limpio y abierto (id/models/run→AgentEvent stream/steering/requestInput); `HarnessId` extensible.
- UI: shell (sidebar/transcript/composer), 30 familias de tema con claro+oscuro y frost/opaque, kit de motion (`motion.rs`), markdown streaming, `crates/visual` con primitivas nativas de charts/tablas/canvas (Luau por encima, pero los nodos son Rust reutilizables).
- zgx (`apps/zgx`) es un segundo frontend Luau con sistema de plugins (pages.add, panel.tabs, servicios headless, consentimiento ed25519) — se quita en v1, aislado y re-introducible.
- RPC: ~230 métodos; añadir uno = constante + match arm en `crates/engine/src/rpc.rs` (+ `forwardable()` para multi-device).
- Update transaccional (`crates/update`): swap atómico con rollback y watchdog.

**Openbot-v2 (`ikermiranda98/Openbot-v2`, MIT, fork de OpenCode V2):**
- Motor trading: `packages/core/src/trading.ts` (4.445 LOC, Effect) + `trading/*` (servidor-bots HTTP 127.0.0.1 `/bot/*` con header por sistema; translator MT5 912 LOC; PUENTE_PY bridge JSON-stdio; pyshim `ib_async` 576 LOC que se inyecta en PYTHONPATH; programas/detección; registro-ib con `@stoqey/ib` parcheado).
- Claims de cierre: tabla `trading_close`, lock único por contrato, validez 5 min, replay de veredicto 24 h, fail-closed con DB caída. Veredictos = claves i18n `trading.*` (en 407 / es 398 claves × 68 locales) — **no son LLM**.
- Bots = carpetas Python del usuario (`NN_name/bot.py`, `ib_async`), spawn/adopt/kill con pid-guard; NO son agentes.
- Chat del desk: sesión con metadata `opencode.trading`, contexto permanente via `InstructionEntry`, tools `trading.*` gated a esa sesión (siempre piden permiso, sin "always-allow").
- DB: 8 tablas drizzle/SQLite (`trading_task/trade/link/meta/account/bot_order/bot_fill/close`), 9 migraciones.
- UI: `packages/app/src/trading/` ~4.700 LOC SolidJS (route.tsx 3.093) en `/trading/:serverKey/:id`; motor.ts poll 3 s con fallback DEMO.
- WARP ring: `ghost.ts` (733) + `ghost-egress.ts` (211, rutea el tráfico LLM) + UI del anillo (`usage/free-daily-ring*`) + elevación NSIS (`warp.nsh`).
- Gate de cierre: `desktop/src/main/windows/trading-close.ts` consulta el snapshot (timeout 1,5 s) y avisa con bots vivos o posiciones abiertas; fail-closed.
- Tests-oráculo: `trading-engine.test.ts` (922), `trading-bots-mt5.test.ts` (141 + e2e OCA), `tool-trading.test.ts` (99), fixtures `bots-mt5/96..99` y `mt5-falso/`.
- Superficie API: 17 endpoints `experimental.trading.*` (protocol/server) + tools + `experimental.ghost.*`.

## Anexo B — Catálogo build-time: los 11 modelos free de OpenCode

Fuente: catálogo del provider `opencode` consultado hoy (todos activos, coste 0):

| Modelo | Release | Lane |
|---|---|---|
| hy4-flash-free | 2026-11-16 | J |
| iceman-free | 2026-11-06 | G |
| inkling-2-small-free | 2026-11-05 | J |
| longcat-2.5-free | 2026-11-02 | F |
| flint-40b-free | 2026-10-21 | H |
| deepseek-v4.1-flash-free | 2026-10-17 | A · D |
| nemotron-3.5-super-vl-free | 2026-10-15 | E |
| beam-free | 2026-10-05 | I |
| mimo-v2.6-flash-free | 2026-09-22 | J |
| glm-5.3-flash-free *(orquestador, yo)* | 2026-08-26 | Contratos · wiring · merges |
| big-pickle | 2025-10-17 | C |

*(Los modelos de ejecución dentro de la app son solo DeepSeek v4.1 + GLM 5.3 filtrados en el catálogo del harness opencode — §2.3.)*
