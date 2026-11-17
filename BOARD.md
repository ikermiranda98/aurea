# BOARD — Estado operativo del orquestador Aurea

> **LEEME PRIMERO SI ERES UN MODELO NUEVO:** este archivo es la memoria operativa del orquestador.
> Compáctalo mentalmente en 30 segundos y sigue. Plan maestro: `PLAN.md`; briefs: `BRIEFS/`;
> specs de port: `SPECS/`; contratos: `SPECS/CONTRACTS.md` (v0.3). NUNCA credenciales en ficheros.
>
> Última actualización: 2026-11-17 ~21:35 · **Fase 0 cerrando, contratos escritos**

## Identidad del proyecto
- **Aurea** = fork de `zeronsh/zeron` (Rust+GPUI) → app de trading de escritorio. Motor portado de Openbot-v2 (TS) a Rust.
- Repo: **https://github.com/ikermiranda98/aurea** (privado) · `main` trackeada · push funciona.
- Local: `C:\Users\WDAGUtilityAccount\Desktop\zeron` · git `C:\Users\WDAGUtilityAccount\tools\mingit\cmd\git.exe` · gh `C:\Users\WDAGUtilityAccount\tools\gh\bin\gh.exe`
- Referencia Openbot (solo lectura): `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main`

## DECISIÓN CLAVE (2026-11-17 ~22:20): quitas §4.2 en modo ligero
Los 2 subagentes de quitas murieron por cupo free (DeepSeek y GLM). La quita literal (engine+UI+rpc.rs 7.409 líneas/432 arms) es ~1,5 días de cirugía y NO está en la ruta crítica.
**Aplicado:** eliminados `apps/zgx`, `apps/card-web`, `apps/landing`, `apps/www-redirect`.
**Restaurado al baseline:** `crates/engine`, `crates/harness`, `crates/preview`, `crates/preview-label` y `Cargo.toml` (el árbol vuelve a ser compilable).
**Pendiente (hardening post-v1):** quitas profundas de engine/UI/RPC (repos/git/cloud/plugins/agent_accounts).

## Git
MinGit lleva GCM de sistema (diálogo fantasma). Global config arreglada (`helper =` vacío + `!gh auth git-credential`). Fallback infalible:
```powershell
$tok = & gh auth token; $b64 = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("ikermiranda98:$tok"))
git -c credential.helper= -c "http.extraheader=AUTHORIZATION: basic $b64" push
```

## Toolchain
- `cargo 1.100.0` + `rustc 1.100.0` ✅ · MSVC 14.44 con `link.exe` ✅ (existe, sin registrar aún).
- **Windows SDK AÚN instalando** (desde 20:38; va por el paquete ~24; falta `C:\Program Files (x86)\Windows Kits\10\Lib\...\kernel32.lib`).
- **Trampa:** sin el SDK completo, `cargo check` falla en build scripts con `linker link.exe not found` aunque link.exe exista.
- Validación sin enlazador disponible: `rustfmt --emit stdout <fichero>` parsea sintaxis; `cargo metadata --no-deps` valida manifiestos.

## Estado actual
| Qué | Estado |
|---|---|
| Quitas ligeras | ✅ (solo apps legacy) |
| `cargo check --workspace` | ⛔ bloqueado por Windows SDK (MSIs en curso: `WindowsSDKDesktopLibraries` aún pendiente) |
| Manifiestos | ✅ `cargo metadata --no-deps` exit 0 |
| Contratos proto trading/warp | ✅ escritos y corregidos contra el schema (v0.3) |
| Constantes RPC | ✅ 18 trading + 3 warp en `crates/rpc/src/lib.rs` |
| Brazos engine RPC | ✅ 20 métodos + 3 WARP en `engine/rpc.rs` |
| Crate `crates/trading` | ✅ mock vivo cada 2 s + acciones simuladas interactivas + `pool.rs` (A1) |
| Lane B (UI) | ✅ `crates/ui/src/trading/{model,surface,mod}.rs` + `Route::Trading` + botón Mesa en footer |
| Sintaxis | ✅ rustfmt OK en todos los ficheros tocados (sin commitear) |
| Specs | ✅ 12 en repo |

### Lane B — lo que hay
- `model.rs`: `Phase`, `Confirm`, `Kpis`, ordenaciones, formato español (`10.085,30 $`, `+0,15 %`), `clock/short_clock/relative`, `spark_bars` + 6 tests puros.
- `surface.rs`: header (fase/DEMO/versión + Cerrar todo + Pánico), barra de confirmación para toda acción de dinero, strip de errores, KPI strip, sparkline de equity, tabla de posiciones con cierre confirmado, tarjetas de sistemas (start/stop) y tareas (cancelar), registro del motor. Stream `WatchTradingSnapshot` vía `EngineHandle`.
- `shell.rs`: `Route::Trading`, `NavEntry::Trading`, `open_trading` (historial back/forward), rama en `render_main` (lazy entity), botón `open-trading` (icono WALLET) junto a Pull Requests en el footer, `ZERON_OPEN_ROUTE=trading` para pruebas.
- Pendiente menor: entrada destacada arriba del árbol de sesiones (hoy en footer; el árbol lleva FLIP keyed rows y se toca tras el check verde).

## Pendientes del USUARIO
1. **~23:00 TWS re-login** tras su reinicio (credenciales solo en chat). TWS API ✅ en 7497 paper; 7496 cerrado.
2. MT5: más adelante ☑ Herramientas→Opciones→Asistentes Expertos→Permitir trading automático.
3. Aviso cupo free: si un modelo muere con "Free usage limit reached", cambiar de modelo, decir «sigue» — BOARD lo cubre.

## Cola del orquestador
1. Al terminar el SDK: `cargo check --workspace` → iterar a verde → commit `fase 0: contratos + mock interactivo` → push → congelar CONTRACTS.md v1.
2. Lane A (motor real) en orden A1→A2; D1 (cliente TWS); luego B (dashboard GPUI sobre el mock ya vivo).
3. Wiring RPC/MCP: SOLO orquestador. Commits: SOLO orquestador.
4. TWS solo paper (7497). PROHIBIDO live (7496). Puerta de dinero SIEMPRE con el usuario.

## Reglas invariables
- Solo renombrado visible a "Aurea"; interno `zeron-*`/`ZERON_*` intacto. `crates/harness/src/opencode/` sagrado.
- Sync dormido en código (v1 sin móvil ni login). Plugins/zgx fuera de v1. UI español por defecto.
- Auto-merge pre-aprobado lanes hoja; dinero siempre con el usuario. Credenciales NUNCA en ficheros.

## Bitácora
- 20:09 fork + commit base `f870dfb`. MinGit + gh portables. BRIEFS completos.
- 21:00-21:30 gh auth OK · repo `aurea` creado · TWS API habilitada (7497).
- 21:35 Lane F: 12 specs + hallazgos (8 tablas, patch @stoqey splice-bug, magic MT5).
- 21:45 baseline en GitHub (94 MiB, 3 commits). Quitas 1ª murió (cupo DeepSeek).
- 22:15 quitas 2ª murió (cupo GLM) — había borrado 62 ficheros engine y 137 harness.
- 22:20 DECISIÓN: quitas ligeras; restore engine/harness/preview al baseline; apps fuera. cargo/rustc YA instalados.
- 22:30 primer `cargo check` (falla por `link.exe`); restore preview-label + Cargo.toml baseline (`e5cd907`).
- 21:30 (reloj VM) barrido completo del schema: corregidos `Link.connected`, `mt5_login` número, `IbkrPort.open`, `Programs`, `PoolSelectResult.systems`, `TaskAddInput` (`systemIDs`/`atTime` "HH:MM"). Proto v0.3 + 20 brazos RPC + mock interactivo (start/stop/close/close-all/panic/tasks/links/detect/pool). rustfmt OK. Check pendiente del SDK.
