#!/usr/bin/env python3
"""Generates X11 input on a headless Xvfb display so GPUI's event loop wakes up.

GPUI (0.2.x, X11) drains the X connection only when its fd becomes readable;
events that arrive while the window is being created (MapNotify, Expose) can
sit in xcb's queue until the next event. On a real desktop the pointer or the
window manager produces that event; under Xvfb nothing does, so the window
never starts its refresh loop and stays black. Moving the pointer fixes that.

Usage: x11_poke.py [DISPLAY] [x y]
"""
import ctypes
import ctypes.util
import sys

display = (sys.argv[1] if len(sys.argv) > 1 else ":99").encode()
x = int(sys.argv[2]) if len(sys.argv) > 3 else 800
y = int(sys.argv[3]) if len(sys.argv) > 3 else 500

lib = ctypes.cdll.LoadLibrary(ctypes.util.find_library("X11"))
lib.XOpenDisplay.restype = ctypes.c_void_p
lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
lib.XDefaultRootWindow.restype = ctypes.c_ulong
lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
lib.XWarpPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_int,
                             ctypes.c_int, ctypes.c_uint, ctypes.c_uint, ctypes.c_int, ctypes.c_int]
lib.XFlush.argtypes = [ctypes.c_void_p]
lib.XCloseDisplay.argtypes = [ctypes.c_void_p]

d = lib.XOpenDisplay(display)
if not d:
    sys.exit(f"cannot open display {display.decode()}")
root = lib.XDefaultRootWindow(d)
for dx in (0, 1, 0):
    lib.XWarpPointer(d, 0, root, 0, 0, 0, 0, x + dx, y)
    lib.XFlush(d)
lib.XCloseDisplay(d)
