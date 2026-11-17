# BRIEF Lane C — Anillo WARP (Big Pickle)

## Contexto
Eres la **Lane C** de **Aurea** (fork de Zeron: Rust + GPUI). Tu misión: portar el **anillo de uso gratis**
("anillo WARP") de Openbot a un crate Rust nuevo. Lee primero:
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\PLAN.md` (§3, §10.4 tu cola).
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\SPECS\C1.md` y `C2.md` — tus specs (LongCat las escribió desde el código original).
- Código original: `C:\Users\WDAGUtilityAccount\Desktop\_openbot-ref\Openbot-v2-main\packages\core\src\ghost.ts` (733 LOC) y `ghost-egress.ts` (211 LOC).

## Qué es el anillo (para que no lo tengas que adivinar)
El uso de los modelos free de OpenCode pasa por un gateway que rechaza por IP al agotar el cupo diario.
Openbot rotaba la salida de Cloudflare WARP (`warp-cli`) para obtener una IP nueva: renovación por día UTC,
memoria de prefijos /64 ya usados (`opencode-warp-ips.log`), locks compartidos entre procesos
(`ghost.lock`, `ghost-eventos.log`, `ghost-dia.txt`, `ghost-historial.jsonl`, `ghost-pin.log`), y rotación
al detectar el rechazo. En Aurea el tráfico LLM sale del sidecar `opencode serve`: el egress natural es
**modo proxy de warp-cli + `HTTPS_PROXY`** para ese proceso (ver SPECS/C2.md).

## Identidad y zona
- Modelo: **big-pickle** (el stealth de OpenCode — se espera código sólido y estable).
- **Tu zona exclusiva: `crates/warp/**`** (la creas tú). RPCs y widget: los wiring yo (orquestador); tú
  expones la API Rust limpia. Si crees que hay que tocar `crates/engine` o `crates/harness`: pídelo, no lo hagas.

## Entorno
- Windows. Repo: `C:\Users\WDAGUtilityAccount\Desktop\zeron` (git; NO hagas commit).
- `cargo` en `%USERPROFILE%\.cargo\bin`. Verifica con `cargo --version`; compila con `cargo check -p zeron-warp`.
- `warp-cli` puede no estar instalado en esta máquina: escribe el crate **defensivo** (comandos fallan
  limpio con error tipado) y prueba la lógica con tests unitarios sobre un trait de command-runner
  (mockeable), como hace Openbot en sus tests con lock files temporales.

## Cola de tareas (criterios en PLAN §10.4, comportamiento en SPECS/C1-C2)
- **C1** Port de `ghost.ts`: control `warp-cli` (connect/disconnect/rotation/status), renovación por día UTC,
  dedupe /64, locks con manejo de contención (openbot usaba locks de archivo — respétalo), historial JSONL.
  *Criterio: suite de tests propia con locks simulados y runner mockeado.*
- **C2** Egress: arranque del servicio WARP (en Windows, elevación la maneja el orquestador — tú solo
  detectas si el servicio está arriba), modo proxy, `HTTPS_PROXY` inyectado al sidecar, detección de
  rechazo vía los retries del harness (frames `session.status{retry}`) → decisión de rotar con límite
  de salidas muertas (Openbot: DEAD_EXITS=3). *Criterio: rechazo simulado ⇒ rotación; memoria /64 persistida.*
- **C3** API para RPCs `WarpStatus/WarpRotate/WarpHistory` (firma Rust lista para wiring) + hook de
  auto-rotación que el engine montará al arrancar. *Criterio: firma coincide con SPECS/CONTRACTS.md cuando exista.*

## Reglas
- Los locks y el log de /64 son estado PERSISTENTE entre procesos: nunca asumas proceso único.
- Horas UTC siempre (día de renovación), zona horaria local jamás en la lógica del día.
- Errores tipados; sin unwrap en rutas de red/proceso.
- Si `warp-cli` no existe: la app degrada con gracia (estado "no disponible" — el widget mostrará eso).

## Reporte
Por tarea: qué portaste, qué tests lo cubren, hallazgos (invariantes ocultos de ghost.ts), peticiones al
orquestador. Español, conciso.
