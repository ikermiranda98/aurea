# BRIEF Lane B — Dashboard de trading GPUI (GLM-5.3)

## Contexto
Eres la **Lane B** de **Aurea** (fork de Zeron: Rust + GPUI + engine daemon) — app de trading nativa de
Windows. Construyes la **surface de trading** de la UI GPUI. Lee primero:
- `C:\Users\WDAGUtilityAccount\Desktop\zeron\PLAN.md` (§3, §10.4 tu cola).
- Código base que vas a extender: `crates/ui/src/` — shell (`shell.rs`, `shell/`), pickers, theme, motion
  (`motion.rs`). Antes de escribir una línea, lee `crates/ui/src/state.rs` (cómo la UI consume el engine
  vía RPC) y 2-3 componentes existentes para clonar su estilo.

## Identidad y zona
- Modelo: **glm-5.3-flash-free**. Puede haber **otra sesión GLM hermana** ejecutando la otra mitad de la
  cola — respetad el reparto: esta sesión hace **B1-B4**, la hermana **B5-B7**.
- **Tu zona exclusiva: `crates/ui/src/trading/**`** (la creas tú). Si necesitas tocar `crates/ui/src/pickers.rs`,
  `shell/sidebar*` o `crates/proto`/`crates/rpc`: NO lo edites — pídelo en tu reporte al orquestador.

## Entorno
- Windows. Repo: `C:\Users\WDAGUtilityAccount\Desktop\zeron` (git; NO hagas commit).
- `cargo check -p zeron-ui` para iterar (compilado lento: agrupa cambios antes de compilar).
- Tema: usa los tokens semánticos de `crates/theme`/`crates/ui/src/theme.rs` — nada de colores hardcodeados.
  Animaciones: solo el kit de `motion.rs` (fade-in/menu-in/dialog-in/200ms eases). `prefers-reduced-motion` se
  respeta solo.

## GATE
Espera a que exista `C:\Users\WDAGUtilityAccount\Desktop\zeron\SPECS\CONTRACTS.md` (contratos proto/RPC
congelados + datos mock). Con eso construyes contra `WatchTradingSnapshot` y las acciones sin engine real:
el mock del orquestador alimenta tu UI desde el minuto uno.

## Cola de tareas (detalles en PLAN §10.4)
- **B1** Montaje de la surface Trading: nueva vista con entrada en el sidebar (grupo TRADING arriba de las
  sesiones), nav strip de links (tabs por servidor/cuenta, estilo del nav de Zeron). *Criterio: navegable con datos mock.*
- **B2** Strip de KPIs (equity, P&L día, exposición, bots activos) + tabla de posiciones (stream
  `WatchTradingSnapshot`); estados cargando/error/DEMO; virtualización si la tabla crece. *Criterio: paridad con mock; cero bloqueos de UI.*
- **B3** Panel de sistemas: lista de sistemas `NN_name` con estado del bot, start/stop (acciones RPC),
  gating de arranque masivo (confirmación si >N sistemas), tasks programadas. *Criterio: acciones visibles en el siguiente snapshot.*
- **B4** Registro de veredictos: lista de eventos del motor con textos i18n (`trading.close.*`,
  `trading.panic.*`, `trading.log.*` — claves que provee la lane G en `crates/ui/src/i18n`; mientras no
  estén, usa las claves en inglés como fallback). *Criterio: es/en renderizan.*
- (Hermana:) **B5** wizards de link, **B6** curva de equity/sparklines, **B7** diálogo de gate al cerrar la app.

## Estética
Look Zeron: oscuro, glass, hairline borders, Geist/Geist Mono. El acento dorado de Aurea lo aplica el
orquestador en la familia de tema; tú usa tokens, no hex. Densidad tipo trading: tablas compactas,
monoespaciado para números (los números mandan el layout, el color es pintura).

## Reglas
- Nada de bloquear el hilo de UI: todo consumo de engine via RPC async del estado.
- Un componente = un archivo en `crates/ui/src/trading/`, nombres claros.
- Si algo del shell necesita cambio estructural (nuevo slot, nueva ruta), pídelo — no lo fuerces.

## Reporte
Por tarea: qué construiste, dónde, qué contratos consumiste, capturas de criterio (qué mostrará el QA
visual de la lane E), y peticiones al orquestador. Español, conciso.
