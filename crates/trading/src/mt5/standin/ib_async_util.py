"""The parts of 'ib_async.util' a bot may call."""
import logging
import time


def sleep(secs: float = 0.02) -> bool:
    time.sleep(secs)
    return True


def df(objs, labels=None):
    import pandas as pd

    if not objs:
        return None
    first = objs[0]
    if hasattr(first, "dict"):
        rows = [o.dict() for o in objs]
    elif hasattr(first, "_asdict"):
        rows = [o._asdict() for o in objs]
    else:
        rows = list(objs)
    frame = pd.DataFrame(rows)
    if labels:
        frame = frame[[c for c in labels if c in frame.columns]]
    return frame


def startLoop():
    pass


def patchAsyncio():
    pass


def useQt(*args, **kwargs):
    pass


def logToConsole(level=logging.INFO):
    logging.basicConfig(level=level)


def logToFile(path, level=logging.INFO):
    logging.basicConfig(filename=path, level=level)
