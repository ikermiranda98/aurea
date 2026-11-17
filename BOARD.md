# BOARD — Estado operativo del orquestador Aurea

> **LEEME PRIMERO SI ERES UN MODELO NUEVO:** este archivo es la memoria operativa del orquestador.
> Compáctalo mentalmente en 30 segundos y sigue. Plan maestro: `PLAN.md`; briefs: `BRIEFS/`;
> specs de port: `SPECS/`; contratos: `SPECS/CONTRACTS.md` (borrador v0.1). NUNCA credenciales en ficheros.
>
> Última actualización: 2026-11-17 ~22:30 · **Fase 0 cerrando, hora ~2,5**

## Identidad del proyecto
- **Aurea** = fork de `zeronsh/zeron` (Rust+GPUI) → app de trading de escritorio. Motor portado de Openbot-v2 (TS) a Rust.
- Repo: **https://github.com/ikermiranda98/aurea** (privado) · `main` trackeada · push funciona.
- Local: `C:\Users\WDAGUtilityAccount\Desktop\zeron` · git `C:\Users\WDAGUtilityAccount\tools\mingit\cmd\git.exe` · gh `C:\Users\WDAGUtilityAccount\tools\gh\bin\gh.exe`
- Referencia Openbot (solo lectura): `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main`

## DECISIÓN CLAVE (2026-11-17 ~22:20): quitas §4.2 en modo ligero
Los 2 subagentes de quitas murieron por cupo free (DeepSeek y GLM). La quita literal (engine+UI+rpc.rs 7.409 líneas/432 arms) es ~1,5 días de cirugía y NO está en la ruta crítica.
**Aplicado:** eliminados `apps/zgx`, `apps/card-web`, `apps/landing`, `apps/www-redirect`, `crates/preview-label` (sin ondulación de compilación). **Restaurado al baseline:** `crates/engine`, `crates/harness`, `crates/preview` (el árbol vuelve a ser compilable).
**Pendiente (hardening post-v1, documentar en release):** quitas profundas de engine/UI/RPC (repos/git/cloud/plugins/agent_accounts) — se harán con CI y compilador en verde, no a ciegas.

## Git
MinGit lleva GCM de sistema (diálogo fantasma). Global config arreglada. Fallback infalible si algo pide credenciales:
```powershell
$tok = & gh auth token; $b64 = [Convert]::ToBase64String([Text.Encoding]::ASCII.GetBytes("ikermiranda98:$tok"))
git -c credential.helper= -c "http.extraheader=AUTHORIZATION: basic $b64" push
```

## Toolchain
- `cargo 1.100.0` + `rustc 1.100.0` ✅ instalados (2026-11-17 22:15).
- **VS Build Tools MSVC AÚN instalando** (Windows SDK desde 20:38; shell bg `sh_1eaed3ada001FtsBXX1YfJ5VV2`, log `C:\Users\WDAGUtilityAccount\tools\toolchain-setup.log`). Sin `cl.exe`/`link.exe`, el check fallará en deps nativas.

## Estado actual
| Qué | Estado |
|---|---|
| Quitas ligeras | ✅ (apps + preview-label) |
| `cargo check --workspace` | 🔄 primer intento lanzado (bg) |
| Contratos | 📝 borrador v0.1 en `SPECS/CONTRACTS.md` — se congela con check verde |
| Specs | ✅ 12 en repo |
| GitHub | ✅ baseline+specs (falta push del estado quitas-lite) |

## Pendientes del USUARIO
1. **~23:00 TWS re-login** tras su reinicio (credenciales solo en chat). TWS API ✅ en 7497 paper.
2. MT5: más adelante ☑ Herramientas→Opciones→Asistentes Expertos→Permitir trading automático.
3. Aviso cupo free: si un modelo muere con "Free usage limit reached", cambiar de modelo (big-pickle/GLM), decir «sigue» — BOARD lo cubre.

## Cola del orquestador
1. `cargo check --workspace` verde → commit `fase 0: quitas ligeras` + push.
2. Contratos: `crates/proto/src/trading.rs` + `warp.rs` + constantes en `crates/rpc/src/lib.rs` + mock en `crates/trading/src/mock.rs` + match arms `WatchTradingSnapshot` → check verde → congelar CONTRACTS.md.
3. Lanzar lanes como subagentes con modelo que tenga cupo: A (motor), B×2 (UI), C (WARP), D (TWS). Briefs autocontenidos en `BRIEFS/`. Si el cupo free sigue muerto: construir yo las piezas críticas en orden A1→A2→D1→B1.
4. Wiring RPC/MCP: SOLO orquestador. Commits: SOLO orquestador.
5. TWS solo paper (7497). PROHIBIDO live (7496). Puerta de dinero SIEMPRE con el usuario.

## Reglas invariables
- Solo renombrado visible a "Aurea"; interno `zeron-*`/`ZERON_*` intacto. `crates/harness/src/opencode/` sagrado.
- Sync dormido en código (v1 sin móvil ni login). Plugins/zgx fuera de v1. UI español por defecto.
- Auto-merge pre-aprobado lanes hoja; dinero siempre con el usuario. Credenciales NUNCA en ficheros.

## Bitácora
- 20:09 fork + commit base `f870dfb`. MinGit + gh portables. BRIEFS completos.
- 21:00-21:30 gh auth OK (2 device flows) · repo `aurea` creado · TWS API habilitada (7497).
- 21:35 Lane F: 12 specs + hallazgos (8 tablas, patch @stoqey splice-bug, magic MT5). PLAN corregido.
- 21:45 baseline en GitHub (94 MiB, 3 commits). Quitas 1ª murió (cupo DeepSeek).
- 22:15 quitas 2ª murió (cupo GLM) — había borrado 62 ficheros engine y 137 harness.
- 22:20 DECISIÓN: quitas ligeras; restore engine/harness/preview al baseline; apps fuera. cargo/rustc YA instalados.
- 22:30 primer `cargo check --workspace` lanzado. Contratos v0.1 escritos.
