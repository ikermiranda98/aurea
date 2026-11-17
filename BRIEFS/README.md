# BRIEFS — Áurea (Índice)

Un archivo por lane. Cada brief es **autocontenido**: se pega tal cual en una sesión nueva de OpenCode
con el modelo asignado. Todos referencian `../PLAN.md` (el plan aprobado).

| Brief | Lane | Modelo | Zona exclusiva | Gate |
|---|---|---|---|---|
| `A.md` | Motor trading Rust | deepseek-v4.1-flash-free | `crates/trading/**` | Contratos congelados |
| `B.md` | UI dashboard GPUI | glm-5.3-flash-free ×2 | `crates/ui/src/trading/**` | Contratos congelados |
| `C.md` | Anillo WARP | big-pickle | `crates/warp/**` | Fase 0 verde |
| `D.md` | Cliente TWS Rust | deepseek-v4.1-flash-free (sesión 2) | `crates/tws/**` | Fase 0 verde |
| `F.md` | Especificador | longcat-2.5-free | `SPECS/**` | (ya ejecutándose por orquestador) |

Reglas globales (válidas en todas las lanes):
1. Dentro de tu lane: secuencial. Entre lanes: paralelo. Nunca edites archivos fuera de tu zona.
2. Los commits los hace el orquestador (GLM-5.3 Flash). Tú dejas el working tree listo y reportas.
3. El código que toca dinero (órdenes, cierres, claims) sigue los specs de `../SPECS/` al pie de la letra.
4. Si el toolchain (`cargo`) no está aún disponible, continúa con los edits y verifica en cuanto aparezca.
5. Git: `C:\Users\WDAGUtilityAccount\tools\mingit\cmd\git.exe` (PATH absoluto — MinGit portable).
