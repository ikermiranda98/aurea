"""The MT5 side of the bots translator (lane A3).

Ported 1:1 from Openbot's `PUENTE_PY`/`TERMINAL_ABIERTO_PY` (`mt5.ts:235-715`).
One process per link, spawned by the Rust side with `python -c` or as this
file; it reads one JSON line per command on stdin (`{"id","verb","arg"}`) and
writes JSON lines on stdout:

* `resultado` — the answer to one command (`ok`, `retcode`, `message`, `data`).
* `estado`    — the link is not attached (`conectado: false`, `motivo`).
* `cuenta`    — the account, every 5 s (balance, equity, P/L, positions, closes).
* `bot`       — the bots' book, every 0.5 s: positions, pending orders and
                three days of deals (deals only when the book signature
                changes or at least every 2 s).

Order verbs and arguments (the DeepTrading bridge protocol):

    SYMBOL name
    RATES symbol;tf;fromUtc;toUtc
    DEAL symbol;buy|sell;volume;magic;comment
    PENDING symbol;buy|sell;stop|limit;volume;price;magic;comment;day(1|0)
    CLOSEVOL ticket;volume
    SLTP ticket;sl;tp
    DELETE ticket

Every order verb refuses to send anything when the terminal is logged into
another account (-3) or has Algo Trading off (-10). The terminal is never
launched by this process: it attaches only while `terminal64.exe` is running
and reports `estado` otherwise.
"""

import datetime, json, math, os, queue, subprocess, sys, threading, time

ARGS = sys.argv[1:]
PROBE = "--probe" in ARGS
RELOJ = ("BTCUSD", "EURUSD", "XAUUSD")


def argumento(nombre):
    if nombre in ARGS:
        i = ARGS.index(nombre)
        if i + 1 < len(ARGS):
            return ARGS[i + 1]
    return None


# The link's own account: trading commands only run while the terminal is logged in to it.
LOGIN = argumento("--login")
SERVIDOR = argumento("--server")
FICHERO_DESFASE = argumento("--desfase")
SALIDA = threading.Lock()


def emitir(objeto):
    texto = json.dumps(objeto, ensure_ascii=True, separators=(",", ":"))
    with SALIDA:
        sys.stdout.write(texto + "\n")
        sys.stdout.flush()


def n(valor):
    try:
        f = float(valor)
    except Exception:
        return None
    return f if math.isfinite(f) else None


def estado(motivo):
    emitir({"type": "estado", "conectado": False, "motivo": motivo})


def terminal_abierto():
    try:
        hecho = subprocess.run(["tasklist", "/FI", "IMAGENAME eq terminal64.exe", "/NH"], capture_output=True, text=True, creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0))
    except Exception:
        return None
    if hecho.returncode != 0:
        return None
    return "terminal64.exe" in hecho.stdout.lower()


def desfase(mt5, simbolos):
    ultimo = 0
    for simbolo in simbolos:
        try:
            tick = mt5.symbol_info_tick(simbolo)
        except Exception:
            tick = None
        if tick is not None and tick.time > ultimo:
            ultimo = tick.time
    if ultimo == 0:
        return 0
    bruto = ultimo - time.time()
    if abs(bruto) > 15 * 3600:
        return 0
    return int(round(bruto / 1800.0) * 1800)


class Reloj:
    """Broker server time minus UTC, in seconds. A fresh tick (within two minutes of a
    quarter-hour step) states it; otherwise the last known one stays, remembered on disk
    so a weekend (no fresh ticks anywhere) still reads the right hours."""

    def __init__(self):
        self.offset = None
        self.revisado = 0.0
        if FICHERO_DESFASE:
            try:
                with open(FICHERO_DESFASE, encoding="utf-8") as fh:
                    self.offset = int(json.load(fh).get("offset", 0))
            except Exception:
                pass

    def valor(self, mt5, simbolos):
        ahora = time.time()
        if self.offset is not None and ahora - self.revisado < 30:
            return self.offset
        self.revisado = ahora
        nombres = list(simbolos) + list(RELOJ)
        try:
            for s in mt5.symbols_get() or ():
                if getattr(s, "visible", False):
                    nombres.append(s.name)
        except Exception:
            pass
        vistos = set()
        mejor = None
        for nombre in nombres:
            if nombre in vistos or len(vistos) > 40:
                continue
            vistos.add(nombre)
            try:
                tick = mt5.symbol_info_tick(nombre)
            except Exception:
                tick = None
            if tick is None or not tick.time:
                continue
            bruto = tick.time - ahora
            redondo = round(bruto / 900.0) * 900
            if abs(bruto - redondo) < 120 and abs(redondo) <= 15 * 3600:
                mejor = int(redondo)
                break
        if mejor is None:
            if self.offset is None:
                self.offset = desfase(mt5, simbolos)
            return self.offset
        if mejor != self.offset and FICHERO_DESFASE:
            try:
                os.makedirs(os.path.dirname(FICHERO_DESFASE), exist_ok=True)
                with open(FICHERO_DESFASE, "w", encoding="utf-8") as fh:
                    json.dump({"offset": mejor}, fh)
            except Exception:
                pass
        self.offset = mejor
        return mejor


RELOJ_SERVIDOR = Reloj()


def linea(mt5, iniciar):
    if iniciar and not mt5.initialize():
        return "el terminal de MetaTrader 5 no responde"
    cuenta = mt5.account_info()
    if cuenta is None:
        return "el terminal no est\u00e1 conectado a una cuenta"
    abiertas = mt5.positions_get() or ()
    off = RELOJ_SERVIDOR.valor(mt5, [p.symbol for p in abiertas])
    medianoche = datetime.datetime.combine(datetime.date.today(), datetime.time.min).timestamp()
    realizado = None
    cierres = []
    try:
        deals = mt5.history_deals_get(int(medianoche + off), int(time.time() + off + 3600))
        if deals is not None:
            realizado = 0.0
            for d in deals:
                # Only trade deals count: deposits and corrections ride their own deal
                # types and would book the initial deposit as the day's P/L.
                if d.type not in (mt5.DEAL_TYPE_BUY, mt5.DEAL_TYPE_SELL):
                    continue
                total = d.profit + d.commission + d.swap
                realizado += total
                if d.entry in (mt5.DEAL_ENTRY_OUT, mt5.DEAL_ENTRY_INOUT, mt5.DEAL_ENTRY_OUT_BY):
                    cierres.append({"symbol": d.symbol, "ts": int((d.time - off) * 1000), "volume": n(d.volume), "price": n(d.price), "pnl": n(total)})
    except Exception:
        realizado = None
    realizado = n(realizado)
    flotante = n(cuenta.profit)
    pnl = realizado
    if realizado is None:
        pnl = flotante
    elif flotante is not None:
        pnl = realizado + flotante
    posiciones = [{
        "symbol": p.symbol,
        "side": "long" if p.type == mt5.POSITION_TYPE_BUY else "short",
        "size": n(p.volume),
        "entry": n(p.price_open),
        "current": n(p.price_current),
        "pnl": n(p.profit + p.swap),
    } for p in abiertas]
    emitir({
        "type": "cuenta",
        "login": cuenta.login,
        "name": cuenta.name,
        "server": cuenta.server,
        "trade_mode": cuenta.trade_mode,
        "balance": n(cuenta.balance),
        "equity": n(cuenta.equity),
        "profit": flotante,
        "margin_free": n(cuenta.margin_free),
        "margin_used": n(cuenta.margin),
        "pnl_day": pnl,
        "pnl_realized": realizado,
        "positions": posiciones,
        "closes": cierres,
    })


def ciclo(mt5, iniciar):
    motivo = linea(mt5, iniciar)
    if motivo is not None:
        estado(motivo)
        return False
    return True


# ----------------------------------------------------------------------------- the bots' side
# The bots' orders (OpenBot's ib_async stand-in) arrive here as one JSON line per command on
# stdin: {"id", "verb", "arg"}; each one is answered with a "resultado" line. Verbs and
# arguments are those of the DeepTrading bridge protocol:
#   SYMBOL name, RATES symbol;tf;fromUtc;toUtc, DEAL symbol;buy|sell;volume;magic;comment,
#   PENDING symbol;buy|sell;stop|limit;volume;price;magic;comment;day,
#   CLOSEVOL ticket;volume, SLTP ticket;sl;tp, DELETE ticket
# Besides, a "bot" line carries the account, every position (ticket, magic, SL, TP), the
# pending orders and three days of deals (order, entry, reason) for the engine's translator.

OK_ORDEN = (10008, 10009, 10010)  # placed, done, done partial
DESVIO = 20
COLA = queue.Queue()
ORDENES = ("DEAL", "PENDING", "CLOSEVOL", "SLTP", "DELETE")


def leer_entrada():
    for texto in sys.stdin:
        texto = texto.strip().lstrip("\ufeff")
        if texto:
            COLA.put(texto)


def respuesta(cid, ok, retcode, mensaje, datos=None):
    return {"type": "resultado", "id": cid, "ok": bool(ok), "retcode": int(retcode), "message": str(mensaje), "data": datos}


def relleno(mt5, simbolo):
    info = mt5.symbol_info(simbolo)
    modos = getattr(info, "filling_mode", 0) or 0
    if modos & 1:
        return mt5.ORDER_FILLING_FOK
    if modos & 2:
        return mt5.ORDER_FILLING_IOC
    return mt5.ORDER_FILLING_RETURN


def mandar(mt5, peticion):
    hecho = mt5.order_send(peticion)
    if hecho is None:
        codigo, texto = mt5.last_error()
        return False, int(codigo), str(texto), None
    return hecho.retcode in OK_ORDEN, int(hecho.retcode), str(hecho.comment), hecho


MARCOS = {}


SEGUNDOS_MARCO = {"M": 60, "H": 3600, "D": 86400, "W": 604800}


def marco(mt5, nombre):
    if not MARCOS:
        for clave in ("M1", "M2", "M3", "M5", "M10", "M15", "M20", "M30", "H1", "H2", "H3", "H4", "H6", "H8", "H12", "D1", "W1"):
            valor = getattr(mt5, "TIMEFRAME_" + clave, None)
            if valor is not None:
                MARCOS[clave] = valor
    return MARCOS.get(nombre)


def ejecutar(mt5, verbo, cid, arg):
    a = arg.split(";")
    off = RELOJ_SERVIDOR.valor(mt5, [])
    if verbo in ORDENES:
        cuenta = mt5.account_info()
        if cuenta is None:
            return respuesta(cid, False, -1, "el terminal no est\u00e1 conectado a una cuenta")
        if LOGIN is None or str(cuenta.login) != LOGIN or (SERVIDOR is not None and cuenta.server != SERVIDOR):
            return respuesta(cid, False, -3, "el terminal est\u00e1 en la cuenta " + str(cuenta.login) + " (" + str(cuenta.server) + "), no en la del enlace: no se envi\u00f3 nada")
        terminal = mt5.terminal_info()
        if terminal is not None and not terminal.trade_allowed:
            return respuesta(cid, False, -10, "MetaTrader 5 tiene desactivado el trading algor\u00edtmico (bot\u00f3n Algo Trading)")

    if verbo == "SYMBOL":
        if not mt5.symbol_select(arg, True):
            return respuesta(cid, False, 4301, "no existe el s\u00edmbolo " + arg)
        s = mt5.symbol_info(arg)
        if s is None:
            return respuesta(cid, False, 4301, "no existe el s\u00edmbolo " + arg)
        return respuesta(cid, True, 0, "", {"symbol": s.name, "digits": s.digits, "contract_size": n(s.trade_contract_size),
                                           "tick_size": n(s.trade_tick_size), "tick_value": n(s.trade_tick_value),
                                           "volume_min": n(s.volume_min), "volume_step": n(s.volume_step), "volume_max": n(s.volume_max),
                                           "currency_profit": s.currency_profit, "bid": n(s.bid), "ask": n(s.ask),
                                           "trade_mode": s.trade_mode})

    if verbo == "RATES":
        if len(a) < 4:
            return respuesta(cid, False, -1, "RATES necesita s\u00edmbolo;tf;desde;hasta")
        tf = marco(mt5, a[1])
        if tf is None:
            return respuesta(cid, False, -1, "temporalidad no soportada: " + a[1])
        mt5.symbol_select(a[0], True)
        desde, hasta = int(float(a[2])) + off, int(float(a[3])) + off
        velas = mt5.copy_rates_range(a[0], tf, desde, hasta)
        # After a gap (the terminal was closed) the first ask returns only what the
        # terminal kept and starts the download: wait a little for the fresh bars.
        segundos = SEGUNDOS_MARCO.get(a[1][:1], 0) * int(a[1][1:] or 1)
        tick = mt5.symbol_info_tick(a[0])
        if segundos and tick is not None and tick.time and hasta >= tick.time - segundos:
            for _ in range(10):
                if velas is not None and len(velas) and int(velas[-1]["time"]) >= tick.time - 2 * segundos:
                    break
                time.sleep(0.5)
                velas = mt5.copy_rates_range(a[0], tf, desde, hasta)
        if velas is None:
            codigo, texto = mt5.last_error()
            return respuesta(cid, False, int(codigo), "MT5 no devolvi\u00f3 velas de " + a[0] + " (" + str(texto) + ")", [])
        filas = [[(int(r["time"]) - off) * 1000, float(r["open"]), float(r["high"]), float(r["low"]), float(r["close"]),
                  int(r["tick_volume"])] for r in velas]
        return respuesta(cid, True, 0, "", filas)

    if verbo == "DEAL":
        if len(a) < 5:
            return respuesta(cid, False, -1, "DEAL necesita s\u00edmbolo;lado;volumen;magic;comentario")
        simbolo, compra = a[0], a[1] == "buy"
        mt5.symbol_select(simbolo, True)
        tick = mt5.symbol_info_tick(simbolo)
        if tick is None:
            return respuesta(cid, False, 10021, "sin precios de " + simbolo)
        ok, codigo, texto, hecho = mandar(mt5, {"action": mt5.TRADE_ACTION_DEAL, "symbol": simbolo, "volume": float(a[2]),
                                                "type": mt5.ORDER_TYPE_BUY if compra else mt5.ORDER_TYPE_SELL,
                                                "price": tick.ask if compra else tick.bid, "deviation": DESVIO, "magic": int(a[3]),
                                                "comment": a[4][:31], "type_time": mt5.ORDER_TIME_GTC, "type_filling": relleno(mt5, simbolo)})
        return respuesta(cid, ok, codigo, texto, {"order": hecho.order if hecho else 0, "deal": hecho.deal if hecho else 0,
                                                  "price": n(hecho.price) if hecho else 0.0, "volume": n(hecho.volume) if hecho else 0.0})

    if verbo == "PENDING":
        if len(a) < 8:
            return respuesta(cid, False, -1, "PENDING necesita 8 campos")
        simbolo, compra, stop = a[0], a[1] == "buy", a[2] == "stop"
        tipo = (mt5.ORDER_TYPE_BUY_STOP if compra else mt5.ORDER_TYPE_SELL_STOP) if stop else \
            (mt5.ORDER_TYPE_BUY_LIMIT if compra else mt5.ORDER_TYPE_SELL_LIMIT)
        mt5.symbol_select(simbolo, True)
        ok, codigo, texto, hecho = mandar(mt5, {"action": mt5.TRADE_ACTION_PENDING, "symbol": simbolo, "volume": float(a[3]), "type": tipo,
                                                "price": float(a[4]), "magic": int(a[5]), "comment": a[6][:31],
                                                "type_time": mt5.ORDER_TIME_DAY if a[7] == "1" else mt5.ORDER_TIME_GTC,
                                                "type_filling": mt5.ORDER_FILLING_RETURN})
        return respuesta(cid, ok, codigo, texto, {"order": hecho.order if hecho else 0})

    if verbo == "CLOSEVOL":
        ticket = int(a[0])
        halladas = mt5.positions_get(ticket=ticket)
        if not halladas:
            return respuesta(cid, False, 10036, "la posici\u00f3n ya no est\u00e1 abierta")
        p = halladas[0]
        volumen = p.volume if len(a) < 2 else min(float(a[1]), p.volume)
        tick = mt5.symbol_info_tick(p.symbol)
        if tick is None:
            return respuesta(cid, False, 10021, "sin precios de " + p.symbol)
        vende = p.type == mt5.POSITION_TYPE_BUY
        ok, codigo, texto, hecho = mandar(mt5, {"action": mt5.TRADE_ACTION_DEAL, "symbol": p.symbol, "volume": volumen, "position": ticket,
                                                "type": mt5.ORDER_TYPE_SELL if vende else mt5.ORDER_TYPE_BUY,
                                                "price": tick.bid if vende else tick.ask, "deviation": DESVIO, "magic": p.magic,
                                                "type_time": mt5.ORDER_TIME_GTC, "type_filling": relleno(mt5, p.symbol)})
        return respuesta(cid, ok, codigo, texto, {"deal": hecho.deal if hecho else 0, "price": n(hecho.price) if hecho else 0.0,
                                                  "volume": n(hecho.volume) if hecho else 0.0})

    if verbo == "SLTP":
        if len(a) < 3:
            return respuesta(cid, False, -1, "SLTP necesita ticket;sl;tp")
        ticket = int(a[0])
        halladas = mt5.positions_get(ticket=ticket)
        if not halladas:
            return respuesta(cid, False, 10036, "la posici\u00f3n ya no est\u00e1 abierta")
        ok, codigo, texto, _ = mandar(mt5, {"action": mt5.TRADE_ACTION_SLTP, "symbol": halladas[0].symbol, "position": ticket,
                                            "sl": float(a[1]), "tp": float(a[2])})
        return respuesta(cid, ok, codigo, texto)

    if verbo == "DELETE":
        ticket = int(arg)
        if not mt5.orders_get(ticket=ticket):
            return respuesta(cid, False, 10036, "la orden ya no est\u00e1 pendiente")
        ok, codigo, texto, _ = mandar(mt5, {"action": mt5.TRADE_ACTION_REMOVE, "order": ticket})
        return respuesta(cid, ok, codigo, texto)

    return respuesta(cid, False, -1, "orden desconocida: " + verbo)


TIPOS_PENDIENTE = {}
FIRMA = {"libro": None}


def linea_bots(mt5, con_operaciones):
    cuenta = mt5.account_info()
    if cuenta is None:
        return
    if not TIPOS_PENDIENTE:
        TIPOS_PENDIENTE.update({mt5.ORDER_TYPE_BUY_STOP: "buy_stop", mt5.ORDER_TYPE_SELL_STOP: "sell_stop",
                                mt5.ORDER_TYPE_BUY_LIMIT: "buy_limit", mt5.ORDER_TYPE_SELL_LIMIT: "sell_limit"})
    abiertas = mt5.positions_get() or ()
    off = RELOJ_SERVIDOR.valor(mt5, [p.symbol for p in abiertas])
    posiciones = [{"ticket": p.ticket, "symbol": p.symbol, "side": "short" if p.type == mt5.POSITION_TYPE_SELL else "long",
                   "volume": n(p.volume) or 0.0, "entry": n(p.price_open) or 0.0, "current": n(p.price_current) or 0.0,
                   "sl": n(p.sl) or 0.0, "tp": n(p.tp) or 0.0, "magic": int(p.magic), "time": int(p.time_msc) - off * 1000}
                  for p in abiertas]
    pendientes = [{"ticket": o.ticket, "symbol": o.symbol, "kind": TIPOS_PENDIENTE.get(o.type, "other"),
                   "volume": n(o.volume_current) or 0.0, "price": n(o.price_open) or 0.0, "magic": int(o.magic)}
                  for o in (mt5.orders_get() or ())]
    salida = {"type": "bot", "login": cuenta.login, "server": cuenta.server, "demo": cuenta.trade_mode != 2,
              "currency": cuenta.currency, "leverage": int(cuenta.leverage or 0), "balance": n(cuenta.balance),
              "equity": n(cuenta.equity), "margin": n(cuenta.margin), "margin_free": n(cuenta.margin_free),
              "profit": n(cuenta.profit), "positions": posiciones, "orders": pendientes}
    # A position or pending order that comes or goes rides with the deals that explain it, so
    # the translator never sees a stop's position gone before the stop's own deal.
    firma = (tuple(sorted(p["ticket"] for p in posiciones)), tuple(sorted(o["ticket"] for o in pendientes)))
    if firma != FIRMA["libro"]:
        FIRMA["libro"] = firma
        con_operaciones = True
    if con_operaciones:
        ahora = time.time() + off
        inicio = ahora - ahora % 86400 - 3 * 86400
        entradas = {mt5.DEAL_ENTRY_IN: "in", mt5.DEAL_ENTRY_OUT: "out", mt5.DEAL_ENTRY_INOUT: "inout"}
        motivos = {mt5.DEAL_REASON_SL: "sl", mt5.DEAL_REASON_TP: "tp", mt5.DEAL_REASON_SO: "so"}
        operaciones = []
        for d in mt5.history_deals_get(int(inicio), int(ahora + 86400)) or ():
            if d.type not in (mt5.DEAL_TYPE_BUY, mt5.DEAL_TYPE_SELL):
                continue
            operaciones.append({"deal": d.ticket, "order": d.order, "position": d.position_id, "symbol": d.symbol,
                                "side": "short" if d.type == mt5.DEAL_TYPE_SELL else "long", "volume": n(d.volume) or 0.0,
                                "price": n(d.price) or 0.0, "pnl": n(d.profit + d.commission + d.swap) or 0.0, "magic": int(d.magic),
                                "time": int(d.time_msc) - off * 1000, "entry": entradas.get(d.entry, "out_by"),
                                "reason": motivos.get(d.reason, "other")})
        salida["deals"] = operaciones
    emitir(salida)


def atender(mt5, enganchado):
    hubo = False
    while True:
        try:
            texto = COLA.get_nowait()
        except queue.Empty:
            return hubo
        try:
            pedido = json.loads(texto)
            cid = str(pedido.get("id", ""))
            verbo = str(pedido.get("verb", ""))
            arg = str(pedido.get("arg", ""))
        except Exception:
            continue
        if not enganchado:
            emitir(respuesta(cid, False, -1, "el terminal de MetaTrader 5 no est\u00e1 enganchado"))
            continue
        try:
            emitir(ejecutar(mt5, verbo, cid, arg))
        except Exception as causa:
            emitir(respuesta(cid, False, -1, "error del puente: " + str(causa)))
        hubo = hubo or verbo in ORDENES


def vigilar():
    try:
        import MetaTrader5 as mt5
    except Exception as causa:
        estado("falta el paquete de Python MetaTrader5: " + str(causa))
        return
    if PROBE:
        ciclo(mt5, True)
        time.sleep(5)
        ciclo(mt5, True)
        return
    # A stand-in package for tests (no terminal process to look for) says so itself.
    falso = bool(getattr(mt5, "__openbot_falso__", False))
    threading.Thread(target=leer_entrada, daemon=True).start()
    enganchado = False
    ultimo_intento = ultima_cuenta = ultima_bot = ultimas_ops = -1e9
    while True:
        ahora = time.monotonic()
        if not enganchado and ahora - ultimo_intento >= 5:
            ultimo_intento = ahora
            abierto = True if falso else terminal_abierto()
            if abierto is None:
                estado("no puedo comprobar si el terminal de MetaTrader 5 est\u00e1 abierto (falla la lista de procesos): no me engancho para no abrirlo")
            elif not abierto:
                estado("el terminal de MetaTrader 5 est\u00e1 cerrado")
            else:
                enganchado = ciclo(mt5, True)
                ultima_cuenta = ahora
                if not enganchado:
                    try:
                        mt5.shutdown()
                    except Exception:
                        pass
        if atender(mt5, enganchado):
            ultima_bot = ultimas_ops = -1e9  # report a command's effect right away
        if enganchado:
            try:
                if ahora - ultima_cuenta >= 5:
                    ultima_cuenta = ahora
                    enganchado = ciclo(mt5, False)
                if enganchado and LOGIN is not None and ahora - ultima_bot >= 0.5:
                    ultima_bot = ahora
                    operaciones = ahora - ultimas_ops >= 2
                    if operaciones:
                        ultimas_ops = ahora
                    linea_bots(mt5, operaciones)
            except Exception as causa:
                estado("lectura fallida: " + str(causa))
                enganchado = False
        time.sleep(0.05)


vigilar()
