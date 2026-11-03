package main

import (
	"fmt"
	"strings"
)

const Pi = 3.14

type Shape interface {
	Area() float64
}

type Rect struct {
	W, H float64
}

func (r Rect) Area() float64 {
	return r.W * r.H
}

type Celsius float64

var names = []string{"a", "b"}

func main() {
	var s Shape = Rect{W: 2, H: 3}
	for i, n := range names {
		fmt.Println(i, strings.ToUpper(n), s.Area())
	}
	go func() { defer fmt.Println("done") }()
}
