import { readFile } from "node:fs/promises";
import type { Options } from "./options";

export interface Greeter {
  name: string;
  greet(times?: number): string;
}

export type Id = string | number;

enum Color {
  Red = "RED",
  Green = "GREEN",
}

const MAX_RETRIES = 3;

export abstract class Base<T> implements Greeter {
  private readonly cache = new Map<string, T>();
  constructor(public name: string) {}

  greet(times = 1): string {
    return `Hello, ${this.name}`.repeat(times);
  }

  abstract load(id: Id): Promise<T>;
}

namespace Util {
  export function clamp(n: number, lo: number, hi: number): number {
    return Math.min(Math.max(n, lo), hi);
  }
}

export const double = (x: number): number => x * 2;

async function main(opts: Options) {
  const text = await readFile("a.txt", "utf8");
  if (text.length > MAX_RETRIES && /ab+c/.test(text)) {
    console.log(double(2), Color.Red, null, undefined);
  }
}
