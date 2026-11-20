# Aurea

Una mesa de trading de escritorio para Windows: sigue tu cuenta, tus posiciones y tus sistemas en una sola página, y pregúntale a un agente de IA desde un chat junto a la mesa.

*[English](README.md) | Español*

## Qué hace

- **Mesa de trading** — equity, P&L del día, exposición, curva de equity, posiciones abiertas, tus sistemas (bots) con arrancar/parar, el registro del motor y las tareas programadas. **Cerrar todo** y **Pánico** siempre a un clic.
- **Conexiones** — Interactive Brokers TWS (solo cuenta paper, puerto 7497) y MetaTrader 5. Sin conexión, la mesa funciona en modo demo con datos de ejemplo.
- **Chat de la mesa** — un agente (OpenCode) en el cajón izquierdo que conoce la mesa. Su system prompt se edita en Ajustes → Chat.
- **Navegador y terminal** — un panel lateral, siempre disponible, para tener un gráfico, la web del bróker o una consola junto a la mesa.
- **Dictado** — háblale al chat; la voz se transcribe en tu ordenador.

Todo se queda en tu ordenador: sin cuenta y sin nube. Las acciones con dinero real siempre te piden confirmación.

## Instalación

Todavía no hay versiones publicadas. Genera el instalador desde el código (Windows, Rust e [Inno Setup 6](https://jrsoftware.org/isinfo.php)):

```powershell
.\scripts\package-windows.ps1 -ReleasesUrl https://github.com/ikermiranda98/aurea/releases
```

El instalador queda en `target\package\aurea-<versión>-windows-x86_64-setup.exe`. Para ejecutarla mientras desarrollas:

```powershell
cargo run -p zeron
```

## Créditos

Aurea es un fork de [Zeron](https://github.com/zeronsh/zeron) (MIT). Los nombres internos (`zeron.exe`, los crates `zeron-*`, las variables `ZERON_*`) se mantienen. En [ARCHITECTURE.md](ARCHITECTURE.md) se explica cómo está montada.

Publicado bajo la [licencia MIT](LICENSE).
