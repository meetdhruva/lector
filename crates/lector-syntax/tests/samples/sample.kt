package com.example

import kotlin.math.max

const val LIMIT = 10

interface Shape {
    fun area(): Double
}

data class Circle(val radius: Double) : Shape {
    override fun area(): Double = Math.PI * radius * radius

    companion object {
        fun unit() = Circle(1.0)
    }
}

enum class Color { RED, GREEN }

object Registry {
    val items = mutableListOf<String>()
}

fun main(args: Array<String>) {
    val shapes: List<Shape> = listOf(Circle(2.0))
    for (s in shapes) {
        println("area = ${s.area()}")
    }
    val n = max(1, 2) // comment
    when (n) {
        1 -> println("one")
        else -> println(null)
    }
}
