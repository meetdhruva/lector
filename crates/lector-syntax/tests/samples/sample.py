#!/usr/bin/env python3
"""Module docstring."""
import os
from dataclasses import dataclass

MAX_SIZE = 100


@dataclass
class Point:
    x: float
    y: float = 0.0

    def norm(self) -> float:
        return (self.x ** 2 + self.y ** 2) ** 0.5

    @staticmethod
    def origin() -> "Point":
        return Point(0, 0)


def read_all(path: str, *, binary: bool = False) -> list[str]:
    with open(path) as f:
        for line in f:
            if not line.strip() or line.startswith("#"):
                continue
            print(f"line: {line!r}")
    return [os.path.basename(path), None, True]
