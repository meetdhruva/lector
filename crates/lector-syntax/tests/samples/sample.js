#!/usr/bin/env node
'use strict';
const path = require('path');

/** Adds numbers. */
function add(a, b = 2) {
  return a + b;
}

class Animal {
  static count = 0;
  #secret = 1;
  constructor(name) {
    this.name = name;
  }
  speak() {
    console.log(`${this.name} makes a noise`);
  }
}

const square = (x) => x * x;
const items = [1, 2, 3].map(square).filter(Boolean);
const view = <div className="x">hello</div>;
module.exports = { add, Animal, items, re: /a+b/g, path };
