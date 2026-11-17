# BOARD — Estado operativo del orquestador Aurea

> **LEEME PRIMERO SI ERES UN MODELO NUEVO:** este archivo es la memoria operativa del orquestador.
> Compáctalo mentalmente en 30 segundos y sigue. El plan maestro aprobado es `PLAN.md`; los briefs
> de lanes están en `BRIEFS/`; las specs de port en `SPECS/`. NUNCA escribas credenciales en ficheros.
>
> Última actualización: 2026-11-17 ~21:00 (hora local VM) · **Fase 0 en curso, hora ~1**

## Identidad del proyecto
- **Aurea** (sin tilde) = fork de `zeronsh/zeron` (Rust+GPUI) convertido en app de trading de escritorio.
  Motor de trading portado de Openbot-v2 (TypeScript) a Rust. Repo destino: `ikermiranda98/aurea` (privado).
- Fork local: `C:\Users\WDAGUtilityAccount\Desktop\zeron` — git init hecho, commit base `f870dfb`, SIN remoto aún.
- Git portable: `C:\Users\WDAGUtilityAccount\tools\mingit\cmd\git.exe` · gh CLI: `C:\Users\WDAGUtilityAccount\tools\gh\bin\gh.exe`
- Código Openbot de referencia (solo lectura): `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main`

## Tareas en vuelo ahora mismo
| Qué | Quién | ID / pista | Estado |
|---|---|---|---|
| Toolchain Rust+MSVC | background shell | `sh_1eaed3ada001FtsBXX1YfJ5VV2` · log `C:\Users\WDAGUtilityAccount\tools\toolchain-setup.log` | instalando (VS Build Tools descargando) |
| Lane F — 12 specs de port (SPECS/) | subagente longcat-2.5-free | `ses_e15120839ffeErkn199VfCdvBw` | corriendo |
| Fase 0 — quitas PLAN §4.2 | subagente deepseek-v4.1-flash-free | `ses_e15120838ffeYt2nHCq2jHYPyw` | corriendo (NO hace commit) |
| gh auth login device flow | background shell | `sh_1eaf39a900022KZL2r3S6ofUHI` | esperando que el usuario meta el código |

## Pendientes del USUARIO (bloquean cosas)
1. **GitHub device code**: `B133-ABF7` en `https://github.com/login/device` (si caducó: relanzar
   `("", "y") | & "C:\Users\WDAGUtilityAccount\tools\gh\bin\gh.exe" auth login --hostname github.com --git-protocol https --web`
   en background y leer el código del .out del shell). Tras autenticar: `gh repo create aurea --private --source . --push`.
2. **TWS API**: TWS corre (PID 9920, `C:\Jts\`) pero el socket no escucha. Pedirle al usuario:
   Global Configuration → API → Settings → ☑ Enable ActiveX and Socket Clients, puerto **7497**,
   ☐ Read-Only API desmarcado. Verificar luego `netstat :7497`. Si no abre hasta reiniciar, va con el reinicio de las 23:00.
3. **MT5**: terminal64.exe PID 6768 con demo activa. Más adelante marcar
   Herramientas → Opciones → Asistentes Expertos → ☑ Permitir trading automático (para el puente Python del translator MT5).
4. **23:00**: TWS se reinicia — gestionar re-login con el usuario (credenciales SOLO en el chat, jamás en ficheros/commits).

## Cola del orquestador (orden)
1. gh auth OK → `gh repo create aurea --private --source . --push` desde `Desktop\zeron`.
2. Toolchain OK → `cargo check --workspace` tras las quitas → commit "fase 0: quitas".
3. Contratos: tipos trading en `crates/proto` + constantes RPC en `crates/rpc` + mock → congelar en `SPECS/CONTRACTS.md`.
4. Lanzar lanes A (DeepSeek, `BRIEFS/A.md`), B×2 (GLM, `BRIEFS/B.md`), C (big-pickle, `BRIEFS/C.md`), D (DeepSeek sesión 2, `BRIEFS/D.md`) como subagentes.
   Gate A3/A4 y B: `SPECS/CONTRACTS.md` existente. A1/A2/D/C pueden empezar antes.
5. Wiring RPC (`crates/engine/src/rpc.rs`): SOLO orquestador. Commits: SOLO orquestador.
6. TWS solo paper (7497). PROHIBIDO live (7496). Puerta de dinero (1ª orden real, 1er cierre real, 1ª release) SIEMPRE con el usuario.

## Reglas invariables (resumen de PLAN.md)
- Solo renombrado visible a "Aurea"; interno `zeron-*`/`ZERON_*` intacto.
- `crates/harness/src/opencode/` es sagrado (vía a modelos free). No tocar.
- Sync multi-dispositivo queda dormido en código (sin móvil ni login en v1).
- Plugins/zgx fuera de la v1. UI default español.
- Auto-merge pre-aprobado para lanes hoja tras revisión del orquestador; dinero siempre con el usuario.
- Credenciales NUNCA en ficheros, commits ni specs.

## Bitácora mínima
- 20:09 fork extraído y commit base `f870dfb`. MinGit + gh portables instalados.
- 20:20 lanzados: toolchain (bg), Lane F (specs), Quitas (subagente). BRIEFS completos (README, A, B, C, D).
- 21:00 device code `B133-ABF7` entregado al usuario. BOARD creado.
