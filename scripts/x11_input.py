#!/usr/bin/env python3
"""Synthesizes X11 input with XTest, for scripted headless screenshots.

Usage: x11_input.py DISPLAY "CMD; CMD; ..."

Commands:
  key ctrl+shift+p      press a chord (modifiers: ctrl, shift, alt, super)
  type some text        type characters (ASCII)
  click X Y [BUTTON]    move and click (1 left, 2 middle, 3 right); BUTTON may be 'double'
  move X Y              move the pointer
  scroll N              wheel N notches (negative = up)
  sleep SECONDS         wait
"""
import ctypes
import ctypes.util
import sys
import time

display_name = sys.argv[1].encode()
script = sys.argv[2] if len(sys.argv) > 2 else ""

x11 = ctypes.cdll.LoadLibrary(ctypes.util.find_library("X11"))
xtst = ctypes.cdll.LoadLibrary(ctypes.util.find_library("Xtst"))
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XStringToKeysym.restype = ctypes.c_ulong
x11.XStringToKeysym.argtypes = [ctypes.c_char_p]
x11.XKeysymToKeycode.restype = ctypes.c_ubyte
x11.XKeysymToKeycode.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x11.XFlush.argtypes = [ctypes.c_void_p]
xtst.XTestFakeKeyEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeButtonEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint, ctypes.c_int, ctypes.c_ulong]
xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_ulong]

d = x11.XOpenDisplay(display_name)
if not d:
    sys.exit("cannot open display")

MODS = {"ctrl": "Control_L", "shift": "Shift_L", "alt": "Alt_L", "super": "Super_L"}
NAMES = {
    "enter": "Return", "esc": "Escape", "escape": "Escape", "tab": "Tab", "space": "space",
    "up": "Up", "down": "Down", "left": "Left", "right": "Right", "home": "Home", "end": "End",
    "pageup": "Prior", "pagedown": "Next", "backspace": "BackSpace", "-": "minus", "=": "equal",
    ",": "comma", ".": "period", "/": "slash", ":": "colon", "\\": "backslash", "[": "bracketleft",
    "]": "bracketright",
}
SHIFTED = {":": "colon", "_": "underscore", "?": "question", "!": "exclam", "+": "plus"}


def keycode(name):
    sym = x11.XStringToKeysym(NAMES.get(name, name).encode())
    return x11.XKeysymToKeycode(d, sym)


def tap(code, mods=()):
    for m in mods:
        xtst.XTestFakeKeyEvent(d, keycode(MODS[m]), 1, 0)
    xtst.XTestFakeKeyEvent(d, code, 1, 0)
    xtst.XTestFakeKeyEvent(d, code, 0, 0)
    for m in reversed(mods):
        xtst.XTestFakeKeyEvent(d, keycode(MODS[m]), 0, 0)
    x11.XFlush(d)
    time.sleep(0.03)


for raw in script.split(";"):
    parts = raw.strip().split(" ", 1)
    if not parts[0]:
        continue
    cmd, arg = parts[0], (parts[1] if len(parts) > 1 else "")
    if cmd == "key":
        keys = arg.split("+")
        tap(keycode(keys[-1]), keys[:-1])
    elif cmd == "type":
        for ch in arg:
            if ch == " ":
                tap(keycode("space"))
            elif ch in SHIFTED:
                tap(x11.XKeysymToKeycode(d, x11.XStringToKeysym(SHIFTED[ch].encode())), ["shift"])
            elif ch.isupper():
                tap(keycode(ch.lower()), ["shift"])
            else:
                tap(keycode(ch))
    elif cmd in ("click", "move"):
        a = arg.split()
        x, y = int(a[0]), int(a[1])
        xtst.XTestFakeMotionEvent(d, -1, x, y, 0)
        x11.XFlush(d)
        time.sleep(0.05)
        if cmd == "click":
            spec = a[2] if len(a) > 2 else "1"
            times = 2 if spec == "double" else 1
            button = 1 if spec == "double" else int(spec)
            for _ in range(times):
                xtst.XTestFakeButtonEvent(d, button, 1, 0)
                xtst.XTestFakeButtonEvent(d, button, 0, 0)
                x11.XFlush(d)
                time.sleep(0.05)
    elif cmd == "scroll":
        n = int(arg)
        button = 5 if n > 0 else 4
        for _ in range(abs(n)):
            xtst.XTestFakeButtonEvent(d, button, 1, 0)
            xtst.XTestFakeButtonEvent(d, button, 0, 0)
            x11.XFlush(d)
            time.sleep(0.02)
    elif cmd == "sleep":
        time.sleep(float(arg))
    x11.XFlush(d)
